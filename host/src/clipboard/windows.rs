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
//! D87c: Explorer puts only the OLE `DataObject` format on the Win32
//! clipboard (no `CF_HDROP`) and hands the file list out through
//! `IDataObject`, so when `CF_HDROP` is absent but `DataObject` is present
//! `read_file_list` falls back to `OleGetClipboard` (`read_file_list_ole`).
//!
//! D87d: in full mode (helper as SYSTEM) even the OLE read fails, so the helper
//! takes the file list from the tray agent running as the user
//! (`super::agent_files`); `WinClipboardBackend::get_file_list` waits for the
//! agent's entry for the current clipboard sequence number.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so `GetClipboardSequenceNumber`'s
//! signature below comes from reading the `windows` 0.61.3 source fetched
//! into the local registry cache, not from memory -- see executor rule 3.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::Win32::Foundation::{GetLastError, ERROR_SUCCESS};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
};
use windows::Win32::System::Ole::{
    OleGetClipboard, OleInitialize, OleUninitialize, ReleaseStgMedium, CF_HDROP,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

use super::agent_files;
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
/// Returns the label string, the Win32 error left after the enumeration
/// ended (`None` for `ERROR_SUCCESS`), and whether the registered OLE
/// `DataObject` format is among them (D87c).
fn list_formats() -> (String, Option<u32>, bool) {
    let mut labels = Vec::new();
    let mut format = 0u32;
    let mut has_data_object = false;
    loop {
        // Safety: the clipboard is open on this thread; `format` is 0 or the
        // previous result, as the API requires.
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        let label = format_label(format);
        if format >= 0xC000 && label.ends_with(":DataObject") {
            has_data_object = true;
        }
        labels.push(label);
    }
    // Safety: no preconditions; reads the calling thread's last error.
    let last = unsafe { GetLastError() };
    let code = (last != ERROR_SUCCESS).then_some(last.0);
    (labels.join(", "), code, has_data_object)
}

/// Reads the paths out of a `CF_HDROP` handle with `DragQueryFileW`, logging
/// every path that cannot be read. Shared by the Win32 and the OLE (D87c)
/// paths. The handle must stay valid for the duration of the call.
fn paths_from_hdrop(hdrop: HDROP) -> Vec<PathBuf> {
    // Safety: `hdrop` is a valid CF_HDROP handle (caller contract); index
    // 0xFFFFFFFF with no buffer returns the file count.
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
    list
}

/// Uninitializes OLE on the owning thread when dropped (D87c). OLE is kept
/// initialized only for the duration of one read: `OleInitialize` makes the
/// thread an STA with a hidden OLE window, and the clipboard watch thread
/// does not pump messages, so a permanent STA could stall other processes'
/// `SendMessage(HWND_BROADCAST)`.
struct OleGuard;

impl Drop for OleGuard {
    fn drop(&mut self) {
        // Safety: only constructed after a successful `OleInitialize` on this
        // thread and a local of the same call, so dropped on the same
        // thread; balances it.
        unsafe { OleUninitialize() };
    }
}

/// Initializes OLE on the current thread for the lifetime of the returned
/// guard. Warns on every failed attempt (no caching: the init is per read).
fn init_ole() -> Option<OleGuard> {
    // Safety: a null reserved pointer is the documented value.
    match unsafe { OleInitialize(None) } {
        Ok(()) => Some(OleGuard),
        Err(err) => {
            // E.g. RPC_E_CHANGED_MODE when the thread already runs in MTA.
            tracing::warn!(error = %err, "OleInitialize failed; OLE clipboard fallback disabled on this thread");
            None
        }
    }
}

/// Releases a `STGMEDIUM` on drop, so every exit path frees it.
struct StgMediumGuard(STGMEDIUM);

impl Drop for StgMediumGuard {
    fn drop(&mut self) {
        // Safety: the medium was filled in by a successful
        // `IDataObject::GetData` and is released exactly once, here.
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

/// D87c: reads the `CF_HDROP` list through OLE (`OleGetClipboard` +
/// `IDataObject::GetData`). Explorer publishes copied files only as an OLE
/// `DataObject`, never as a Win32 `CF_HDROP`. Must be called with the Win32
/// clipboard closed. Called from the `clipboard::watch_loop` thread (its own
/// std thread per session, no other COM users); OLE is initialized only for
/// this one read because that thread does not pump messages.
fn read_file_list_ole() -> Option<Vec<PathBuf>> {
    // Declared first on purpose: locals drop in reverse order, so the
    // `IDataObject` and the `StgMediumGuard` below are released BEFORE
    // `OleUninitialize` runs.
    let _ole = init_ole()?;
    // Safety: OLE is initialized on this thread (`_ole`); no arguments.
    let data = match unsafe { OleGetClipboard() } {
        Ok(data) => data,
        Err(err) => {
            tracing::warn!(error = %err, "OleGetClipboard failed");
            return None;
        }
    };
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // Safety: `format` is a valid FORMATETC that outlives the call.
    let medium = match unsafe { data.GetData(&format) } {
        Ok(medium) => StgMediumGuard(medium),
        Err(err) => {
            tracing::warn!(error = %err, "OLE clipboard has no readable CF_HDROP");
            return None;
        }
    };
    if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
        tracing::warn!(tymed = medium.0.tymed, "OLE CF_HDROP medium is not HGLOBAL");
        return None;
    }
    // Safety: `tymed` is TYMED_HGLOBAL, so the `hGlobal` union member is the
    // active one; the handle is owned by `medium` and released by its guard.
    let hglobal = unsafe { medium.0.u.hGlobal };
    let list = paths_from_hdrop(HDROP(hglobal.0));
    if list.is_empty() {
        tracing::warn!("OLE CF_HDROP had no readable paths");
        return None;
    }
    tracing::info!(count = list.len(), "clipboard file list read via OLE");
    Some(list)
}

/// Outcome of one `read_file_list_impl` call (D87d).
struct FileListRead {
    /// The file list, `None` when there are no (readable) files.
    files: Option<Vec<PathBuf>>,
    /// The Win32 clipboard had the OLE `DataObject` format but no `CF_HDROP`:
    /// the files, if any, are only reachable through OLE (or, for a SYSTEM
    /// helper, through the tray agent).
    data_object_only: bool,
}

/// Reads the file list the way the tray agent does (Win32, then the OLE
/// fallback) and returns it. `None` when there are no files. Used by
/// `rcdesk-agent` (D87d) to hand the list to the SYSTEM helper, which cannot
/// read it itself.
pub fn read_clipboard_files() -> Option<Vec<PathBuf>> {
    read_file_list_impl(true).files
}

/// Reads the `CF_HDROP` path list straight through Win32, logging every
/// failure (arboard hides them, D87). `files` is `None` when there are no
/// files. D87b: logs the clipboard formats this process sees on every call.
/// D87c: when `CF_HDROP` is absent but the OLE `DataObject` format is present
/// (Explorer), falls back to `read_file_list_ole` after closing the Win32
/// clipboard -- unless `try_ole` is false (D87d: the SYSTEM helper, where the
/// OLE read always fails).
fn read_file_list_impl(try_ole: bool) -> FileListRead {
    let none = |data_object_only| FileListRead {
        files: None,
        data_object_only,
    };
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
    let Some(guard) = opened else {
        return none(false);
    };

    let (formats, enum_error, has_data_object) = list_formats();
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
        // OLE must not be called with the Win32 clipboard open.
        drop(guard);
        let data_object_only = has_data_object;
        let files = if data_object_only && try_ole {
            read_file_list_ole()
        } else {
            None
        };
        return FileListRead {
            files,
            data_object_only,
        };
    }

    // Safety: the clipboard is open (held by `guard`). The returned handle
    // is owned by the clipboard and must not be freed or kept after close.
    let handle = match unsafe { GetClipboardData(u32::from(CF_HDROP.0)) } {
        Ok(handle) => handle,
        Err(err) => {
            tracing::warn!(error = %err, "CF_HDROP is on the clipboard but could not be read");
            return none(false);
        }
    };
    // `guard` stays alive until the end of the function, so the handle is
    // valid while `paths_from_hdrop` runs.
    let list = paths_from_hdrop(HDROP(handle.0));
    if list.is_empty() {
        tracing::warn!("CF_HDROP had no readable paths");
        return none(false);
    }
    tracing::info!(count = list.len(), "clipboard file list read");
    FileListRead {
        files: Some(list),
        data_object_only: false,
    }
}

/// How long the helper waits for the agent's entry for the current clipboard
/// change (D87d), and how often it looks.
const AGENT_FILES_TIMEOUT: Duration = Duration::from_secs(1);
const AGENT_FILES_STEP: Duration = Duration::from_millis(50);

pub struct WinClipboardBackend {
    // `arboard::Clipboard`'s Windows backend is a zero-sized `Clipboard(())`
    // (see arboard 3.6.1 `src/platform/windows.rs`), trivially `Send`, so a
    // single instance can be held here and reused across calls.
    clipboard: arboard::Clipboard,
    /// D87d: the console user's rcdesk data directory where the tray agent
    /// drops the file list (full-mode helper only); `None` otherwise.
    agent_files_dir: Option<PathBuf>,
}

impl WinClipboardBackend {
    pub fn new(agent_files_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        Ok(Self {
            clipboard: arboard::Clipboard::new()?,
            agent_files_dir,
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
        let read = read_file_list_impl(self.agent_files_dir.is_none());
        if read.files.is_some() {
            return read.files;
        }
        if !read.data_object_only {
            return None;
        }
        let dir: &Path = self.agent_files_dir.as_deref()?;
        // Safety: no preconditions -- a plain Win32 query.
        let seq = unsafe { GetClipboardSequenceNumber() };
        match agent_files::wait_for_seq(dir, u64::from(seq), AGENT_FILES_TIMEOUT, AGENT_FILES_STEP)
        {
            Some(list) => {
                tracing::info!(count = list.len(), "clipboard file list read from agent");
                Some(list)
            }
            None => {
                tracing::info!(seq, "no file list from agent for this clipboard change");
                None
            }
        }
    }

    fn set_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.clipboard.set_text(text)?;
        Ok(())
    }
}
