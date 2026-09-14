//! Sends absolute pointer moves via `SendInput`, bypassing `enigo`.
//!
//! `enigo` 0.6.1's `Mouse::move_mouse` (`Coordinate::Abs` branch, in
//! `win_impl.rs`) normalizes the target position against
//! `self.main_display()`, which is `GetSystemMetrics(SM_CXSCREEN)` /
//! `SM_CYSCREEN` -- the *primary* monitor's size only -- and sets only
//! `MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE`, with the implementation's own
//! comment noting the gap:
//!
//! ```text
//! // TODO: Check if we should use MOUSEEVENTF_VIRTUALDESK too
//! (MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, x as i32, y as i32)
//! ```
//!
//! Without `MOUSEEVENTF_VIRTUALDESK`, an absolute coordinate is normalized by
//! Windows against the primary monitor's rectangle, so any point outside it
//! (a captured display to the left/above/right of a non-origin primary, or
//! any monitor when the primary isn't first) resolves to the wrong pixel or
//! clamps onto the primary screen. This module normalizes against the full
//! virtual desktop (`SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN`/
//! `SM_CXVIRTUALSCREEN`/`SM_CYVIRTUALSCREEN`) and sets
//! `MOUSEEVENTF_VIRTUALDESK` so `SendInput` places the pointer correctly on
//! any monitor.
//!
//! Compiles only on `windows-latest` in CI: there is no Windows machine
//! available to this executor run, so every signature/type below comes from
//! reading the `windows` 0.61.3 source fetched into the local registry cache,
//! not from memory -- see executor rule 3 (the same approach
//! `platform::windows::keyboard` documents and uses).

use std::mem::size_of;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// Maps `value` (in `[origin, origin + extent))`) to the `[0, 65535]` range
/// `MOUSEEVENTF_ABSOLUTE` expects, rounding to the nearest representable
/// point (see the `MOUSE_EVENT` remarks this mirrors in
/// `enigo`'s `move_mouse`). Returns `0` for a degenerate `extent <= 1`
/// instead of dividing by zero -- callers are not expected to hit this in
/// practice (`GetSystemMetrics` reports a real virtual desktop size), it's
/// only a guard.
fn normalize(value: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let value = i64::from(value - origin);
    let span = i64::from(extent - 1);
    ((value * 65535 + span.max(0) / 2) / span.max(1)) as i32
}

/// Moves the pointer to an absolute position `(x, y)` in virtual-desktop
/// pixel coordinates (the same units `capture::DisplayInfo`'s `x`/`y`/
/// `width`/`height` use on Windows), via `SendInput` with
/// `MOUSEEVENTF_VIRTUALDESK` so any monitor -- not just the primary one -- is
/// reachable.
pub fn move_to(x: i32, y: i32) -> Result<(), windows::core::Error> {
    // Safety: `GetSystemMetrics` has no preconditions; every `SM_*` index
    // used here is a valid `SYSTEM_METRICS_INDEX`.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };

    let dx = normalize(x, vx, vw);
    let dy = normalize(y, vy, vh);

    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    // Safety: `input` is a single, fully-initialized `INPUT` of the
    // `INPUT_MOUSE` variant; `SendInput` reads it and does not retain the
    // pointer past the call.
    let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    if sent != 1 {
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_maps_origin_to_zero() {
        assert_eq!(normalize(0, 0, 1920), 0);
    }

    #[test]
    fn normalize_maps_last_pixel_to_max() {
        assert_eq!(normalize(1919, 0, 1920), 65535);
    }

    #[test]
    fn normalize_offsets_by_origin_for_a_secondary_monitor() {
        // value=2420, origin=1920, extent=1000 -> value-origin=500, span=999.
        // (500 * 65535 + 999/2) / 999 = (32_767_500 + 499) / 999
        //                              = 32_767_999 / 999 = 32800 (floor).
        assert_eq!(normalize(1920 + 500, 1920, 1000), 32800);
    }

    #[test]
    fn normalize_handles_a_negative_origin() {
        assert_eq!(normalize(-100, -100, 1080), 0);
    }

    #[test]
    fn normalize_returns_zero_for_a_degenerate_extent() {
        assert_eq!(normalize(5, 0, 1), 0);
    }
}
