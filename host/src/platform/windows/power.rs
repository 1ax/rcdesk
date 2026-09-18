//! Keeps the display/system awake for the duration of an active remote
//! session (slice 2.6c) -- the Windows counterpart of
//! `platform::macos::activity`. `rcdesk-agent` has no visible window and no
//! direct user input while a session runs, so without this the OS may sleep
//! the display or the whole system out from under a live session.
//!
//! Must be called from the same thread every time (`SetThreadExecutionState`
//! is per-thread) -- `agent_main` calls both from its main thread, the one
//! driving `platform::windows::event_loop::pump`.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see `platform::windows::mouse`'s doc comment,
//! which documents and uses the same approach.

use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};

/// Call when a session becomes active: keeps the system and display from
/// sleeping until `end_session_activity` is called or the process exits.
pub fn begin_session_activity() {
    // Safety: `SetThreadExecutionState` has no preconditions beyond passing
    // a valid `EXECUTION_STATE` flag combination; `ES_CONTINUOUS` here means
    // "keep this state in effect until changed again", not just for this
    // one call.
    unsafe {
        let _ = SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
    }
}

/// Call when a session ends: clears the flags `begin_session_activity` set,
/// letting the system/display sleep normally again.
pub fn end_session_activity() {
    // Safety: same as `begin_session_activity` -- `ES_CONTINUOUS` alone
    // means "stop overriding the idle timers", the documented way to
    // release a previous `SetThreadExecutionState` call from the same
    // thread.
    unsafe {
        let _ = SetThreadExecutionState(ES_CONTINUOUS);
    }
}
