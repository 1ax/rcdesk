//! `rcdesk-agent`'s own minimal main-thread event loop (slice 2.6c) --
//! Windows counterpart of `platform::macos::event_loop`. No winit/tao (see
//! `host/Cargo.toml`'s `tray-icon` entry): `MsgWaitForMultipleObjects` parks
//! the thread until either a window message arrives or `timeout` elapses,
//! then an ordinary `PeekMessageW`/`TranslateMessage`/`DispatchMessageW`
//! drain follows -- the standard shape of a Win32 message loop with
//! `GetMessageW`'s unconditional block replaced by a bounded wait, since
//! `agent_main` also has to poll agent status/permissions between pumps
//! instead of blocking forever.
//!
//! Unlike macOS there is no separate `init()`: `tray-icon`'s Windows backend
//! creates its own hidden window and only needs *a* message loop pumping on
//! the same thread afterwards, not any particular setup beforehand.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry
//! cache, not from memory -- see `platform::windows::mouse`'s doc comment,
//! which documents and uses the same approach.

use std::time::Duration;

use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    QS_ALLINPUT,
};

/// Waits up to `timeout` for the next window message, then drains and
/// dispatches every message currently queued without blocking further.
/// Mirrors `platform::macos::event_loop::pump`'s "wait for the first one,
/// then drain the rest" shape.
pub fn pump(timeout: Duration) {
    // Safety: no handles are passed (`phandles: None`), so there is nothing
    // for `MsgWaitForMultipleObjects` to validate beyond the plain integer
    // arguments; it has no other preconditions.
    unsafe {
        let _ = MsgWaitForMultipleObjects(
            None,
            false,
            timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            QS_ALLINPUT,
        );
    }

    let mut msg = MSG::default();
    loop {
        // Safety: `msg` is a valid, exclusively-owned `MSG` for the
        // duration of the call; `hwnd: None` means "any window belonging to
        // this thread", the ordinary top-level message-loop usage.
        let has_message = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
        if !has_message.as_bool() {
            break;
        }
        // Safety: `msg` was just filled in by the successful `PeekMessageW`
        // above.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
