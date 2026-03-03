use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use std::{env, process::Command};
use chrono::Local;
use wl_clipboard_rs::copy::{MimeSource, MimeType as CopyMimeType, Options, Source};
use wl_clipboard_rs::paste::{
    get_contents, ClipboardType, Error as PasteError, MimeType as PasteMimeType, Seat,
};

use crate::error::{Generify, MyError, MyResult, Standardize};
use crate::log;

// ---------------------------------------------------------------------------
// Thread-safe guard for setting display environment variables
// ---------------------------------------------------------------------------

/// Serialises all `env::set_var("WAYLAND_DISPLAY" / "DISPLAY", …)` calls so
/// they are never concurrent (env is process-global and modification is unsafe
/// in a multi-threaded context).
static DISPLAY_ENV_MUTEX: Mutex<()> = Mutex::new(());

fn lock_display_env() -> MutexGuard<'static, ()> {
    DISPLAY_ENV_MUTEX
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// MIME-type constants
// ---------------------------------------------------------------------------

/// Ordered list of MIME types we probe when reading a Wayland clipboard.
/// Binary types (images) are listed first so rich content is preserved.
pub const WAYLAND_PROBE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
    "text/html",
    "text/uri-list",
    "text/plain;charset=utf-8",
    "text/plain",
];

/// Ordered list of X11 selection targets we probe.
pub const X11_PROBE_TARGETS: &[&str] = &[
    "image/png",
    "image/jpeg",
    "text/html",
    "text/uri-list",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// Returns `true` for MIME types / X11 atoms that carry human-readable text.
pub fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(mime, "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT")
}

// ---------------------------------------------------------------------------
// MIME-type discovery helpers
// ---------------------------------------------------------------------------

/// Cap on how many MIME types we will fetch per clipboard read, to avoid
/// unbounded network / IPC overhead from unusual compositors.
const MAX_MIME_TYPES: usize = 64;

/// Discover all MIME types currently offered by the clipboard on `display`
/// via `wl-paste --list-types`. Returns an empty list if the clipboard is
/// empty or wl-paste is not installed.
fn discover_wayland_mime_types(display: &str) -> Vec<String> {
    let out = Command::new("wl-paste")
        .arg("--list-types")
        .env("WAYLAND_DISPLAY", display)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(MAX_MIME_TYPES)
            .map(str::to_string)
            .collect(),
        _ => vec![],
    }
}

/// Sort `types` so entries that appear in `priority_list` come first (in that
/// order), with any unknown types preserved afterwards in their original order.
fn sort_by_mime_priority(types: &mut Vec<String>, priority_list: &[&str]) {
    // Stable sort: equal keys preserve relative order.
    types.sort_by_key(|m| {
        priority_list
            .iter()
            .position(|&p| p == m.as_str())
            .unwrap_or(priority_list.len())
    });
}

/// Returns `true` for Wayland MIME types that are application-internal
/// bookkeeping entries and should not be synced to other displays.
fn is_wayland_internal_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/x-kde-cutselection"
            | "x-special/gnome-copied-files"
            | "x-special/nautilus-clipboard"
            | "x-kde-nativedata"
    ) || mime.starts_with("application/x-qt-image")
        || mime.starts_with("application/x-kde")
}

/// Returns `true` for X11 target atom names that represent meta/control
/// atoms and should never be fetched or propagated as clipboard content.
fn is_x11_meta_target(name: &str) -> bool {
    matches!(
        name,
        "TARGETS"
            | "MULTIPLE"
            | "TIMESTAMP"
            | "DELETE"
            | "INSERT_PROPERTY"
            | "INSERT_SELECTION"
            | "INCR"
            | "CLIPBOARD_MANAGER"
            | "SAVE_TARGETS"
            | "ATOM"
            | "ATOM_PAIR"
            | "INTEGER"
            | "PIXMAP"
            | "BITMAP"
            | "COLORMAP"
            | "DRAWABLE"
            | "WINDOW"
            | "PIXEL"
    ) || name.starts_with("_NET_")
        || name.starts_with("_WM_")
        || name.starts_with("_XSETTINGS")
        || name.starts_with("_MOTIF")
}

// ---------------------------------------------------------------------------
// ClipboardData — the multi-MIME clipboard payload
// ---------------------------------------------------------------------------

/// A single MIME-typed chunk of clipboard data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeEntry {
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// The full contents of a clipboard selection, possibly spanning several MIME
/// types (e.g. `text/html` + `text/plain` + `image/png` simultaneously).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClipboardData {
    /// Entries ordered from highest to lowest priority.
    pub entries: Vec<MimeEntry>,
}

impl ClipboardData {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Construct a plain-text payload offered under all common text targets.
    pub fn from_text(s: &str) -> Self {
        let bytes = s.as_bytes().to_vec();
        Self {
            entries: vec![
                MimeEntry {
                    mime_type: "text/plain;charset=utf-8".into(),
                    data: bytes.clone(),
                },
                MimeEntry {
                    mime_type: "text/plain".into(),
                    data: bytes.clone(),
                },
                MimeEntry {
                    mime_type: "UTF8_STRING".into(),
                    data: bytes.clone(),
                },
                MimeEntry {
                    mime_type: "STRING".into(),
                    data: bytes,
                },
            ],
        }
    }

    /// Retrieve raw bytes for a specific MIME type, if present.
    pub fn get_mime(&self, mime: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|e| e.mime_type == mime)
            .map(|e| e.data.as_slice())
    }

    /// Attempt to extract a UTF-8 string from any text entry, in priority order.
    pub fn text(&self) -> Option<String> {
        for preferred in &[
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
            "TEXT",
        ] {
            if let Some(bytes) = self.get_mime(preferred) {
                if let Ok(s) = std::str::from_utf8(bytes) {
                    let s = s.trim_end_matches('\0');
                    if !s.is_empty() {
                        return Some(s.to_owned());
                    }
                }
            }
        }
        // Fall back to any remaining text/* entry
        for entry in &self.entries {
            if is_text_mime(&entry.mime_type) {
                if let Ok(s) = std::str::from_utf8(&entry.data) {
                    let s = s.trim_end_matches('\0');
                    if !s.is_empty() {
                        return Some(s.to_owned());
                    }
                }
            }
        }
        None
    }

    /// The "canonical" bytes used for equality comparison and change detection.
    /// Returns the highest-priority entry (index 0).
    pub fn canonical(&self) -> Option<(&str, &[u8])> {
        self.entries
            .first()
            .map(|e| (e.mime_type.as_str(), e.data.as_slice()))
    }

    /// Returns `true` if any entry in this clipboard has a non-text (binary) MIME type.
    pub fn has_binary(&self) -> bool {
        self.entries.iter().any(|e| !is_text_mime(&e.mime_type))
    }

    /// Semantic equality: two clipboard payloads are considered the same if:
    /// - Neither has binary content and both decode to the same plain text, OR
    /// - Both have binary content and their canonical bytes are identical, OR
    /// - `self` is text-only while `other` had binary: this is a degraded echo
    ///   (X11 stores only the last `store()` call and loses earlier binary
    ///   MIME types), treated as unchanged when the text content matches.
    ///
    /// This handles the common case where wayland offers rich types (text/html,
    /// text/plain;charset=utf-8 …) but X11 only stores UTF8_STRING/STRING.
    /// After propagation the echo from X11 compares different at the MIME level
    /// yet carries the exact same human-readable content.
    pub fn same_content(&self, other: &ClipboardData) -> bool {
        let self_binary = self.has_binary();
        let other_binary = other.has_binary();

        if !self_binary && other_binary {
            // self (new data) is text-only but other (previous) had binary.
            // This is the classic X11 degraded-echo scenario: after we
            // propagated image/png + text/html, X11 only retained the last
            // MIME written in set_data, so on the next poll we read back
            // text/html only. Treat as unchanged when the text content matches
            // so we don't overwrite the richer clipboard with a degraded copy.
            match (self.text(), other.text()) {
                (Some(a), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            }
        } else if self_binary || other_binary {
            // At least one side has binary content: require matching canonical
            // (first-entry) bytes.
            self.canonical().map(|(_, b)| b) == other.canonical().map(|(_, b)| b)
        } else {
            // Pure-text: compare decoded strings, ignoring MIME-type name
            // differences between platforms (UTF8_STRING vs
            // text/plain;charset=utf-8 etc.).
            match (self.text(), other.text()) {
                (Some(a), Some(b)) => a == b,
                (None, None) => true,
                _ => false,
            }
        }
    }

    /// Add entries from `other` that are not already present in `self`.
    #[allow(dead_code)]
    pub fn merge(&mut self, other: ClipboardData) {
        for entry in other.entries {
            if !self.entries.iter().any(|e| e.mime_type == entry.mime_type) {
                self.entries.push(entry);
            }
        }
    }

    /// Remove entries with duplicate byte content, keeping the first occurrence.
    #[allow(dead_code)]
    pub fn dedup_by_content(&mut self) {
        let mut seen: Vec<Vec<u8>> = Vec::new();
        self.entries.retain(|e| {
            if seen.iter().any(|s| s == &e.data) {
                false
            } else {
                seen.push(e.data.clone());
                true
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Clipboard trait
// ---------------------------------------------------------------------------

pub trait Clipboard: std::fmt::Debug {
    fn display(&self) -> String;

    /// Fetch all available MIME-type entries from this clipboard.
    fn get_data(&self) -> MyResult<ClipboardData>;

    /// Write a full `ClipboardData` payload to this clipboard.
    fn set_data(&self, data: &ClipboardData) -> MyResult<()>;

    // --- Convenience text helpers (delegate to get_data / set_data) ---

    fn get(&self) -> MyResult<String> {
        Ok(self.get_data()?.text().unwrap_or_default())
    }

    fn set(&self, value: &str) -> MyResult<()> {
        self.set_data(&ClipboardData::from_text(value))
    }

    fn should_poll(&self) -> bool {
        true
    }

    /// Lower rank = preferred when deduplicating identical clipboards.
    fn rank(&self) -> u8 {
        100
    }
}

impl<T: Clipboard> Clipboard for Box<T> {
    fn display(&self) -> String {
        (**self).display()
    }
    fn get_data(&self) -> MyResult<ClipboardData> {
        (**self).get_data()
    }
    fn set_data(&self, data: &ClipboardData) -> MyResult<()> {
        (**self).set_data(data)
    }
    fn should_poll(&self) -> bool {
        (**self).should_poll()
    }
    fn rank(&self) -> u8 {
        (**self).rank()
    }
}

// ---------------------------------------------------------------------------
// WlrClipboard — wl-clipboard-rs backend (zwlr_data_control_manager_v1)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct WlrClipboard {
    pub display: String,
}

impl Clipboard for WlrClipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get_data(&self) -> MyResult<ClipboardData> {
        let _guard = lock_display_env();
        // Safety: all env mutations are serialised by DISPLAY_ENV_MUTEX.
        unsafe { env::set_var("WAYLAND_DISPLAY", &self.display) };

        // Discover all MIME types currently offered by the compositor.
        // wl-paste is spawned as a subprocess so it reads its own env copy —
        // no lock conflict. Fall back to the static list when unavailable.
        let mime_types: Vec<String> = {
            let mut discovered = discover_wayland_mime_types(&self.display);
            discovered.retain(|m| !is_wayland_internal_mime(m));
            if discovered.is_empty() {
                WAYLAND_PROBE_MIME_TYPES.iter().map(|s| s.to_string()).collect()
            } else {
                sort_by_mime_priority(&mut discovered, WAYLAND_PROBE_MIME_TYPES);
                discovered
            }
        };

        let mut data = ClipboardData::empty();

        for mime in &mime_types {
            let result = get_contents(
                ClipboardType::Regular,
                Seat::Unspecified,
                PasteMimeType::Specific(mime.as_str()),
            );
            match result {
                Ok((mut pipe, _)) => {
                    let mut bytes = Vec::new();
                    pipe.read_to_end(&mut bytes)?;
                    if !bytes.is_empty() {
                        data.entries.push(MimeEntry {
                            mime_type: mime.clone(),
                            data: bytes,
                        });
                    }
                }
                Err(PasteError::NoSeats)
                | Err(PasteError::ClipboardEmpty)
                | Err(PasteError::NoMimeType) => {}
                // Compositor is gone entirely — propagate immediately so the
                // caller (get_wayland) can drop this display from the list.
                Err(e @ PasteError::WaylandConnection(_)) => return Err(e.into()),
                Err(err) => {
                    log::debug!(
                        "WlrClipboard({}): error getting '{}': {}",
                        self.display,
                        mime,
                        err
                    );
                }
            }
        }

        Ok(data)
    }

    fn set_data(&self, data: &ClipboardData) -> MyResult<()> {
        if data.is_empty() {
            return Ok(());
        }

        let _guard = lock_display_env();
        unsafe { env::set_var("WAYLAND_DISPLAY", &self.display) };

        // Keep only Wayland-native MIME types (skip X11-specific atoms).
        let entries: Vec<(Vec<u8>, String)> = data
            .entries
            .iter()
            .filter(|e| {
                !matches!(
                    e.mime_type.as_str(),
                    "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT"
                )
            })
            .map(|e| (e.data.clone(), e.mime_type.clone()))
            .collect();

        if entries.is_empty() {
            // All entries were X11-only atoms; fall back to plain text.
            if let Some(text) = data.text() {
                let result = std::panic::catch_unwind(move || {
                    Options::new().copy(
                        Source::Bytes(text.into_bytes().into()),
                        CopyMimeType::Text,
                    )
                });
                return Ok(result.standardize().generify()??);
            }
            return Ok(());
        }

        let result = std::panic::catch_unwind(move || {
            let sources: Vec<MimeSource> = entries
                .iter()
                .map(|(bytes, mime)| {
                    let source = Source::Bytes(bytes.clone().into());
                    let mime_type = match mime.as_str() {
                        "text/plain" | "text/plain;charset=utf-8" => CopyMimeType::Text,
                        m => CopyMimeType::Specific(m.to_string()),
                    };
                    MimeSource { source, mime_type }
                })
                .collect();
            Options::new().copy_multi(sources)
        });

        Ok(result.standardize().generify()??)
    }

    fn rank(&self) -> u8 {
        10
    }
}

// ---------------------------------------------------------------------------
// WlCommandClipboard — fallback using wl-copy / wl-paste binaries
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct WlCommandClipboard {
    pub display: String,
}

impl Clipboard for WlCommandClipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get_data(&self) -> MyResult<ClipboardData> {
        // Discover available MIME types; fall back to static list.
        let mime_types: Vec<String> = {
            let mut discovered = discover_wayland_mime_types(&self.display);
            discovered.retain(|m| !is_wayland_internal_mime(m));
            if discovered.is_empty() {
                WAYLAND_PROBE_MIME_TYPES.iter().map(|s| s.to_string()).collect()
            } else {
                sort_by_mime_priority(&mut discovered, WAYLAND_PROBE_MIME_TYPES);
                discovered
            }
        };

        let mut data = ClipboardData::empty();

        for mime in &mime_types {
            let out = Command::new("wl-paste")
                .args(["--no-newline", "--type", mime.as_str()])
                .env("WAYLAND_DISPLAY", &self.display)
                .output();
            match out {
                Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                    data.entries.push(MimeEntry {
                        mime_type: mime.clone(),
                        data: o.stdout,
                    });
                }
                _ => {}
            }
        }

        Ok(data)
    }

    fn set_data(&self, data: &ClipboardData) -> MyResult<()> {
        if data.is_empty() {
            return Ok(());
        }

        // wl-copy accepts only one MIME type at a time; pick the first
        // Wayland-native entry (skip X11-specific atoms).
        let entry = data
            .entries
            .iter()
            .find(|e| {
                !matches!(e.mime_type.as_str(), "UTF8_STRING" | "STRING" | "TEXT")
            })
            .or_else(|| data.entries.first());

        let Some(entry) = entry else {
            return Ok(());
        };

        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new("wl-copy")
            .args(["--type", &entry.mime_type])
            .env("WAYLAND_DISPLAY", &self.display)
            .stdin(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&entry.data)?;
        }

        Ok(())
    }

    /// WlCommandClipboard can be polled (wl-paste is a one-shot command).
    fn should_poll(&self) -> bool {
        true
    }

    fn rank(&self) -> u8 {
        200
    }
}

// ---------------------------------------------------------------------------
// X11ClipboardDirect — uses the x11-clipboard crate for full atom support
// ---------------------------------------------------------------------------

/// X11 clipboard backend using the x11-clipboard crate directly.
/// Supports custom MIME-type atoms (text/html, image/png, etc.) via xcb atom
/// interning, in addition to the standard UTF8_STRING and STRING targets.
pub struct X11ClipboardDirect {
    display: String,
    inner: x11_clipboard::Clipboard,
}

impl std::fmt::Debug for X11ClipboardDirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11ClipboardDirect")
            .field("display", &self.display)
            .finish()
    }
}

impl X11ClipboardDirect {
    /// Create a new X11 clipboard connection to the given display (e.g. `":0"`).
    pub fn new(display: &str) -> MyResult<Self> {
        let _guard = lock_display_env();
        unsafe { env::set_var("DISPLAY", display) };
        let inner = x11_clipboard::Clipboard::new()
            .map_err(|e| MyError::X11Clipboard(e.to_string()))?;
        Ok(Self {
            display: display.to_string(),
            inner,
        })
    }

    /// Intern a custom atom by name (e.g. `"text/html"`, `"image/png"`).
    /// Returns `None` if the atom does not exist on this display.
    fn intern_atom(
        cb: &x11_clipboard::Clipboard,
        name: &str,
    ) -> Option<x11_clipboard::xcb::Atom> {
        x11_clipboard::xcb::intern_atom(&cb.getter.connection, true, name)
            .get_reply()
            .ok()
            .map(|r| r.atom())
            .filter(|&a| a != 0)
    }

    /// Query the CLIPBOARD selection for its `TARGETS` meta-atom and return
    /// the names of all supported target atoms, sorted by priority.
    /// Falls back to an empty list if TARGETS is not supported.
    fn discover_targets(cb: &x11_clipboard::Clipboard) -> Vec<String> {
        let timeout = Some(Duration::from_millis(200));

        // Intern the TARGETS atom (must exist on any ICCCM-compliant display).
        let targets_atom =
            match x11_clipboard::xcb::intern_atom(&cb.getter.connection, false, "TARGETS")
                .get_reply()
            {
                Ok(r) if r.atom() != 0 => r.atom(),
                _ => return vec![],
            };

        // Load the TARGETS value from the CLIPBOARD selection.
        let bytes = match cb.load(
            cb.getter.atoms.clipboard,
            targets_atom,
            cb.getter.atoms.property,
            timeout,
        ) {
            Ok(b) if b.len() >= 4 => b,
            _ => return vec![],
        };

        // TARGETS data is a packed array of 32-bit atom IDs (native byte order).
        let mut names: Vec<String> = bytes
            .chunks_exact(4)
            .filter_map(|c| {
                let atom_id = u32::from_ne_bytes(c.try_into().ok()?);
                if atom_id == 0 {
                    return None;
                }
                x11_clipboard::xcb::get_atom_name(&cb.getter.connection, atom_id)
                    .get_reply()
                    .ok()
                    .map(|r| r.name().to_string())
            })
            .filter(|n| !is_x11_meta_target(n))
            .take(MAX_MIME_TYPES)
            .collect();

        sort_by_mime_priority(&mut names, X11_PROBE_TARGETS);
        names
    }
}

impl Clipboard for X11ClipboardDirect {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get_data(&self) -> MyResult<ClipboardData> {
        let cb = &self.inner;
        let mut data = ClipboardData::empty();
        let timeout = Some(Duration::from_millis(200));

        // Dynamically discover available targets via the TARGETS meta-atom.
        // Fall back to the static probe list if TARGETS is not supported.
        let targets: Vec<String> = {
            let discovered = Self::discover_targets(cb);
            if discovered.is_empty() {
                X11_PROBE_TARGETS.iter().map(|s| s.to_string()).collect()
            } else {
                discovered
            }
        };

        for target_name in &targets {
            // Resolve target atom using built-ins or intern it on the fly.
            let atom = match target_name.as_str() {
                "UTF8_STRING" => cb.getter.atoms.utf8_string,
                "STRING" | "TEXT" => cb.getter.atoms.string,
                other => match Self::intern_atom(cb, other) {
                    Some(a) => a,
                    None => continue,
                },
            };

            match cb.load(
                cb.getter.atoms.clipboard,
                atom,
                cb.getter.atoms.property,
                timeout,
            ) {
                Ok(bytes) if !bytes.is_empty() => {
                    data.entries.push(MimeEntry {
                        mime_type: target_name.clone(),
                        data: bytes,
                    });
                }
                _ => {}
            }
        }

        Ok(data)
    }

    fn set_data(&self, data: &ClipboardData) -> MyResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        let cb = &self.inner;

        // x11-clipboard's store() replaces the entire X11 selection owner on
        // each call, so storing multiple entries sequentially leaves only the
        // last one visible via TARGETS.  To avoid losing earlier binary/rich
        // MIME types, store exactly ONE entry: the highest-priority entry that
        // maps to a real X11 atom (prefer non-text-atom MIME types first).
        let entry = data
            .entries
            .iter()
            .find(|e| {
                !matches!(
                    e.mime_type.as_str(),
                    "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT"
                )
            })
            .or_else(|| data.entries.first());

        let Some(entry) = entry else {
            return Ok(());
        };

        let atom = match entry.mime_type.as_str() {
            "UTF8_STRING" => cb.setter.atoms.utf8_string,
            "STRING" | "TEXT" => cb.setter.atoms.string,
            "text/plain" | "text/plain;charset=utf-8" => cb.setter.atoms.utf8_string,
            other => match Self::intern_atom(cb, other) {
                Some(a) => a,
                None => {
                    // Atom not available on this display; fall back to text.
                    if let Some(text) = data.text() {
                        cb.store(
                            cb.setter.atoms.clipboard,
                            cb.setter.atoms.utf8_string,
                            text.as_bytes(),
                        )
                        .map_err(|e| MyError::X11Clipboard(e.to_string()))?;
                    }
                    return Ok(());
                }
            },
        };

        cb.store(cb.setter.atoms.clipboard, atom, entry.data.as_slice())
            .map_err(|e| MyError::X11Clipboard(e.to_string()))?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ArClipboard — arboard-based backend (text + image support)
// ---------------------------------------------------------------------------

/// arboard-based clipboard backend; not wired into the auto-discovery flow
/// but available for direct instantiation or future extension.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ArClipboard {
    pub display: String,
}

impl Clipboard for ArClipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get_data(&self) -> MyResult<ClipboardData> {
        let _guard = lock_display_env();
        unsafe { env::set_var("WAYLAND_DISPLAY", &self.display) };
        let mut cb = arboard::Clipboard::new()?;

        // Prefer image content (richer), fall back to text.
        if let Ok(img) = cb.get_image() {
            // Store raw RGBA with a 8-byte width/height prefix so we can
            // reconstruct it in set_data without needing a PNG encoder.
            let mut raw: Vec<u8> = Vec::with_capacity(8 + img.bytes.len());
            raw.extend_from_slice(&(img.width as u32).to_le_bytes());
            raw.extend_from_slice(&(img.height as u32).to_le_bytes());
            raw.extend_from_slice(&img.bytes);
            return Ok(ClipboardData {
                entries: vec![MimeEntry {
                    mime_type: "image/x-raw-rgba".to_string(),
                    data: raw,
                }],
            });
        }

        let text = cb.get_text().unwrap_or_default();
        if text.is_empty() {
            return Ok(ClipboardData::empty());
        }
        Ok(ClipboardData::from_text(&text))
    }

    fn set_data(&self, data: &ClipboardData) -> MyResult<()> {
        let _guard = lock_display_env();
        unsafe { env::set_var("WAYLAND_DISPLAY", &self.display) };
        let mut cb = arboard::Clipboard::new()?;

        if let Some(raw) = data.get_mime("image/x-raw-rgba") {
            if raw.len() >= 8 {
                let width = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
                let height = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
                let pixels = raw[8..].to_vec();
                if pixels.len() == width * height * 4 {
                    cb.set_image(arboard::ImageData {
                        width,
                        height,
                        bytes: pixels.into(),
                    })?;
                    return Ok(());
                }
            }
        }

        if let Some(text) = data.text() {
            cb.set_text(text)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// X11Clipboard — legacy terminal_clipboard fallback
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct X11Backend(pub Rc<RefCell<terminal_clipboard::X11Clipboard>>);

impl X11Backend {
    /// Try to only call this once; repeated initializations may time out.
    pub fn new(display: &str) -> MyResult<Self> {
        let _guard = lock_display_env();
        unsafe { env::set_var("DISPLAY", display) };
        let backend = terminal_clipboard::X11Clipboard::new().standardize()?;
        Ok(Self(Rc::new(RefCell::new(backend))))
    }
}

pub struct X11Clipboard {
    pub display: String,
    backend: X11Backend,
}

impl std::fmt::Debug for X11Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Clipboard")
            .field("display", &self.display)
            .finish()
    }
}

impl X11Clipboard {
    pub fn new(display: String) -> MyResult<Self> {
        Ok(Self {
            backend: X11Backend::new(&display)?,
            display,
        })
    }
}

impl Clipboard for X11Clipboard {
    fn display(&self) -> String {
        self.display.clone()
    }

    fn get_data(&self) -> MyResult<ClipboardData> {
        use terminal_clipboard::Clipboard as _;
        let text = self
            .backend
            .0
            .try_borrow()?
            .get_string()
            .unwrap_or_default();
        if text.is_empty() {
            return Ok(ClipboardData::empty());
        }
        Ok(ClipboardData::from_text(&text))
    }

    fn set_data(&self, data: &ClipboardData) -> MyResult<()> {
        use terminal_clipboard::Clipboard as _;
        let text = data.text().unwrap_or_default();
        self.backend
            .0
            .try_borrow_mut()?
            .set_string(&text)
            .standardize()?;
        Ok(())
    }
}
