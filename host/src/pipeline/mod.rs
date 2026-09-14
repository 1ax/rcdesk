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
        mut source: Box<dyn FrameSource>,
        mut encoder: Box<dyn Encoder>,
        initial: RateTarget,
    ) -> PipelineHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let request_keyframe = Arc::new(AtomicBool::new(false));
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
                // Token bucket for the fps pacer: tokens accrue at the
                // target fps along the *capture* clock (`RawFrame::ts`), one
                // token per encoded frame, capped at `PACER_BURST`. The cap
                // is what lets a source that stalled and then caught up in a
                // burst (the synthetic source on a loaded CI VM, see slice
                // 2.1d; a real capture after a hiccup) still deliver the
                // frames it "owes" instead of losing them -- a plain
                // next-due deadline threw those away and starved a 30 fps
                // test down to 15 frames/s on `macos-latest`. Steady-state
                // throughput is still capped at the target fps. Starts full
                // so the first frames go straight through.
                let mut tokens: f64 = PACER_BURST;
                let mut last_captured_at: Option<Instant> = None;

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

                    // (b) Pace to the target fps (see `tokens` above): top
                    // the bucket up by the capture-clock time elapsed since
                    // the previous frame, then spend one token or skip.
                    let fps = rate_control.fps().max(1);
                    if let Some(last) = last_captured_at {
                        let elapsed = captured_at.saturating_duration_since(last).as_secs_f64();
                        tokens = (tokens + elapsed * f64::from(fps)).min(PACER_BURST);
                    }
                    last_captured_at = Some(captured_at);
                    if tokens < 1.0 {
                        stats.paced_out.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    tokens -= 1.0;

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
        handle.stop();

        assert!(
            (15..=26).contains(&received.len()),
            "expected roughly 10fps over 2s (15..=26 frames), got {}",
            received.len()
        );
        assert!(
            paced_out > 40,
            "expected the 60fps source to be paced down heavily, got paced_out={paced_out}"
        );
    }

    #[test]
    fn rate_control_applies_pending_target() {
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
