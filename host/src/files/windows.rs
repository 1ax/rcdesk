//! Windows download folder lookup for incoming files (slice 3.4a).
//!
//! Compiles only on `windows-latest` in CI: signatures of
//! `SHGetKnownFolderPath`/`CoTaskMemFree`/`PWSTR::to_string` below come from
//! reading the `windows` 0.61.3 / `windows-strings` 0.4 sources in the local
//! registry cache, not from memory.

use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

/// The current user's Downloads folder (`FOLDERID_Downloads`).
pub fn downloads_dir() -> anyhow::Result<PathBuf> {
    // Safety: `FOLDERID_Downloads` is a valid GUID constant; a null token
    // means "the current user". On success the returned string is allocated
    // by the shell with `CoTaskMemAlloc` and must be freed with
    // `CoTaskMemFree`, which happens below on every path after success.
    let pwstr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) }
        .map_err(|err| anyhow::anyhow!("SHGetKnownFolderPath(Downloads) failed: {err}"))?;
    // Safety: `pwstr` is a valid NUL-terminated UTF-16 string returned just
    // above and not yet freed.
    let path = unsafe { pwstr.to_string() };
    // Safety: `pwstr.0` was allocated by the shell via `CoTaskMemAlloc` and
    // is freed exactly once, here; `path` already holds an owned copy.
    unsafe { CoTaskMemFree(Some(pwstr.0 as *const core::ffi::c_void)) };
    Ok(PathBuf::from(path?))
}
