//! Frame sources: something that produces raw video frames for the encode
//! stage. `scap` (macOS/Windows) is the real backend; `synthetic` is a
//! platform-independent generator used by tests, CI and local development on
//! machines without screen-recording permission.

use std::time::{Duration, Instant};

pub mod synthetic;

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod scap;

#[cfg(target_os = "windows")]
pub mod gdi;

/// A single raw (unencoded) video frame captured from a source.
///
/// `Nv12` is what `scap` hands us on macOS (biplanar 4:2:0, Y + interleaved
/// CbCr). `Bgra` is what `scap` hands us on Windows (packed BGRA8).
#[derive(Debug, Clone)]
pub enum RawFrame {
    Nv12 {
        width: u32,
        height: u32,
        y: Vec<u8>,
        y_stride: usize,
        uv: Vec<u8>,
        uv_stride: usize,
        ts: Instant,
    },
    Bgra {
        width: u32,
        height: u32,
        data: Vec<u8>,
        stride: usize,
        ts: Instant,
    },
}

impl RawFrame {
    /// Capture timestamp, used by the pipeline to track capture-to-encode
    /// latency (see ARCHITECTURE.md §12 latency targets).
    pub fn ts(&self) -> Instant {
        match self {
            RawFrame::Nv12 { ts, .. } | RawFrame::Bgra { ts, .. } => *ts,
        }
    }
}

/// Something that produces a sequence of raw frames, one call at a time.
///
/// Implementations may block inside `next_frame` (e.g. waiting for the next
/// screen update); callers are expected to run them on a dedicated thread.
pub trait FrameSource: Send {
    fn next_frame(&mut self) -> Result<RawFrame, CaptureError>;
    fn size(&self) -> (u32, u32);
}

/// A capturable display, as reported by the OS.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: u32,
    pub title: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen recording permission not granted")]
    PermissionDenied,
    #[error("screen capture is not supported on this platform")]
    Unsupported,
    #[error("capture backend error: {0}")]
    Backend(String),
    #[error("capture source stopped")]
    Stopped,
}

/// Lists capturable displays via `scap`. Requires screen-recording
/// permission to be granted already: this never prompts (there is nobody to
/// click the dialog on a headless/CI/terminal session), it just reports the
/// permission is missing.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    if !::scap::is_supported() {
        return Err(CaptureError::Unsupported);
    }
    if !::scap::has_permission() {
        return Err(CaptureError::PermissionDenied);
    }

    Ok(::scap::get_all_targets()
        .into_iter()
        .filter_map(|target| match target {
            ::scap::Target::Display(display) => Some(DisplayInfo {
                id: display.id,
                title: display.title,
            }),
            ::scap::Target::Window(_) => None,
        })
        .collect())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    Err(CaptureError::Unsupported)
}

/// Fixed-rate pacing shared by `FrameSource` implementations that must poll
/// on a timer instead of blocking on the OS for the next frame (used by
/// [`gdi::GdiSource`] -- `synthetic::SyntheticSource` has its own copy of
/// this exact logic since it predates this type and is out of scope here to
/// change; `scap::ScapSource` needs no pacer at all, `get_next_frame` blocks
/// until the OS has a frame ready).
pub struct FramePacer {
    next_tick: Instant,
    interval: Duration,
}

impl FramePacer {
    /// How many intervals behind schedule `wait` tolerates (catching up
    /// with shorter sleeps) before snapping the schedule to `now`.
    const CATCH_UP_LIMIT: u32 = 4;

    /// `fps` of 0 is treated as 1 (a single frame per second) rather than
    /// producing a zero/infinite interval.
    pub fn new(fps: u32) -> Self {
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        Self {
            next_tick: Instant::now(),
            interval,
        }
    }

    /// Blocks until the next scheduled tick, then advances the schedule by
    /// one interval.
    ///
    /// Ordinary lateness (an oversleeping `thread::sleep`, a slow capture
    /// call, a busy machine) is absorbed by the fixed schedule: the next few
    /// calls simply sleep less, so the average rate stays at `fps`. Only when
    /// the caller has fallen behind by more than `CATCH_UP_LIMIT` intervals
    /// (e.g. a source built long before its capture thread started) is the
    /// schedule snapped to `now` instead of advancing from the old baseline
    /// -- otherwise the next several calls would all return instantly in a
    /// burst of duplicate/stale frames to "catch up".
    pub fn wait(&mut self) {
        let now = Instant::now();
        if now < self.next_tick {
            std::thread::sleep(self.next_tick - now);
        } else if now - self.next_tick > self.interval * Self::CATCH_UP_LIMIT {
            self.next_tick = now;
        }
        self.next_tick += self.interval;
    }
}

/// `true` if `cur` is byte-for-byte identical to the previous frame, i.e.
/// nothing changed on screen and encoding it again would be wasted work.
/// `None` (no previous frame yet, e.g. the first frame) is never
/// "unchanged".
pub fn frame_unchanged(prev: Option<&[u8]>, cur: &[u8]) -> bool {
    matches!(prev, Some(p) if p == cur)
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    #[test]
    fn wait_paces_at_the_configured_fps() {
        let mut pacer = FramePacer::new(100); // 10ms interval
        let start = Instant::now();
        pacer.wait();
        pacer.wait();
        assert!(
            start.elapsed() >= Duration::from_millis(10),
            "two waits at 100fps should take at least one 10ms interval"
        );
    }

    #[test]
    fn wait_resets_schedule_after_falling_behind_instead_of_bursting() {
        let mut pacer = FramePacer::new(100); // 10ms interval
        pacer.wait();

        // Simulate the caller falling far behind (e.g. a slow capture call)
        // without the pacer's own knowledge.
        std::thread::sleep(Duration::from_millis(50));

        let before = Instant::now();
        pacer.wait();
        // Falling behind must not leave a backlog of already-due ticks: the
        // next scheduled tick has to be at or after "now", so the *next*
        // call to `wait()` sleeps again instead of returning instantly too.
        assert!(
            pacer.next_tick >= before,
            "pacer must not owe a backlog of ticks after falling behind"
        );
    }

    #[test]
    fn frame_unchanged_is_false_with_no_previous_frame() {
        assert!(!frame_unchanged(None, &[1, 2, 3]));
    }

    #[test]
    fn frame_unchanged_is_true_for_identical_bytes() {
        let prev = vec![1u8, 2, 3, 4];
        let cur = vec![1u8, 2, 3, 4];
        assert!(frame_unchanged(Some(&prev), &cur));
    }

    #[test]
    fn frame_unchanged_is_false_when_a_byte_differs() {
        let prev = vec![1u8, 2, 3, 4];
        let cur = vec![1u8, 2, 3, 5];
        assert!(!frame_unchanged(Some(&prev), &cur));
    }
}
