//! Wires a `FrameSource` and an `Encoder` together into a running pipeline:
//! one thread captures, one thread converts + encodes, and encoded frames
//! come out through a bounded channel. No network/transport here (that is
//! slice 1.2b) -- this just proves the video path end to end.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::capture::{FrameSource, RawFrame};
use crate::encode::{EncodedFrame, Encoder, RateTarget};

/// How many encoded frames may sit in the output channel before new ones get
/// dropped. Keeps memory/latency bounded when nothing is draining the
/// channel yet (no transport in this slice).
const FRAME_CHANNEL_CAPACITY: usize = 4;

/// How long the encode thread waits on the condvar between polls of the
/// stop flag, so `stop()` doesn't have to wait for a whole frame interval.
const SLOT_WAIT_TIMEOUT: Duration = Duration::from_millis(100);

/// Burst capacity of the fps pacer's token bucket (see the encode thread in
/// `Pipeline::start`): how many frames may pass back to back after a stall
/// before the target fps applies again. Three is enough to absorb a
/// catch-up burst from a source that oversleeps by a couple of intervals,
/// small enough not to matter for the rate the encoder sees.
const PACER_BURST: f64 = 3.0;

/// Token-bucket fps pacer for the encode thread: tokens accrue at the
/// target fps along the *capture* clock (`RawFrame::ts`), one token per
/// encoded frame, capped at `PACER_BURST`. The cap is what lets a source
/// that stalled and then caught up in a burst (the synthetic source on a
/// loaded CI VM, see slice 2.1d; a real capture after a hiccup) still
/// deliver the frames it "owes" instead of losing them, while steady-state
/// throughput stays capped at the target fps. Starts full so the first
/// frames go straight through. Pure (time is the frame's own timestamp), so
/// it is unit-tested with synthetic instants rather than by racing real
/// threads against the wall clock.
struct Pacer {
    tokens: f64,
    last_ts: Option<Instant>,
}

impl Pacer {
    fn new() -> Self {
        Self {
            tokens: PACER_BURST,
            last_ts: None,
        }
    }

    /// Whether a frame captured at `ts` should be encoded at `fps`: tops
    /// the bucket up by the capture-clock time since the previous frame,
    /// then spends one token if there is one.
    fn admit(&mut self, ts: Instant, fps: u32) -> bool {
        let fps = fps.max(1);
        if let Some(last) = self.last_ts {
            let elapsed = ts.saturating_duration_since(last).as_secs_f64();
            self.tokens = (self.tokens + elapsed * f64::from(fps)).min(PACER_BURST);
        }
        self.last_ts = Some(ts);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[derive(Debug, Default)]
pub struct PipelineStats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub dropped: AtomicU64,
    pub keyframes: AtomicU64,
    /// Raw frames skipped by the fps pacer (see `RateControl`/`Pipeline`'s
    /// encode loop) before they ever reached the encoder -- distinct from
    /// `dropped`, which counts frames the encoder produced but the output
    /// channel couldn't take.
    pub paced_out: AtomicU64,
    /// Captured frames the encode thread never took because the capture
    /// thread replaced them in the slot first -- i.e. frames arrived faster
    /// than the encoder consumed them. Distinct from `paced_out` (skipped on
    /// purpose, cheaply) and `dropped` (encoded but not delivered).
    pub overwritten: AtomicU64,
}

/// Runtime bitrate/fps target shared between whoever drives adaptation
/// (slice 2.3's controller, or a test) and the pipeline's encode thread:
/// `set()` stages a `RateTarget` for the encode thread to pick up and apply
/// on its next iteration (`encoder.set_rate`), and records the fps so the
/// encode thread's pacer can budget capture-to-encode at that rate without
/// waiting for the encoder to actually apply it.
#[derive(Debug, Default)]
pub struct RateControl {
    pending: Mutex<Option<RateTarget>>,
    fps: AtomicU32,
}

impl RateControl {
    fn new(initial: RateTarget) -> Self {
        Self {
            pending: Mutex::new(None),
            fps: AtomicU32::new(initial.fps),
        }
    }

    /// Stages `target` for the encode thread to apply, and updates the fps
    /// the pacer uses immediately (it doesn't need to wait for the encoder
    /// to actually pick up the new rate).
    pub fn set(&self, target: RateTarget) {
        *self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(target);
        self.fps.store(target.fps, Ordering::Release);
    }

    /// The fps the pacer is currently budgeting frames at.
    pub fn fps(&self) -> u32 {
        self.fps.load(Ordering::Acquire)
    }
}

/// Single-slot mailbox from the capture thread to the encode thread: only
/// the most recent unconsumed frame is kept, older ones are overwritten.
struct FrameSlot {
    frame: Mutex<Option<RawFrame>>,
    condvar: Condvar,
}

pub struct PipelineHandle {
    pub frames: mpsc::Receiver<EncodedFrame>,
    request_keyframe: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    pub stats: Arc<PipelineStats>,
    rate_control: Arc<RateControl>,
    capture_thread: Option<JoinHandle<()>>,
    encode_thread: Option<JoinHandle<()>>,
}

pub struct Pipeline;

impl Pipeline {
    pub fn start(
        source: Box<dyn FrameSource>,
        encoder: Box<dyn Encoder>,
        initial: RateTarget,
    ) -> PipelineHandle {
        Self::start_with_keyframe_flag(source, encoder, initial, Arc::new(AtomicBool::new(false)))
    }

    /// Same as `start`, but takes the keyframe-request flag instead of
    /// creating a fresh one. The flag is shared across every pipeline in a
    /// session: the transport's PLI/FIR handler (`PeerSession::start_video`)
    /// holds one `Arc` for the whole session, while the pipeline itself gets
    /// torn down and rebuilt whenever the transmitted display changes
    /// (slice 2.4) -- rebuilding it would lose a keyframe request that
    /// arrived mid-switch.
    pub fn start_with_keyframe_flag(
        mut source: Box<dyn FrameSource>,
        mut encoder: Box<dyn Encoder>,
        initial: RateTarget,
        request_keyframe: Arc<AtomicBool>,
    ) -> PipelineHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(PipelineStats::default());
        let rate_control = Arc::new(RateControl::new(initial));
        let slot = Arc::new(FrameSlot {
            frame: Mutex::new(None),
            condvar: Condvar::new(),
        });
        let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);

        let capture_thread = {
            let stop = Arc::clone(&stop);
            let slot = Arc::clone(&slot);
            let stats = Arc::clone(&stats);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match source.next_frame() {
                        Ok(frame) => {
                            stats.captured.fetch_add(1, Ordering::Relaxed);
                            // Poisoning would mean the encode thread panicked
                            // while holding the lock; there's nothing sound
                            // to recover into, so surface the panic instead
                            // of masking it.
                            let mut guard = slot.frame.lock().expect("frame slot mutex poisoned");
                            if guard.replace(frame).is_some() {
                                // The encode thread never took the previous
                                // frame: it is still busy encoding the one
                                // before -- the "encoder can't keep up"
                                // signal the adaptation controller reads
                                // (see `crate::adapt`).
                                stats.overwritten.fetch_add(1, Ordering::Relaxed);
                            }
                            slot.condvar.notify_one();
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let encode_thread = {
            let stop = Arc::clone(&stop);
            let slot = Arc::clone(&slot);
            let stats = Arc::clone(&stats);
            let request_keyframe = Arc::clone(&request_keyframe);
            let rate_control = Arc::clone(&rate_control);
            std::thread::spawn(move || {
                let mut pacer = Pacer::new();

                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }

                    let raw = {
                        let mut guard = slot.frame.lock().expect("frame slot mutex poisoned");
                        loop {
                            if let Some(frame) = guard.take() {
                                break Some(frame);
                            }
                            if stop.load(Ordering::Relaxed) {
                                break None;
                            }
                            let (next_guard, _timeout) = slot
                                .condvar
                                .wait_timeout(guard, SLOT_WAIT_TIMEOUT)
                                .expect("frame slot mutex poisoned");
                            guard = next_guard;
                        }
                    };

                    let Some(raw) = raw else { continue };
                    let captured_at = raw.ts();

                    // (a) Apply any pending rate-control target before this
                    // frame, whether or not pacing ends up encoding it.
                    let pending = rate_control
                        .pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    if let Some(target) = pending {
                        if let Err(err) = encoder.set_rate(target) {
                            tracing::warn!(
                                error = %err,
                                bitrate_kbps = target.bitrate_kbps,
                                fps = target.fps,
                                "failed to apply rate target, continuing at previous rate"
                            );
                        }
                    }

                    // (b) Pace to the target fps (see `Pacer`).
                    if !pacer.admit(captured_at, rate_control.fps()) {
                        stats.paced_out.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    // Never encode a frame nobody can take: dropping an
                    // already encoded P-frame would break the decoder's
                    // reference chain until the next keyframe. Skip the raw
                    // frame instead.
                    if tx.capacity() == 0 {
                        stats.dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    let force_keyframe = request_keyframe.swap(false, Ordering::AcqRel);

                    match encoder.encode(&raw, force_keyframe) {
                        Ok(Some(encoded)) => {
                            stats.encoded.fetch_add(1, Ordering::Relaxed);
                            if encoded.keyframe {
                                stats.keyframes.fetch_add(1, Ordering::Relaxed);
                            }
                            tracing::trace!(
                                convert_encode_latency_us =
                                    (encoded.ts - captured_at).as_micros() as u64,
                                keyframe = encoded.keyframe,
                                "encoded frame"
                            );
                            if tx.try_send(encoded).is_err() {
                                // Channel closed (receiver gone) or a race
                                // with the capacity check: the stream is now
                                // missing a reference frame, so recover with
                                // a keyframe.
                                stats.dropped.fetch_add(1, Ordering::Relaxed);
                                request_keyframe.store(true, Ordering::Release);
                            }
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            })
        };

        PipelineHandle {
            frames: rx,
            request_keyframe,
            stop,
            stats,
            rate_control,
            capture_thread: Some(capture_thread),
            encode_thread: Some(encode_thread),
        }
    }
}

impl PipelineHandle {
    /// Asks the encoder to produce a keyframe on the next encoded frame.
    pub fn request_keyframe(&self) {
        self.request_keyframe.store(true, Ordering::Release);
    }

    /// Shares the underlying keyframe-request flag so a transport layer
    /// (e.g. a PLI/FIR handler on the WebRTC video track) can set it
    /// directly, the same way `request_keyframe()` does.
    pub fn keyframe_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.request_keyframe)
    }

    /// Shares the rate-control handle a caller (the slice 2.3 adaptation
    /// controller, or a test) uses to push new bitrate/fps targets into the
    /// running encoder via `RateControl::set`.
    pub fn rate_control(&self) -> Arc<RateControl> {
        Arc::clone(&self.rate_control)
    }

    /// Stops both threads and waits for them to finish.
    ///
    /// For the synthetic source this returns promptly (it sleeps in bounded
    /// slices). For a live `scap` source the capture thread may be blocked
    /// inside a blocking `get_next_frame()` call with no external way to
    /// interrupt it from here; it unblocks once the source itself errors out
    /// (e.g. `stop_capture` from its own `Drop`) or the next frame arrives.
    /// Live capture shutdown is exercised manually, not by the test suite.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.encode_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::synthetic::SyntheticSource;
    use crate::encode::{build_encoder, EncoderConfig, EncoderKind};
    use std::time::Instant;
    use tokio::sync::mpsc::error::TryRecvError;

    /// The three pipeline tests below each spin up a capture thread and an
    /// encoder; run concurrently on a small CI VM (`macos-latest`, 3 vCPU)
    /// they starve each other and the wall-clock frame counts they assert
    /// on come out short (15 frames/s from a 30 fps source, a 60 fps source
    /// managing 25). Serialize them; the pacer's own logic is covered by the
    /// deterministic `pacer_*` tests, which don't need this.
    static PIPELINE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        PIPELINE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn pacer_passes_a_steady_source_at_target_fps() {
        let base = Instant::now();
        let mut pacer = Pacer::new();
        // 30 fps source with ±3 ms capture jitter, target 30: every frame
        // must pass -- the bucket absorbs the jitter.
        let jitter = [0i64, 3, -2, 1, -3, 2, 0, -1, 3, -2];
        let mut admitted = 0;
        for i in 0..60u64 {
            let ts = base
                + Duration::from_micros(
                    (i as i64 * 33_333 + jitter[i as usize % jitter.len()] * 1000) as u64,
                );
            if pacer.admit(ts, 30) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 60);
    }

    #[test]
    fn pacer_caps_a_fast_source_at_target_fps() {
        let base = Instant::now();
        let mut pacer = Pacer::new();
        // 60 fps source, target 10, over 2 s: 20 frames' worth of tokens
        // plus the initial burst of 3.
        let admitted = (0..120u64)
            .filter(|&i| pacer.admit(at(base, i * 1000 / 60), 10))
            .count();
        // ±1 for the integer-millisecond timestamps (the last frame lands
        // at 1983 ms, not 2000).
        let expected = 20 + PACER_BURST as usize;
        assert!(
            (expected - 1..=expected).contains(&admitted),
            "expected ~{expected} admitted frames, got {admitted}"
        );
    }

    #[test]
    fn pacer_lets_a_catch_up_burst_through_after_a_stall() {
        let base = Instant::now();
        let mut pacer = Pacer::new();
        // Drain the initial burst with a steady 30 fps run.
        for i in 0..30u64 {
            assert!(pacer.admit(at(base, i * 1000 / 30), 30));
        }
        // 200 ms stall, then three frames 1 ms apart (a source catching up
        // on its schedule): all three pass on the tokens accrued during the
        // stall; the fourth, 1 ms later, doesn't.
        let stall_end = at(base, 1000 + 200);
        assert!(pacer.admit(stall_end, 30));
        assert!(pacer.admit(stall_end + Duration::from_millis(1), 30));
        assert!(pacer.admit(stall_end + Duration::from_millis(2), 30));
        assert!(!pacer.admit(stall_end + Duration::from_millis(3), 30));
    }

    #[test]
    fn pacer_follows_fps_changes_immediately() {
        let base = Instant::now();
        let mut pacer = Pacer::new();
        for i in 0..30u64 {
            assert!(pacer.admit(at(base, i * 1000 / 30), 30));
        }
        // Same 30 fps source, target dropped to 15: half the frames pass
        // (+1 for the fraction of a token left over from the 30 fps run).
        let admitted = (30..90u64)
            .filter(|&i| pacer.admit(at(base, i * 1000 / 30), 15))
            .count();
        assert!(
            (30..=31).contains(&admitted),
            "expected ~30 of 60 frames admitted at 15 fps, got {admitted}"
        );
    }

    fn drain_for(handle: &mut PipelineHandle, duration: Duration) -> Vec<EncodedFrame> {
        let deadline = Instant::now() + duration;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match handle.frames.try_recv() {
                Ok(frame) => out.push(frame),
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
                Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    #[test]
    fn synthetic_pipeline_produces_frames_and_honours_keyframe_requests() {
        let _guard = serialized();
        let source: Box<dyn FrameSource> = Box::new(SyntheticSource::new(64, 64, 30));
        let cfg = EncoderConfig {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_kbps: 2000,
            keyframe_interval_frames: 60,
            max_qp: None,
        };
        let (encoder, _kind) =
            build_encoder(Some(EncoderKind::OpenH264), cfg).expect("encoder init");

        let mut handle = Pipeline::start(
            source,
            encoder,
            RateTarget {
                bitrate_kbps: 2000,
                fps: 30,
            },
        );

        let received = drain_for(&mut handle, Duration::from_secs(1));
        assert!(
            received.len() >= 20,
            "expected >= 20 frames in ~1s, got {}",
            received.len()
        );
        assert!(
            received[0].keyframe,
            "first encoded frame must be a keyframe"
        );

        handle.request_keyframe();
        let more = drain_for(&mut handle, Duration::from_millis(300));
        assert!(
            more.iter().any(|f| f.keyframe),
            "expected a keyframe shortly after request_keyframe()"
        );

        handle.stop();
    }

    #[test]
    fn pacing_caps_encoded_fps() {
        let _guard = serialized();
        let source: Box<dyn FrameSource> = Box::new(SyntheticSource::new(64, 64, 60));
        let cfg = EncoderConfig {
            width: 64,
            height: 64,
            fps: 10,
            bitrate_kbps: 2000,
            keyframe_interval_frames: 60,
            max_qp: None,
        };
        let (encoder, _kind) =
            build_encoder(Some(EncoderKind::OpenH264), cfg).expect("encoder init");

        let mut handle = Pipeline::start(
            source,
            encoder,
            RateTarget {
                bitrate_kbps: 2000,
                fps: 10,
            },
        );

        let received = drain_for(&mut handle, Duration::from_secs(2));
        let paced_out = handle.stats.paced_out.load(Ordering::Relaxed);
        let captured = handle.stats.captured.load(Ordering::Relaxed);
        handle.stop();

        // Wall-clock bounds only in the direction a slow machine can't
        // break: at most 10 fps * ~2 s plus the pacer's burst (the exact
        // rate is covered by `pacer_caps_a_fast_source_at_target_fps`), and
        // the pacer must have skipped something, however few frames the
        // 60 fps source actually managed.
        assert!(
            received.len() <= 26,
            "expected at most ~10 fps over 2 s, got {} frames",
            received.len()
        );
        assert!(
            paced_out >= 1,
            "expected the pacer to skip frames of a 60fps source \
             (captured={captured}, encoded={}), got paced_out=0",
            received.len()
        );
    }

    #[test]
    fn rate_control_applies_pending_target() {
        let _guard = serialized();
        let source: Box<dyn FrameSource> = Box::new(SyntheticSource::new(64, 64, 30));
        let cfg = EncoderConfig {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_kbps: 2000,
            keyframe_interval_frames: 60,
            max_qp: None,
        };
        let (encoder, _kind) =
            build_encoder(Some(EncoderKind::OpenH264), cfg).expect("encoder init");

        let mut handle = Pipeline::start(
            source,
            encoder,
            RateTarget {
                bitrate_kbps: 2000,
                fps: 30,
            },
        );

        handle.rate_control().set(RateTarget {
            bitrate_kbps: 500,
            fps: 30,
        });

        let received = drain_for(&mut handle, Duration::from_secs(1));
        handle.stop();

        assert!(
            received.len() >= 10,
            "expected pipeline to keep producing frames after set_rate, got {}",
            received.len()
        );
    }
}
