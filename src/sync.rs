use chrono::Local;
use std::collections::HashSet;
use std::{thread::sleep, time::Duration};
use wl_clipboard_rs::paste::Error as PasteError;

use crate::clipboard::*;
use crate::error::{MyError, MyResult, StandardizedError};
use crate::log::{self, concise_numbers};

pub fn get_clipboards() -> MyResult<Vec<Box<dyn Clipboard>>> {
    log::debug!("identifying unique clipboards...");
    let mut clipboards = get_clipboards_spec(get_wayland);
    clipboards.extend(get_clipboards_spec(get_x11));

    // Find the richest initial clipboard content (first non-empty clipboard).
    let start = clipboards
        .iter()
        .find_map(|c| c.get_data().ok().filter(|d| !d.is_empty()))
        .unwrap_or_default();

    if let Some(text) = start.text() {
        log::sensitive!(log::info, "Clipboard contents at the start: '{text}'");
    } else if !start.is_empty() {
        log::sensitive!(
            log::info,
            "Clipboard at start: {} MIME type(s), primary='{}'",
            start.entries.len(),
            start.canonical().map(|(m, _)| m).unwrap_or("(empty)")
        );
    }

    // Deduplicate: remove clipboards that share the same backing store.
    let mut remove_me = HashSet::new();
    let len = clipboards.len();
    for i in 0..len {
        if !remove_me.contains(&i) {
            let cb1 = &clipboards[i];
            for j in (i + 1)..len {
                if remove_me.contains(&j) {
                    continue;
                }
                let cb2 = &clipboards[j];
                if are_same(&**cb1, &**cb2)? {
                    if cb1.rank() <= cb2.rank() {
                        log::debug!("dupe detected: {cb1:?} == {cb2:?} -> removing {cb2:?}");
                        remove_me.insert(j);
                    } else {
                        log::debug!("dupe detected: {cb1:?} == {cb2:?} -> removing {cb1:?}");
                        remove_me.insert(i);
                        break; // cb1 is removed; stop inner loop
                    }
                }
            }
        }
    }

    let clipboards = clipboards
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !remove_me.contains(i))
        .map(|(_, c)| c)
        .collect::<Vec<Box<dyn Clipboard>>>();

    // Synchronise all discovered clipboards to the same initial state.
    if !start.is_empty() {
        for c in clipboards.iter() {
            if let Err(e) = c.set_data(&start) {
                log::warning!(
                    "error initializing clipboard {}: {}",
                    c.display(),
                    e
                );
            }
        }
    }

    log::info!("Using clipboards: {:?}", clipboards);
    Ok(clipboards)
}

pub fn keep_synced(clipboards: &Vec<Box<dyn Clipboard>>) -> MyResult<()> {
    if clipboards.is_empty() {
        return Err(MyError::NoClipboards);
    }
    let mut last_data: Option<ClipboardData> = None;
    loop {
        sleep(Duration::from_millis(100));
        let new_data = await_change(clipboards, last_data.as_ref())?;
        last_data = Some(new_data.clone());
        log::debug!(
            "propagating {} MIME type(s) to {} clipboard(s)",
            new_data.entries.len(),
            clipboards.len()
        );
        for c in clipboards {
            if let Err(e) = c.set_data(&new_data) {
                log::error!(
                    "error propagating clipboard to {}: {}",
                    c.display(),
                    e
                );
            }
        }
    }
}

/// Check whether two clipboards share the same backing store by writing a
/// probe string into one and reading it from the other, then restoring.
///
/// Any I/O error is treated as "not the same" — a clipboard we can't write to
/// or read from is definitely not sharing state with another.
fn are_same(one: &dyn Clipboard, two: &dyn Clipboard) -> MyResult<bool> {
    let probe1 = one.display();
    let probe2 = two.display();
    if one.set(&probe1).is_err() {
        return Ok(false);
    }
    match two.get() {
        Ok(v) if v != probe1 => return Ok(false),
        Err(_) => return Ok(false),
        _ => {}
    }
    if two.set(&probe2).is_err() {
        return Ok(false);
    }
    match one.get() {
        Ok(v) if v != probe2 => return Ok(false),
        Err(_) => return Ok(false),
        _ => {}
    }
    Ok(true)
}

fn get_clipboards_spec<F: Fn(u8) -> MyResult<Option<Box<dyn Clipboard>>>>(
    getter: F,
) -> Vec<Box<dyn Clipboard>> {
    let mut clipboards: Vec<Box<dyn Clipboard>> = Vec::new();
    let mut xcb_conn_err = None;
    let mut xcb_conn_failed_clipboards = vec![];
    for i in 0..u8::MAX {
        let result = getter(i);
        match result {
            Ok(option) => {
                if let Some(clipboard) = option {
                    log::debug!("Found clipboard: {:?}", clipboard);
                    clipboards.push(clipboard);
                }
            }
            Err(MyError::TerminalClipboard(StandardizedError {
                inner,
                stdio: None,
            })) if format!("{inner}") == "clipboard error: X11 clipboard error : XCB connection error: Connection" => {
                xcb_conn_failed_clipboards.push(i);
                xcb_conn_err = Some(inner);
            }
            Err(MyError::X11Clipboard(ref msg)) if msg.contains("connection") || msg.contains("Connection") => {
                xcb_conn_failed_clipboards.push(i);
            }
            Err(err) => log::error!(
                "unexpected error while attempting to setup clipboard {}: {}",
                i,
                err
            ),
        }
    }
    if let Some(err) = xcb_conn_err {
        let displays = concise_numbers(&xcb_conn_failed_clipboards);
        log::warning!(
            "Issue connecting to some x11 clipboards. \
This is expected when hooking up to gnome wayland, and not a problem in that context. \
Details: '{err}' for x11 displays: {displays}",
        );
    }

    clipboards
}

fn get_wayland(n: u8) -> MyResult<Option<Box<dyn Clipboard>>> {
    let wl_display = format!("wayland-{}", n);
    let clipboard = WlrClipboard {
        display: wl_display.clone(),
    };

    // Probe read capability first.
    let data = match clipboard.get_data() {
        // Any Wayland connection failure means this compositor is unavailable.
        Err(MyError::WlcrsPaste(PasteError::WaylandConnection(_))) => return Ok(None),

        Err(MyError::WlcrsPaste(PasteError::MissingProtocol {
            name: "zwlr_data_control_manager_v1",
            version: 1,
        })) => {
            log::warning!(
                "{wl_display} does not support zwlr_data_control_manager_v1. If you are running \
gnome in wayland, that's OK because it provides an x11 clipboard, which will be used instead. \
Otherwise, `wl-copy` will be used to sync data *into* this clipboard, but it will not be possible \
to read data *from* this clipboard into other clipboards."
            );
            let command = WlCommandClipboard {
                display: wl_display.clone(),
            };
            // Verify wl-paste and wl-copy are functional for this display.
            let Ok(data) = command.get_data() else {
                return Ok(None);
            };
            if command.set_data(&data).is_err() {
                return Ok(None);
            }
            return Ok(Some(Box::new(command)));
        }

        other => other?,
    };

    // Probe write capability: if the clipboard already has content, restore it.
    // This catches compositors that accept paste (one-shot) but reject copy
    // (long-lived connection) with WaylandConnection(NoCompositorListening).
    if !data.is_empty() {
        if let Err(e) = clipboard.set_data(&data) {
            log::debug!("{wl_display}: write probe failed ({e}), skipping this display");
            return Ok(None);
        }
    }

    Ok(Some(Box::new(clipboard)))
}

fn get_x11(n: u8) -> MyResult<Option<Box<dyn Clipboard>>> {
    let display = format!(":{}", n);

    // Try the richer x11-clipboard backend first.
    if let Ok(cb) = X11ClipboardDirect::new(&display) {
        match cb.get_data() {
            Ok(_) => return Ok(Some(Box::new(cb))),
            Err(e) => log::debug!("X11ClipboardDirect({display}): {e}, falling back"),
        }
    }

    // Fall back to terminal_clipboard (text-only).
    let clipboard = X11Clipboard::new(display)?;
    clipboard.get_data()?;
    Ok(Some(Box::new(clipboard)))
}

/// Poll all clipboards and return the first `ClipboardData` that differs from
/// the known state. Uses semantic comparison so that content equivalent across
/// platforms (e.g. wayland text/html vs X11 UTF8_STRING carrying the same
/// plain text) does not trigger spurious re-propagation.
///
/// `last_data` — if `Some`, used as the baseline instead of re-reading from
/// the clipboard. This avoids echo-loops after propagation.
fn await_change(
    clipboards: &[Box<dyn Clipboard>],
    last_data: Option<&ClipboardData>,
) -> MyResult<ClipboardData> {
    // Use the caller-supplied baseline when available; otherwise snapshot the
    // current clipboard state so we can detect the first subsequent change.
    let start_data: Option<ClipboardData> = last_data
        .cloned()
        .or_else(|| {
            clipboards
                .iter()
                .filter(|c| c.should_poll())
                .find_map(|c| c.get_data().ok().filter(|d| !d.is_empty()))
        });

    loop {
        for c in clipboards {
            if !c.should_poll() {
                continue;
            }
            let new_data = c.get_data()?;
            if new_data.is_empty() {
                continue;
            }
            let changed = match &start_data {
                None => !new_data.is_empty(),
                Some(prev) => !new_data.same_content(prev),
            };
            if changed {
                log::info!("clipboard updated from display {}", c.display());
                for entry in &new_data.entries {
                    let mime = entry.mime_type.as_str();
                    let bytes = entry.data.as_slice();
                    if is_text_mime(mime) {
                        let content = std::str::from_utf8(bytes)
                            .map(|s| s.trim_end_matches('\0').to_string())
                            .unwrap_or_else(|_| format!("(invalid UTF-8, {} byte(s))", bytes.len()));
                        log::sensitive!(
                            log::info,
                            "clipboard MIME: '{}', {} byte(s): {:?}",
                            mime,
                            bytes.len(),
                            content
                        );
                    } else {
                        log::sensitive!(
                            log::info,
                            "clipboard MIME: '{}', {} byte(s) (binary)",
                            mime,
                            bytes.len()
                        );
                    }
                }
                return Ok(new_data);
            }
        }
        sleep(Duration::from_millis(200));
    }
}
