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
//! Signatures added in D61 (file operations by path run under the logged-on
//! user's token), checked in `windows` 0.61.3:
//!
//! ```text
//! // Security (feature Win32_Security)
//! pub unsafe fn ImpersonateLoggedOnUser(htoken: HANDLE) -> Result<()>
//! pub unsafe fn RevertToSelf() -> Result<()>
//! // System/Threading (feature Win32_System_Threading); TOKEN_ACCESS_MASK is
//! // `windows::Win32::Security::TOKEN_ACCESS_MASK`
//! pub unsafe fn OpenProcessToken(processhandle: HANDLE, desiredaccess: TOKEN_ACCESS_MASK, tokenhandle: *mut HANDLE) -> Result<()>
//! pub unsafe fn OpenThreadToken(threadhandle: HANDLE, desiredaccess: TOKEN_ACCESS_MASK, openasself: bool, tokenhandle: *mut HANDLE) -> Result<()>
//! pub unsafe fn GetCurrentProcess() -> HANDLE
//! pub unsafe fn GetCurrentThread() -> HANDLE
//! // Foundation
//! pub const ERROR_NO_TOKEN: WIN32_ERROR   // = 1008
//! // windows_result::HRESULT
//! pub const fn from_win32(error: u32) -> HRESULT
//! ```
//!
//! `ImpersonateLoggedOnUser` accepts a primary token (it duplicates it into
//! an impersonation token; needs `TOKEN_QUERY | TOKEN_DUPLICATE`) or an
//! impersonation token (`TOKEN_QUERY | TOKEN_IMPERSONATE`).
//!
//! `HANDLE` implements `windows_core::Free`, so the user token lives in
//! `Owned<HANDLE>` and is closed on drop.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;

use windows::core::Owned;
use windows::core::GUID;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    FOLDERID_Downloads, FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};

/// The Downloads folder (`FOLDERID_Downloads`). With `console_user == false`
/// it belongs to the current process's user; with `true` (full-mode helper
/// running as SYSTEM) to the user logged on to this process's session, and
/// an error means nobody is logged on there.
pub fn downloads_dir(console_user: bool) -> anyhow::Result<PathBuf> {
    known_folder(&FOLDERID_Downloads, "Downloads", console_user)
}

/// `%LOCALAPPDATA%` (`FOLDERID_LocalAppData`), with the same `console_user`
/// meaning as [`downloads_dir`] (D87d).
pub fn local_app_data_dir(console_user: bool) -> anyhow::Result<PathBuf> {
    known_folder(&FOLDERID_LocalAppData, "LocalAppData", console_user)
}

/// A known folder of the current user (`console_user == false`) or of the
/// user logged on to this process's session (`true`). `name` is only for
/// the error message.
fn known_folder(id: &GUID, name: &str, console_user: bool) -> anyhow::Result<PathBuf> {
    let token = if console_user {
        Some(console_user_token()?)
    } else {
        None
    };
    // `None` means "the current user".
    let htoken = token.as_ref().map(|t| **t);
    // Safety: `id` is a valid GUID reference; `htoken` is `None` or a live
    // token owned by `token` above. On success the returned string is
    // allocated by the shell with `CoTaskMemAlloc` and must be freed with
    // `CoTaskMemFree`, which happens below on every path after success.
    let pwstr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, htoken) }
        .map_err(|err| anyhow::anyhow!("SHGetKnownFolderPath({name}) failed: {err}"))?;
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

/// File operations by path under the rights of the user logged on to this
/// process's session (D61). An error means nobody is logged on.
pub fn console_user_fs() -> anyhow::Result<super::FsUser> {
    Ok(token_fs_user(console_user_token()?))
}

/// An [`FsUser`](super::FsUser) that impersonates the owner of `token`.
pub(crate) fn token_fs_user(token: Owned<HANDLE>) -> super::FsUser {
    super::FsUser::from_scope(Arc::new(TokenScope { token }))
}

thread_local! {
    /// This thread is inside a [`TokenScope`]. `RevertToSelf` drops ALL
    /// impersonation, so a nested scope must not revert: it just runs.
    static IN_SCOPE: Cell<bool> = const { Cell::new(false) };
}

struct TokenScope {
    token: Owned<HANDLE>,
}

// Safety: a token handle is a kernel object handle usable from any thread;
// `TokenScope` only passes it by value to `ImpersonateLoggedOnUser` and
// closes it on drop (once, with the owning `Arc`).
unsafe impl Send for TokenScope {}
unsafe impl Sync for TokenScope {}

/// Reverts the thread to the process token on drop, also on a panic in the
/// operation.
struct RevertGuard;

impl Drop for RevertGuard {
    fn drop(&mut self) {
        IN_SCOPE.with(|s| s.set(false));
        // Safety: no arguments; affects only the calling thread.
        if let Err(err) = unsafe { RevertToSelf() } {
            // A pool thread left under the user's token (or worse, with
            // an unknown token) would later serve unrelated work with the
            // wrong rights. Dying is the only safe answer.
            tracing::error!(%err, "RevertToSelf failed, aborting the process");
            std::process::abort();
        }
    }
}

impl super::UserScope for TokenScope {
    fn scope(&self, f: &mut dyn FnMut()) -> std::io::Result<()> {
        if IN_SCOPE.with(Cell::get) {
            f();
            return Ok(());
        }
        // Safety: `self.token` is a live token handle owned by `self`.
        unsafe { ImpersonateLoggedOnUser(*self.token) }.map_err(|err| {
            std::io::Error::other(format!("ImpersonateLoggedOnUser failed: {err}"))
        })?;
        IN_SCOPE.with(|s| s.set(true));
        let _guard = RevertGuard;
        f();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::HRESULT;
    use windows::Win32::Foundation::ERROR_NO_TOKEN;
    use windows::Win32::Security::{TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
    };

    fn thread_is_impersonating() -> bool {
        let mut raw = HANDLE::default();
        // Safety: `raw` is a valid out pointer; the pseudo handle of the
        // current thread needs no closing.
        let res = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut raw) };
        match res {
            Ok(()) => {
                // Safety: a new handle that we own; closed on drop.
                drop(unsafe { Owned::new(raw) });
                true
            }
            Err(err) => {
                assert_eq!(err.code(), HRESULT::from_win32(ERROR_NO_TOKEN.0));
                false
            }
        }
    }

    #[test]
    fn scope_impersonates_inside_and_reverts_after() {
        let mut raw = HANDLE::default();
        // Safety: `raw` is a valid out pointer; the process pseudo handle
        // needs no closing.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
                &mut raw,
            )
        }
        .unwrap();
        // Safety: a new handle that we own.
        let user = token_fs_user(unsafe { Owned::new(raw) });
        assert!(!thread_is_impersonating());
        let inside = user.run(|| Ok(thread_is_impersonating())).unwrap();
        assert!(inside, "thread must impersonate inside run");
        assert!(!thread_is_impersonating(), "thread must revert after run");
        // A nested scope must not revert the outer one early.
        let nested = user
            .run(|| {
                user.run(|| Ok(()))?;
                Ok(thread_is_impersonating())
            })
            .unwrap();
        assert!(nested);
        assert!(!thread_is_impersonating());
    }
}
