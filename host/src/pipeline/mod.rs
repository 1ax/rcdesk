//! Wires a `FrameSource` and an `Encoder` together into a running pipeline:
//! one thread captures, one thread converts + encodes, and encoded frames
//! come out through a bounded channel. No network/transport here (that is
//! slice 1.2b) -- this just proves the video path end to end.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::capture::{FrameSource, RawFrame};
use crate::encode::{to_i420, EncodedFrame, Encoder};

/// How many encoded frames may sit in the output channel before new ones get
/// dropped. Keeps memory/latency bounded when nothing is draining the
/// channel yet (no transport in this slice).
const FRAME_CHANNEL_CAPACITY: usize = 4;

/// How long the encode thread waits on the condvar between polls of the
/// stop flag, so `stop()` doesn't have to wait for a whole frame interval.
const SLOT_WAIT_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Default)]
pub struct PipelineStats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub dropped: AtomicU64,
    pub keyframes: AtomicU64,
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
    capture_thread: Option<JoinHandle<()>>,
    encode_thread: Option<JoinHandle<()>>,
}

pub struct Pipeline;

impl Pipeline {
    pub fn start(
        mut source: Box<dyn FrameSource>,
        mut encoder: Box<dyn Encoder>,
    ) -> PipelineHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let request_keyframe = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(PipelineStats::default());
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
                            *guard = Some(frame);
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
            std::thread::spawn(move || loop {
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

                // Never encode a frame nobody can take: dropping an already
                // encoded P-frame would break the decoder's reference chain
                // until the next keyframe. Skip the raw frame instead.
                if tx.capacity() == 0 {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let i420 = to_i420(&raw);
                let force_keyframe = request_keyframe.swap(false, Ordering::AcqRel);

                match encoder.encode(&i420, force_keyframe) {
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
                            // Channel closed (receiver gone) or a race with
                            // the capacity check: the stream is now missing a
                            // reference frame, so recover with a keyframe.
                            stats.dropped.fetch_add(1, Ordering::Relaxed);
                            request_keyframe.store(true, Ordering::Release);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            })
        };

        PipelineHandle {
            frames: rx,
            request_keyframe,
            stop,
            stats,
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
    use crate::encode::openh264::OpenH264Encoder;
    use crate::encode::EncoderConfig;
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
        };
        let encoder: Box<dyn Encoder> = Box::new(OpenH264Encoder::new(cfg).expect("encoder init"));

        let mut handle = Pipeline::start(source, encoder);

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
}
