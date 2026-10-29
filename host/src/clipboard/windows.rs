//! Windows clipboard backend: text via `arboard::Clipboard`, change
//! detection via `GetClipboardSequenceNumber` -- a cheap Win32 query, bumped
//! by the OS on every clipboard write by any application (including our own
//! `set_text` calls, see `ClipboardSync::apply_remote`).
//!
//! The file list (`CF_HDROP`) is NOT read through arboard: its
//! `file_list()` turns any `clipboard_win` error into
//! `ContentNotAvailable`, which hides why copied Explorer files never reach
//! the client (D87). `read_file_list` below talks to Win32 directly and logs
//! every failure with the Win32 error code. D87b: every call also logs one
//! line with the clipboard formats the process actually sees
//! (`EnumClipboardFormats`) and whether `CF_HDROP` is among them.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so `GetClipboardSequenceNumber`'s
//! signature below comes from reading the `windows` 0.61.3 source fetched
//! into the local registry cache, not from memory -- see executor rule 3.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Foundation::{GetLastError, ERROR_SUCCESS};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use super::ClipboardBackend;

/// Closes the clipboard on drop, so every early return releases it.
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // Safety: only constructed after a successful `OpenClipboard` on
        // this thread; no out-parameters.
        if let Err(err) = unsafe { CloseClipboard() } {
            tracing::warn!(error = %err, "failed to close clipboard");
        }
    }
}

/// Human-readable name of a clipboard format: registered formats (id >=
/// 0xC000) by `GetClipboardFormatNameW`, standard ones from a short table,
/// anything else as a bare number.
fn format_label(id: u32) -> String {
    if id >= 0xC000 {
        let mut buf = [0u16; 256];
        // Safety: `buf` is a valid writable buffer; the call returns the
        // number of characters copied (0 on failure) and needs no open
        // clipboard.
        let n = unsafe { GetClipboardFormatNameW(id, &mut buf) };
        if n > 0 {
            let n = (n as usize).min(buf.len());
            return format!("{id}:{}", String::from_utf16_lossy(&buf[..n]));
        }
        return id.to_string();
    }
    let name = match id {
        1 => "CF_TEXT",
        2 => "CF_BITMAP",
        7 => "CF_OEMTEXT",
        8 => "CF_DIB",
        13 => "CF_UNICODETEXT",
        15 => "CF_HDROP",
        16 => "CF_LOCALE",
        17 => "CF_DIBV5",
        _ => return id.to_string(),
    };
    format!("{id}:{name}")
}

/// Lists the formats currently on the clipboard. The clipboard must be open.
/// Returns the label string and the Win32 error left after the enumeration
/// ended (`None` for `ERROR_SUCCESS`).
fn list_formats() -> (String, Option<u32>) {
    let mut labels = Vec::new();
    let mut format = 0u32;
    loop {
        // Safety: the clipboard is open on this thread; `format` is 0 or the
        // previous result, as the API requires.
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        labels.push(format_label(format));
    }
    // Safety: no preconditions; reads the calling thread's last error.
    let last = unsafe { GetLastError() };
    let code = (last != ERROR_SUCCESS).then_some(last.0);
    (labels.join(", "), code)
}

/// Reads the `CF_HDROP` path list straight through Win32, logging every
/// failure (arboard hides them, D87). `None` when there are no files. D87b:
/// logs the clipboard formats this process sees on every call.
fn read_file_list() -> Option<Vec<PathBuf>> {
    // Another application may hold the clipboard for a moment.
    let mut opened = None;
    for attempt in 0..5 {
        // Safety: a null owner window is allowed; the clipboard is then
        // associated with the current task.
        match unsafe { OpenClipboard(None) } {
            Ok(()) => {
                opened = Some(ClipboardGuard);
                break;
            }
            Err(err) if attempt == 4 => {
                tracing::warn!(error = %err, "failed to open clipboard to read file list");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let _guard = opened?;

    let (formats, enum_error) = list_formats();
    // Safety: no preconditions; also fine with the clipboard open.
    let hdrop_available = unsafe { IsClipboardFormatAvailable(u32::from(CF_HDROP.0)) }.is_ok();
    // Safety: no preconditions -- a plain Win32 query.
    let seq = unsafe { GetClipboardSequenceNumber() };
    match enum_error {
        Some(code) => tracing::info!(
            seq,
            formats = %formats,
            hdrop = hdrop_available,
            enum_error = code,
            "clipboard formats seen (D87b)"
        ),
        None => tracing::info!(
            seq,
            formats = %formats,
            hdrop = hdrop_available,
            "clipboard formats seen (D87b)"
        ),
    }
    if !hdrop_available {
        return None;
    }

    // Safety: the clipboard is open (held by `_guard`). The returned handle
    // is owned by the clipboard and must not be freed or kept after close.
    let handle = match unsafe { GetClipboardData(u32::from(CF_HDROP.0)) } {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!(error = %err, "CF_HDROP is on the clipboard but could not be read");
            return None;
        }
    };
    let hdrop = HDROP(handle.0);

    // Safety: `hdrop` is a valid CF_HDROP handle while the clipboard stays
    // open; index 0xFFFFFFFF with no buffer returns the file count.
    let count = unsafe { DragQueryFileW(hdrop, 0xFFFF_FFFF, None) };
    let mut list = Vec::new();
    for i in 0..count {
        // Safety: as above; no buffer returns the path length in
        // characters, without the terminating NUL.
        let len = unsafe { DragQueryFileW(hdrop, i, None) } as usize;
        let mut buf = vec![0u16; len + 1];
        // Safety: as above; `buf` has room for `len` characters + NUL.
        let n = unsafe { DragQueryFileW(hdrop, i, Some(&mut buf)) } as usize;
        if n == 0 {
            tracing::warn!(
                index = i,
                error = %windows::core::Error::from_win32(),
                "failed to read a path from CF_HDROP"
            );
            continue;
        }
        list.push(PathBuf::from(OsString::from_wide(&buf[..n])));
    }
    if list.is_empty() {
        tracing::warn!(count, "CF_HDROP had no readable paths");
        return None;
    }
    tracing::info!(count = list.len(), "clipboard file list read");
    Some(list)
}

pub struct WinClipboardBackend {
    // `arboard::Clipboard`'s Windows backend is a zero-sized `Clipboard(())`
    // (see arboard 3.6.1 `src/platform/windows.rs`), trivially `Send`, so a
    // single instance can be held here and reused across calls.
    clipboard: arboard::Clipboard,
}

impl WinClipboardBackend {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new()?,
        })
    }
}

impl ClipboardBackend for WinClipboardBackend {
    fn change_count(&mut self) -> u64 {
        // Safety: no preconditions -- a plain Win32 query with no
        // out-parameters to validate.
        u64::from(unsafe { GetClipboardSequenceNumber() })
    }

    fn get_text(&mut self) -> Option<String> {
        match self.clipboard.get_text() {
            Ok(text) => Some(text),
            // Genuinely "nothing to sync" (empty clipboard, or non-text
            // content like an image) -- not a failure, not logged.
            Err(arboard::Error::ContentNotAvailable) => None,
            Err(err) => {
                tracing::warn!(error = %err, "failed to read clipboard text");
                None
            }
        }
    }

    fn get_file_list(&mut self) -> Option<Vec<PathBuf>> {
        read_file_list()
    }

    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.clipboard.set_text(text)?;
        Ok(())
    }
}
