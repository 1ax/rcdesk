//! Windows download folder lookup for incoming files (slice 3.4a).
//!
//! Compiles only on `windows-latest` in CI: signatures of
//! `SHGetKnownFolderPath`/`CoTaskMemFree`/`PWSTR::to_string` below come from
//! reading the `windows` 0.61.3 / `windows-strings` 0.4 sources in the local
//! registry cache, not from memory.
//!
//! Signatures added in 4.1d (full-mode helper runs as SYSTEM in the console
//! session, so its own token would give the SYSTEM profile's Downloads):
//!
//! ```text
//! // System/RemoteDesktop (feature Win32_System_RemoteDesktop)
//! pub unsafe fn ProcessIdToSessionId(dwprocessid: u32, psessionid: *mut u32) -> Result<()>
//! pub unsafe fn WTSQueryUserToken(sessionid: u32, phtoken: *mut HANDLE) -> Result<()>
//! // System/Threading (feature Win32_System_Threading)
//! pub unsafe fn GetCurrentProcessId() -> u32
//! // UI/Shell (feature Win32_UI_Shell)
//! pub unsafe fn SHGetKnownFolderPath(rfid: *const GUID, dwflags: KNOWN_FOLDER_FLAG, htoken: Option<HANDLE>) -> Result<PWSTR>
//! ```
//!
//! `HANDLE` implements `windows_core::Free`, so the user token lives in
//! `Owned<HANDLE>` and is closed on drop.

use std::path::PathBuf;

use windows::core::Owned;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

/// The Downloads folder (`FOLDERID_Downloads`). With `console_user == false`
/// it belongs to the current process's user; with `true` (full-mode helper
/// running as SYSTEM) to the user logged on to this process's session, and
/// an error means nobody is logged on there.
pub fn downloads_dir(console_user: bool) -> anyhow::Result<PathBuf> {
    let token = if console_user {
        Some(console_user_token()?)
    } else {
        None
    };
    // `None` means "the current user".
    let htoken = token.as_ref().map(|t| **t);
    // Safety: `FOLDERID_Downloads` is a valid GUID constant; `htoken` is
    // `None` or a live token owned by `token` above. On success the returned string is allocated
    // by the shell with `CoTaskMemAlloc` and must be freed with
    // `CoTaskMemFree`, which happens below on every path after success.
    let pwstr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, htoken) }
        .map_err(|err| anyhow::anyhow!("SHGetKnownFolderPath(Downloads) failed: {err}"))?;
    // Safety: `pwstr` is a valid NUL-terminated UTF-16 string returned just
    // above and not yet freed.
    let path = unsafe { pwstr.to_string() };
    // Safety: `pwstr.0` was allocated by the shell via `CoTaskMemAlloc` and
    // is freed exactly once, here; `path` already holds an owned copy.
    unsafe { CoTaskMemFree(Some(pwstr.0 as *const core::ffi::c_void)) };
    Ok(PathBuf::from(path?))
}

/// Primary token of the user logged on to this process's session.
fn console_user_token() -> anyhow::Result<Owned<HANDLE>> {
    const NOBODY: &str = "в консольной сессии никто не вошёл";
    let mut session = 0u32;
    // Safety: `session` is a valid out pointer for the call.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
        .map_err(|err| anyhow::anyhow!("{NOBODY} (ProcessIdToSessionId: {err})"))?;
    let mut raw = HANDLE::default();
    // Safety: `raw` is a valid out pointer; the caller must be SYSTEM
    // (SeTcbPrivilege), which the full-mode helper is.
    unsafe { WTSQueryUserToken(session, &mut raw) }
        .map_err(|err| anyhow::anyhow!("{NOBODY} (WTSQueryUserToken: {err})"))?;
    // Safety: on success `raw` is a new token handle that we own and close
    // exactly once, on drop of the `Owned`.
    Ok(unsafe { Owned::new(raw) })
}
