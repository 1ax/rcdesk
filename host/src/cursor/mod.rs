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

use tokio::sync::mpsc::{self, error::TrySendError};

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
    /// state to compare against). Delivery is the consumer's job: the
    /// watcher never resends a shape it already reported, so a consumer whose
    /// transport is not ready yet (the `control` channel opens after session
    /// start) must retry the latest state itself, see the cursor task in
    /// `crate::signaling`.
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

/// Applies the outcome of `try_send` to the dedup state. Returns `false`
/// when the receiver is gone and the loop should stop (V-7).
fn after_send(
    result: Result<(), TrySendError<CursorState>>,
    state: CursorState,
    last: &mut Option<CursorState>,
) -> bool {
    match result {
        Ok(()) => {
            *last = Some(state);
            true
        }
        // Channel full: leave `last` untouched so the next poll retries
        // instead of losing this shape for good.
        Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Closed(_)) => false,
    }
}

/// True the first time a given oversized (width, height) is seen: one
/// warning per size instead of one per poll.
fn should_warn_too_large(warned: &mut Option<(u32, u32)>, width: u32, height: u32) -> bool {
    if *warned == Some((width, height)) {
        return false;
    }
    *warned = Some((width, height));
    true
}

fn watch_loop(
    mut source: Box<dyn CursorSource>,
    period: Duration,
    tx: &mpsc::Sender<CursorState>,
    stop: &AtomicBool,
) {
    let mut last: Option<CursorState> = None;
    let mut warned_too_large: Option<(u32, u32)> = None;
    while !stop.load(Ordering::Relaxed) {
        if let Some(state) = source.current() {
            if let CursorState::Shape(image) = &state {
                if image.width > MAX_CURSOR_DIMENSION || image.height > MAX_CURSOR_DIMENSION {
                    if should_warn_too_large(&mut warned_too_large, image.width, image.height) {
                        tracing::warn!(
                            width = image.width,
                            height = image.height,
                            "cursor image too large for a CSS cursor, skipping"
                        );
                    }
                    thread::sleep(period);
                    continue;
                }
            }
            if last.as_ref() != Some(&state) {
                let result = tx.try_send(state.clone());
                if !after_send(result, state, &mut last) {
                    return;
                }
            }
        }
        thread::sleep(period);
    }
}

/// Something that can read the host OS's current pointer *position* (VM-2),
/// in the global coordinates of `crate::input::CaptureRect`/`DisplayInfo` --
/// the inverse of the injector's `to_pixel`. Called only on `watch_position`'s
/// dedicated thread, so a slow platform call never stalls the tokio runtime.
pub trait PositionSource: Send {
    /// Reads the pointer position. `None` means the read failed; the caller
    /// just skips this poll.
    fn position(&mut self) -> Option<(i32, i32)>;
}

/// A `PositionSource` for platforms with no real backend and for synthetic
/// runs: never reports a position.
#[derive(Debug, Default)]
pub struct NoopPositionSource;

impl NoopPositionSource {
    pub fn new() -> Self {
        Self
    }
}

impl PositionSource for NoopPositionSource {
    fn position(&mut self) -> Option<(i32, i32)> {
        None
    }
}

/// One pointer position read by `watch_position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSample {
    pub x: i32,
    pub y: i32,
    /// `true` for the first sample after polling was (re-)enabled: the
    /// consumer must send it even if the position did not change, so a
    /// viewer who just entered view mode gets the pointer at once.
    pub fresh: bool,
}

/// Owns the thread started by `watch_position`; dropping it stops the thread.
pub struct PositionWatcher {
    /// Every reading while `enabled` is set (no dedup here: the consumer
    /// compares *normalized* values, which also depend on the display rect).
    pub rx: mpsc::Receiver<PositionSample>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for PositionWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Starts a dedicated thread that reads `source.position()` every `period`
/// **only while `enabled` is set** (in control mode the platform is not
/// touched at all) and pushes each reading to the returned watcher's `rx`.
/// The first reading after `enabled` goes (back) up is marked `fresh`.
///
/// A separate thread from `watch` on purpose: the shape watcher's dedup and
/// size filter do not fit a position stream, and on Windows each thread syncs
/// itself to the input desktop (`platform::desktop::sync_thread`) anyway.
pub fn watch_position(
    source: Box<dyn PositionSource>,
    period: Duration,
    enabled: Arc<AtomicBool>,
) -> PositionWatcher {
    let (tx, rx) = mpsc::channel(2);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || position_loop(source, period, &enabled, &tx, &thread_stop));
    PositionWatcher {
        rx,
        stop,
        thread: Some(thread),
    }
}

fn position_loop(
    mut source: Box<dyn PositionSource>,
    period: Duration,
    enabled: &AtomicBool,
    tx: &mpsc::Sender<PositionSample>,
    stop: &AtomicBool,
) {
    let mut fresh = true;
    while !stop.load(Ordering::Relaxed) {
        if enabled.load(Ordering::Relaxed) {
            if let Some((x, y)) = source.position() {
                match tx.try_send(PositionSample { x, y, fresh }) {
                    Ok(()) => fresh = false,
                    // Channel full: keep `fresh` for the next poll.
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Closed(_)) => return,
                }
            }
        } else {
            fresh = true;
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
    fn after_send_ok_records_last() {
        let mut last = None;
        assert!(after_send(Ok(()), CursorState::Hidden, &mut last));
        assert_eq!(last, Some(CursorState::Hidden));
    }

    #[test]
    fn full_channel_keeps_shape_for_retry() {
        let mut last = None;
        let err = TrySendError::Full(CursorState::Hidden);
        assert!(after_send(Err(err), CursorState::Hidden, &mut last));
        assert_eq!(
            last, None,
            "last must not be updated so the send is retried"
        );
    }

    #[test]
    fn closed_channel_stops_the_loop() {
        let mut last = None;
        let err = TrySendError::Closed(CursorState::Hidden);
        assert!(!after_send(Err(err), CursorState::Hidden, &mut last));
        assert_eq!(last, None);
    }

    #[test]
    fn too_large_warning_is_logged_once_per_size() {
        let mut warned = None;
        assert!(should_warn_too_large(&mut warned, 256, 40));
        assert!(!should_warn_too_large(&mut warned, 256, 40));
        assert!(should_warn_too_large(&mut warned, 300, 40));
        assert!(!should_warn_too_large(&mut warned, 300, 40));
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

    /// Replays fixed positions (then `None`) and counts every `position()` call.
    struct FakePositionSource {
        positions: VecDeque<Option<(i32, i32)>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl PositionSource for FakePositionSource {
        fn position(&mut self) -> Option<(i32, i32)> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.positions.pop_front().flatten()
        }
    }

    fn fake_positions(
        positions: Vec<Option<(i32, i32)>>,
    ) -> (Box<FakePositionSource>, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Box::new(FakePositionSource {
                positions: positions.into(),
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }

    fn recv_positions(rx: &mut mpsc::Receiver<PositionSample>, n: usize) -> Vec<PositionSample> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut out = Vec::new();
        while out.len() < n && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(s) => out.push(s),
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        out
    }

    #[test]
    fn position_source_is_not_read_while_disabled() {
        let (source, calls) = fake_positions(vec![Some((1, 2)); 5]);
        let enabled = Arc::new(AtomicBool::new(false));
        let mut watcher = watch_position(source, Duration::from_millis(2), enabled);

        thread::sleep(Duration::from_millis(60));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "platform must stay untouched"
        );
        assert!(watcher.rx.try_recv().is_err());
    }

    #[test]
    fn first_position_after_enabling_is_fresh_and_later_ones_are_not() {
        let (source, _) = fake_positions(vec![Some((1, 2)), Some((3, 4))]);
        let enabled = Arc::new(AtomicBool::new(true));
        let mut watcher = watch_position(source, Duration::from_millis(2), enabled);

        let got = recv_positions(&mut watcher.rx, 2);
        assert_eq!(
            got,
            vec![
                PositionSample {
                    x: 1,
                    y: 2,
                    fresh: true
                },
                PositionSample {
                    x: 3,
                    y: 4,
                    fresh: false
                },
            ]
        );
    }

    #[test]
    fn re_enabling_marks_the_next_position_fresh_again() {
        let (source, _) = fake_positions(vec![Some((5, 6)); 100]);
        let enabled = Arc::new(AtomicBool::new(true));
        let mut watcher = watch_position(source, Duration::from_millis(2), Arc::clone(&enabled));
        assert!(recv_positions(&mut watcher.rx, 1)[0].fresh);

        enabled.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(40));
        while watcher.rx.try_recv().is_ok() {}
        enabled.store(true, Ordering::Relaxed);

        let got = recv_positions(&mut watcher.rx, 1);
        assert_eq!(got.len(), 1);
        assert!(
            got[0].fresh,
            "re-entering view mode starts with a fresh sample"
        );
    }

    #[test]
    fn failed_position_read_is_skipped() {
        let (source, _) = fake_positions(vec![None, Some((7, 8))]);
        let enabled = Arc::new(AtomicBool::new(true));
        let mut watcher = watch_position(source, Duration::from_millis(2), enabled);

        let got = recv_positions(&mut watcher.rx, 1);
        assert_eq!(
            got,
            vec![PositionSample {
                x: 7,
                y: 8,
                fresh: true
            }]
        );
    }

    #[test]
    fn noop_position_source_never_reports() {
        assert_eq!(NoopPositionSource::new().position(), None);
    }

    #[test]
    fn position_watcher_drop_stops_the_thread() {
        let (source, _) = fake_positions(vec![]);
        let watcher = watch_position(
            source,
            Duration::from_millis(2),
            Arc::new(AtomicBool::new(true)),
        );
        drop(watcher); // must not hang
    }
}
