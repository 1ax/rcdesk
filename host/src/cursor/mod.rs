//! System cursor *shape* tracking (see ARCHITECTURE.md §7 and slice 1.5):
//! the host watches the current system cursor image/hotspot/visibility on a
//! dedicated thread -- like `crate::input`'s `InputRouter`, so a slow or
//! blocking platform call never stalls the tokio runtime -- and reports
//! every change to its owner (`crate::signaling`), which base64-encodes it
//! into a `proto::control::ControlMessage::CursorShape`/`CursorHidden` and
//! sends it down the `control` data channel. The client draws it with CSS
//! `cursor: url(...)`; the browser's own OS-cursor keeps moving with zero
//! added latency, only its *shape* needs to travel over the wire.
//!
//! Screen capture itself already excludes the cursor (`show_cursor: false`,
//! see `crate::capture`), so this module is the only source of cursor shape
//! information -- it does not touch pixels captured for the video track.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// A decoded cursor image, in device pixels.
///
/// `hotspot_x`/`hotspot_y` are in *logical* points (matching `scale`: a
/// `scale` of `2.0` means the image is twice as large, in pixels, as its
/// logical point size). `rgba` is `width * height * 4` bytes, straight
/// (non-premultiplied) alpha is not required -- see
/// `proto::control::ControlMessage::CursorShape`'s doc comment.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: f64,
    pub hotspot_y: f64,
    pub scale: f64,
    pub rgba: Vec<u8>,
}

/// The system cursor's current state, as read by a `CursorSource`.
#[derive(Debug, Clone, PartialEq)]
pub enum CursorState {
    Shape(CursorImage),
    /// The system cursor is hidden (e.g. the OS hid it while the user is
    /// typing).
    Hidden,
}

/// Something that can read the host OS's current system cursor shape.
///
/// Implementations do their own platform calls synchronously -- `watch`
/// below is what keeps them off the async runtime, by polling on a
/// dedicated thread, the same pattern `crate::input::Injector` uses.
pub trait CursorSource: Send {
    /// Reads the current cursor state. `None` means the read failed (a
    /// platform call errored); the caller should keep showing whatever
    /// shape it last had, not treat this as "cursor gone".
    fn current(&mut self) -> Option<CursorState>;
}

/// A `CursorSource` for platforms with no real backend (anything but
/// macOS/Windows), or when no platform read is wanted. Always reports "no
/// read" -- callers keep showing the client's default cursor.
#[derive(Debug, Default)]
pub struct NoopCursorSource;

impl NoopCursorSource {
    pub fn new() -> Self {
        Self
    }
}

impl CursorSource for NoopCursorSource {
    fn current(&mut self) -> Option<CursorState> {
        None
    }
}

/// A CSS `cursor: url(...)` image can't show anything sensibly this large;
/// `watch` drops (with a `warn`) any `CursorState::Shape` bigger than this
/// in either dimension, in pixels, rather than forwarding it.
const MAX_CURSOR_DIMENSION: u32 = 128;

/// Owns the dedicated thread started by `watch`. Dropping it stops the
/// thread (mirrors `crate::input::InputRouter`'s `Drop`).
pub struct CursorWatcher {
    /// Every *change* in cursor state, most recent last. The first state a
    /// fresh watcher reads always counts as a change (there's no prior
    /// state to compare against), so a newly connected client gets the
    /// current shape immediately rather than waiting for the cursor to
    /// actually move to a different shape.
    pub rx: mpsc::Receiver<CursorState>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for CursorWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Starts a dedicated thread that polls `source.current()` every `period`
/// and pushes every *change* (by `PartialEq` on `CursorState`/`CursorImage`
/// -- comparing every field, including the raw pixels, is simplest and
/// cheap enough at cursor-image sizes) onto the returned watcher's `rx`.
///
/// The channel has capacity 4 and uses `try_send`: if a consumer falls
/// behind and it fills up, the oldest-not-yet-read change is effectively
/// superseded by whichever one the consumer gets to next, and a dropped
/// change is harmless -- the next actual change (including the cursor
/// eventually settling back on a shape already in flight) will be sent too.
pub fn watch(source: Box<dyn CursorSource>, period: Duration) -> CursorWatcher {
    let (tx, rx) = mpsc::channel(4);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || watch_loop(source, period, &tx, &thread_stop));
    CursorWatcher {
        rx,
        stop,
        thread: Some(thread),
    }
}

fn watch_loop(
    mut source: Box<dyn CursorSource>,
    period: Duration,
    tx: &mpsc::Sender<CursorState>,
    stop: &AtomicBool,
) {
    let mut last: Option<CursorState> = None;
    while !stop.load(Ordering::Relaxed) {
        if let Some(state) = source.current() {
            if let CursorState::Shape(image) = &state {
                if image.width > MAX_CURSOR_DIMENSION || image.height > MAX_CURSOR_DIMENSION {
                    tracing::warn!(
                        width = image.width,
                        height = image.height,
                        "cursor image too large for a CSS cursor, skipping"
                    );
                    thread::sleep(period);
                    continue;
                }
            }
            if last.as_ref() != Some(&state) {
                // Best-effort: a full channel means the consumer is behind
                // and will catch up on the next change (see `watch`'s doc
                // comment).
                let _ = tx.try_send(state.clone());
                last = Some(state);
            }
        }
        thread::sleep(period);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Instant;

    fn image(width: u32, height: u32) -> CursorImage {
        CursorImage {
            width,
            height,
            hotspot_x: 1.0,
            hotspot_y: 2.0,
            scale: 1.0,
            rgba: vec![0u8; (width * height * 4) as usize],
        }
    }

    /// A `CursorSource` that replays a fixed sequence of `current()` results
    /// (`None` past the end, matching a real source's "no read" contract).
    /// `watch` owns its `Box<dyn CursorSource>` exclusively on the watcher
    /// thread, so no synchronization is needed here.
    struct FakeCursorSource {
        states: VecDeque<Option<CursorState>>,
    }

    impl FakeCursorSource {
        fn new(states: Vec<Option<CursorState>>) -> Self {
            Self {
                states: states.into(),
            }
        }
    }

    impl CursorSource for FakeCursorSource {
        fn current(&mut self) -> Option<CursorState> {
            self.states.pop_front().flatten()
        }
    }

    /// Receives from `rx` until `n` messages arrive or a generous timeout
    /// elapses, then returns whatever arrived (possibly fewer than `n`).
    fn recv_n(rx: &mut mpsc::Receiver<CursorState>, n: usize) -> Vec<CursorState> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut out = Vec::new();
        while out.len() < n && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(state) => out.push(state),
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        out
    }

    /// Nothing else arrives within a short grace period -- used to assert an
    /// upper bound on message count without waiting out the full timeout in
    /// `recv_n`.
    fn assert_no_more(rx: &mut mpsc::Receiver<CursorState>) {
        thread::sleep(Duration::from_millis(50));
        assert!(
            rx.try_recv().is_err(),
            "expected no further cursor state changes"
        );
    }

    #[test]
    fn identical_states_are_sent_only_once() {
        let shape = CursorState::Shape(image(16, 16));
        let states = std::iter::repeat_n(Some(shape.clone()), 5).collect();
        let mut watcher = watch(
            Box::new(FakeCursorSource::new(states)),
            Duration::from_millis(2),
        );

        let received = recv_n(&mut watcher.rx, 1);
        assert_eq!(received, vec![shape]);
        assert_no_more(&mut watcher.rx);
    }

    #[test]
    fn hidden_is_delivered_as_a_change() {
        let shape = CursorState::Shape(image(16, 16));
        let states = vec![Some(shape.clone()), Some(CursorState::Hidden)];
        let mut watcher = watch(
            Box::new(FakeCursorSource::new(states)),
            Duration::from_millis(2),
        );

        let received = recv_n(&mut watcher.rx, 2);
        assert_eq!(received, vec![shape, CursorState::Hidden]);
    }

    #[test]
    fn oversized_images_are_skipped() {
        let oversized = CursorState::Shape(image(256, 40));
        let states = vec![Some(oversized)];
        let mut watcher = watch(
            Box::new(FakeCursorSource::new(states)),
            Duration::from_millis(2),
        );

        assert_no_more(&mut watcher.rx);
    }

    #[test]
    fn no_read_keeps_previous_state_without_resending() {
        let shape = CursorState::Shape(image(16, 16));
        let states = vec![Some(shape.clone()), None, None, Some(shape.clone())];
        let mut watcher = watch(
            Box::new(FakeCursorSource::new(states)),
            Duration::from_millis(2),
        );

        let received = recv_n(&mut watcher.rx, 1);
        assert_eq!(received, vec![shape]);
        assert_no_more(&mut watcher.rx);
    }

    #[test]
    fn noop_source_never_reports_a_state() {
        let mut source = NoopCursorSource::new();
        assert_eq!(source.current(), None);
    }

    #[test]
    fn drop_stops_the_thread() {
        let states = vec![Some(CursorState::Hidden)];
        let watcher = watch(
            Box::new(FakeCursorSource::new(states)),
            Duration::from_millis(2),
        );
        drop(watcher); // must not hang
    }
}
