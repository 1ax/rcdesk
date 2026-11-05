//! System timer resolution for the video pipeline (audit V-3). The default
//! Windows timer tick is 15.6 ms, so `thread::sleep` in the capture pacer and
//! the 1 ms `tokio::time::sleep` between RTP pacing bursts get rounded up to
//! 15.6 ms. `timeBeginPeriod(1)` lowers the tick to ~1 ms while the guard lives.
//!
//! Signatures come from `windows` 0.61 (`Win32::Media`): both functions are
//! `unsafe fn(u32) -> u32` returning `TIMERR_NOERROR` (0) on success. Calls
//! are reference-counted by the OS, so every successful `timeBeginPeriod`
//! must be paired with a `timeEndPeriod` of the same value.

use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};

/// Requested timer period in milliseconds.
const PERIOD_MS: u32 = 1;
/// `TIMERR_NOERROR` from `mmsystem.h`.
const TIMERR_NOERROR: u32 = 0;

/// RAII guard: restores the timer resolution on drop (only if it was raised).
pub struct TimerResolution {
    active: bool,
}

/// Raises the system timer resolution to 1 ms until the guard is dropped.
/// Never fails hard: on error the guard is inert.
pub fn high_resolution() -> TimerResolution {
    // Safety: `timeBeginPeriod` takes a plain integer period and has no
    // pointer or lifetime preconditions.
    let rc = unsafe { timeBeginPeriod(PERIOD_MS) };
    let active = rc == TIMERR_NOERROR;
    if !active {
        tracing::debug!(code = rc, "timeBeginPeriod(1) failed, timer stays coarse");
    }
    TimerResolution { active }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.active {
            // Safety: pairs with the successful `timeBeginPeriod(PERIOD_MS)`
            // in `high_resolution`; same period value, no other preconditions.
            unsafe {
                let _ = timeEndPeriod(PERIOD_MS);
            }
        }
    }
}
