//! Detects whether the Windows foreground window belongs to a process
//! running at a higher integrity level than this one (slice 2.6e) -- UIPI
//! (User Interface Privilege Isolation) then silently discards any
//! `SendInput` event aimed at it, no error returned to the caller, no log
//! anywhere. See `docs/host-windows.md` ("Окна с правами администратора")
//! and `proto::control::ControlMessage::InputBlocked`, which this feeds.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes
//! from reading the `windows` 0.61.3 source fetched into the local
//! registry cache, not from memory -- see executor rule 3 (the same
//! approach `platform::windows::keyboard`/`mouse`/`monitors` document and
//! use).

use std::sync::OnceLock;

use windows::core::Owned;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, OpenProcessToken,
    TokenIntegrityLevel, PSID, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// Reads process `pid`'s integrity level, as the last RID (sub-authority)
/// of its token's `TokenIntegrityLevel` label SID (`S-1-16-X`, X e.g.
/// `0x1000` low, `0x2000` medium, `0x3000` high, `0x4000` system) --
/// higher RID means higher privilege. `None` on any failure: denied access
/// opening the process/token, or a malformed label (zero sub-authorities).
/// The caller decides what a failure means; see `foreground_input_blocked`,
/// which treats "can't inspect the *foreground* window's process" itself as
/// a sign of elevation.
fn integrity_level_of(pid: u32) -> Option<u32> {
    // Safety: `PROCESS_QUERY_LIMITED_INFORMATION` is enough to open a
    // process's token for a read-only query (works even across an
    // elevation boundary, unlike `PROCESS_QUERY_INFORMATION`); wrapping the
    // returned `HANDLE` in `Owned` closes it via `HANDLE`'s `Free` impl
    // when it goes out of scope, on every return path below.
    let process: Owned<HANDLE> =
        unsafe { Owned::new(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?) };

    let mut token_handle = HANDLE::default();
    // Safety: `process` is a valid, still-open process handle; `token_handle`
    // is a valid out-parameter for the new token handle.
    unsafe { OpenProcessToken(*process, TOKEN_QUERY, &mut token_handle).ok()? };
    let token: Owned<HANDLE> = unsafe { Owned::new(token_handle) };

    let mut needed = 0u32;
    // Safety: a `None` buffer with a real `returnlength` out-param is
    // `GetTokenInformation`'s documented way to query the required buffer
    // size; this call is expected to itself return `Err`
    // (`ERROR_INSUFFICIENT_BUFFER`), only `needed` matters.
    let _ = unsafe { GetTokenInformation(*token, TokenIntegrityLevel, None, 0, &mut needed) };
    if needed == 0 {
        return None;
    }

    // `TOKEN_MANDATORY_LABEL` holds a `PSID` (pointer-sized) field, so the
    // buffer needs pointer alignment -- a `Vec<u8>` would only guarantee
    // 1-byte alignment when cast to `*const TOKEN_MANDATORY_LABEL` below,
    // which is undefined behavior. A `Vec<u64>` sized in 8-byte words gives
    // 8-byte alignment, matching a 64-bit `PSID`.
    let words = (needed as usize).div_ceil(8);
    let mut buf: Vec<u64> = vec![0u64; words];
    let mut written = 0u32;
    // Safety: `buf` has at least `needed` bytes, matching
    // `tokeninformationlength`'s contract; `token` is still valid.
    unsafe {
        GetTokenInformation(
            *token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut written,
        )
        .ok()?;
    }

    // Safety: `buf` was just filled by `GetTokenInformation` above with a
    // `TOKEN_MANDATORY_LABEL` (plus its trailing SID data), sized and
    // aligned as that call requires.
    let label = unsafe { &*(buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
    let sid: PSID = label.Label.Sid;

    // Safety: `sid` points into `buf`, still alive; both calls are
    // documented to accept any valid `PSID`.
    let sub_authority_count = unsafe { *GetSidSubAuthorityCount(sid) };
    if sub_authority_count == 0 {
        return None;
    }
    let rid = unsafe { *GetSidSubAuthority(sid, u32::from(sub_authority_count - 1)) };
    Some(rid)
}

/// This process's own integrity level, queried once and cached -- it never
/// changes at runtime. `None` only if the query somehow fails for our own
/// process (should not happen in practice); treated the same as "can't
/// tell" by `foreground_input_blocked`, which then reports `false` rather
/// than risk a false alarm from a bug in this check itself.
fn own_integrity_level() -> Option<u32> {
    static OWN: OnceLock<Option<u32>> = OnceLock::new();
    *OWN.get_or_init(|| integrity_level_of(std::process::id()))
}

/// `true` when the current Windows foreground window belongs to a process
/// running at a higher integrity level than this one -- e.g. Task Manager
/// or another app opened "Run as administrator" while this agent runs
/// unelevated. In that case `SendInput` calls this host makes still report
/// success, but UIPI drops every event aimed at that window before it ever
/// reaches it (see the module doc comment).
///
/// Returns `false` (don't warn) for: no foreground window (`hwnd` is null),
/// a `GetWindowThreadProcessId` failure, and the foreground window
/// belonging to this very process. Returns `true` (warn) whenever the
/// foreground process's integrity level can't even be determined --
/// `OpenProcess`/`OpenProcessToken`/`GetTokenInformation` denied access is
/// itself a symptom of the other process running with more privilege than
/// this one has to look at it.
pub fn foreground_input_blocked() -> bool {
    // Safety: takes no arguments, never fails (returns null on no
    // foreground window).
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return false;
    }

    let mut pid = 0u32;
    // Safety: `hwnd` was just obtained above; `pid` is a valid out-param.
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if thread_id == 0 || pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return false;
    }

    let Some(own_level) = own_integrity_level() else {
        return false;
    };
    match integrity_level_of(pid) {
        Some(level) => level > own_level,
        None => true,
    }
}
