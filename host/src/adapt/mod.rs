//! Bitrate/fps adaptation controller (slice 2.3): watches REMB/loss feedback
//! from the peer and the encoder's own capture-to-encode latency, and
//! decides the `encode::RateTarget` the pipeline's `pipeline::RateControl`
//! should be driving the encoder at.
//!
//! Deliberately free of tokio/async and any I/O: `signaling/mod.rs` owns the
//! task that feeds this controller `Feedback` and calls `tick()` once a
//! second, and applies the resulting `Decision` to `RateControl` and the
//! `control` data channel. Time is always an explicit `Instant` argument so
//! the whole thing is deterministic and unit-testable without sleeping.

use std::time::{Duration, Instant};

use crate::encode::RateTarget;

/// How often `tick()` is expected to be called. Fine-grained enough to react
/// to REMB (Chrome sends one roughly every 200ms -- see
/// `transport::SessionEvent::Remb`'s doc comment) well within a second,
/// coarse enough that a probe step (`PROBE_UP`) doesn't hunt.
pub const TICK: Duration = Duration::from_secs(1);

/// Never adapt the bitrate below this: below it H.264 at any reasonable
/// resolution turns to mush before fps has room to drop further to help.
pub const MIN_BITRATE_KBPS: u32 = 300;

/// Never adapt fps below this: much less than a real desktop session is
/// still usable at, and low-fps mouse tracking becomes actively painful.
pub const MIN_FPS: u32 = 5;

/// REMB older than this is stale (Chrome sends one every ~200ms; a gap this
/// large means the RTCP path itself is in trouble, not that bandwidth is
/// fine) and is ignored rather than acted on.
pub const REMB_FRESH: Duration = Duration::from_secs(3);

/// A REMB below this fraction of what we actually sent last tick signals
/// congestion (the browser's receive-side estimate is capped at roughly
/// 1.5x what it actually received -- see `docs/host-libs-api-notes.md` --
/// so it is only a trustworthy *down* signal, and only once it falls
/// meaningfully short of what we pushed).
pub const REMB_CONGESTION_RATIO: f64 = 0.9;

/// Packet loss fraction above which we cut bitrate aggressively regardless
/// of what REMB says.
pub const LOSS_HIGH: f32 = 0.10;

/// Packet loss fraction below which the link is healthy enough to probe for
/// more bandwidth.
pub const LOSS_LOW: f32 = 0.02;

/// Bitrate growth factor applied once per tick while probing for more
/// bandwidth (REMB only ever tells us to go *down*; growth has to be our
/// own probing -- see this module's doc comment).
pub const PROBE_UP: f64 = 1.10;

/// Minimum relative bitrate change worth publishing a `Decision` for --
/// smaller changes are noise the client's overlay and the encoder's rate
/// control don't need to see.
pub const HYSTERESIS: f64 = 0.05;

/// Candidate frame rates, highest first. Only entries within
/// `[min_fps, max_fps]` are used, and `max_fps` is added as the top rung if
/// it isn't already one of these (see `Controller::ladder`).
pub const FPS_LADDER: [u32; 8] = [60, 30, 24, 20, 15, 12, 10, 8];

/// If the average capture-to-encode latency over a tick exceeds this
/// multiple of the frame interval at the *current* fps, the encoder isn't
/// keeping up and fps should step down.
pub const ENCODER_SLOW_RATIO: f64 = 1.2;

/// If the average capture-to-encode latency over a tick is below this
/// multiple of the frame interval at the fps *one rung up*, the encoder has
/// headroom to spare.
pub const ENCODER_FAST_RATIO: f64 = 0.5;

/// How many consecutive "encoder has headroom" ticks (see
/// `ENCODER_FAST_RATIO`) are required before stepping fps back up -- avoids
/// flapping on a single lucky frame.
pub const ENCODER_FAST_TICKS: u32 = 3;

/// Minimum bits-per-pixel-per-frame the target bitrate/fps combination must
/// afford. Below this, more fps just spreads the same bits over more (worse)
/// frames -- ARCHITECTURE.md §5 says to sacrifice fps before quality.
pub const MIN_BITS_PER_PIXEL_PER_FRAME: f64 = 0.02;

/// Everything the controller needs to know about the session's configured
/// limits: the ceiling comes from `--bitrate`/`--fps`, the floor is fixed
/// (see `MIN_BITRATE_KBPS`/`MIN_FPS`), and `width`/`height` size the
/// per-frame bit budget (see `MIN_BITS_PER_PIXEL_PER_FRAME`).
#[derive(Debug, Clone, Copy)]
pub struct AdaptConfig {
    pub max_bitrate_kbps: u32,
    pub min_bitrate_kbps: u32,
    pub max_fps: u32,
    pub min_fps: u32,
    pub width: u32,
    pub height: u32,
}

/// One piece of feedback fed into the controller between ticks.
#[derive(Debug, Clone, Copy)]
pub enum Feedback {
    /// `transport::SessionEvent::Remb`.
    Remb { bitrate_bps: u64 },
    /// Derived from `transport::SessionEvent::ReceiverReport::fraction_lost`.
    Loss { fraction: f32 },
    /// One encoded frame, reported by the forward task in `signaling/mod.rs`
    /// for every frame the pipeline produces.
    Frame {
        bytes: usize,
        capture_to_encoded: Duration,
    },
}

/// A change to the encoder's rate target the controller decided on, with the
/// reason shown in the client's overlay (see `proto::control::ControlMessage::Quality`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub target: RateTarget,
    pub reason: &'static str,
}

/// Accumulates feedback between ticks; see this module's doc comment.
#[derive(Debug, Default)]
struct Window {
    bytes: u64,
    latency_sum: Duration,
    latency_count: u32,
}

impl Window {
    fn reset(&mut self) {
        *self = Window::default();
    }

    fn avg_latency(&self) -> Option<Duration> {
        if self.latency_count == 0 {
            None
        } else {
            Some(self.latency_sum / self.latency_count)
        }
    }
}

/// The bitrate/fps adaptation controller. See this module's doc comment for
/// how it's driven.
pub struct Controller {
    cfg: AdaptConfig,
    target: RateTarget,
    last_remb_kbps: Option<f64>,
    last_remb_at: Option<Instant>,
    last_loss: f32,
    window: Window,
    /// Consecutive ticks the encoder has shown headroom at the next rung up
    /// (see `ENCODER_FAST_TICKS`).
    fast_ticks: u32,
    last_tick_at: Instant,
}

impl Controller {
    /// Starts at the configured ceiling: `--bitrate`/`--fps` for the whole
    /// session until feedback says otherwise.
    pub fn new(cfg: AdaptConfig, now: Instant) -> Self {
        let target = RateTarget {
            bitrate_kbps: cfg.max_bitrate_kbps,
            fps: cfg.max_fps,
        };
        Self {
            cfg,
            target,
            last_remb_kbps: None,
            last_remb_at: None,
            last_loss: 0.0,
            window: Window::default(),
            fast_ticks: 0,
            last_tick_at: now,
        }
    }

    /// The controller's current committed rate target (only changes when a
    /// `tick()` call returns `Some`).
    pub fn target(&self) -> RateTarget {
        self.target
    }

    pub fn feedback(&mut self, fb: Feedback, now: Instant) {
        match fb {
            Feedback::Remb { bitrate_bps } => {
                self.last_remb_kbps = Some(bitrate_bps as f64 / 1000.0);
                self.last_remb_at = Some(now);
            }
            Feedback::Loss { fraction } => {
                self.last_loss = fraction;
            }
            Feedback::Frame {
                bytes,
                capture_to_encoded,
            } => {
                self.window.bytes += bytes as u64;
                self.window.latency_sum += capture_to_encoded;
                self.window.latency_count += 1;
            }
        }
    }

    /// The fps ladder for this controller's config: entries of `FPS_LADDER`
    /// within `[min_fps, max_fps]`, plus `max_fps` itself if it isn't one of
    /// them, sorted highest first.
    fn ladder(&self) -> Vec<u32> {
        let mut steps: Vec<u32> = FPS_LADDER
            .iter()
            .copied()
            .filter(|&f| f >= self.cfg.min_fps && f <= self.cfg.max_fps)
            .collect();
        if !steps.contains(&self.cfg.max_fps) {
            steps.push(self.cfg.max_fps);
        }
        steps.sort_unstable_by(|a, b| b.cmp(a));
        steps.dedup();
        steps
    }

    /// Called once per `TICK`. Returns the new target if it changed enough
    /// to be worth publishing (see `HYSTERESIS`), `None` otherwise -- in
    /// which case the controller's committed target is left untouched (see
    /// this function's body for why that matters for `PROBE_UP`).
    pub fn tick(&mut self, now: Instant) -> Option<Decision> {
        let elapsed = now
            .saturating_duration_since(self.last_tick_at)
            .as_secs_f64();
        let sent_kbps = if elapsed > 0.0 {
            self.window.bytes as f64 * 8.0 / 1000.0 / elapsed
        } else {
            0.0
        };
        let avg_latency = self.window.avg_latency();

        let old_b = self.target.bitrate_kbps as f64;
        let remb_fresh = self
            .last_remb_at
            .is_some_and(|at| now.saturating_duration_since(at) <= REMB_FRESH);
        let remb_kbps = if remb_fresh {
            self.last_remb_kbps
        } else {
            None
        };

        let (mut b, bitrate_reason) = if self.last_loss > LOSS_HIGH {
            (old_b * (1.0 - 0.5 * self.last_loss as f64), Some("loss"))
        } else if let Some(remb) = remb_kbps.filter(|&r| r < sent_kbps * REMB_CONGESTION_RATIO) {
            (old_b.min(remb), Some("remb"))
        } else if self.last_loss < LOSS_LOW
            && (remb_kbps.is_none_or(|r| r > old_b))
            && old_b < self.cfg.max_bitrate_kbps as f64
        {
            (old_b * PROBE_UP, Some("probe"))
        } else {
            (old_b, None)
        };
        b = b.clamp(
            self.cfg.min_bitrate_kbps as f64,
            self.cfg.max_bitrate_kbps as f64,
        );

        let steps = self.ladder();
        let current_fps = self.target.fps;

        let (encoder_fps, encoder_fired) = match avg_latency {
            None => (current_fps, false),
            Some(avg) => {
                let avg_secs = avg.as_secs_f64();
                let current_interval = 1.0 / current_fps as f64;
                if avg_secs > ENCODER_SLOW_RATIO * current_interval {
                    let stepped = step_down(&steps, current_fps);
                    self.fast_ticks = 0;
                    (stepped, stepped != current_fps)
                } else {
                    let higher = step_up(&steps, current_fps);
                    let higher_interval = 1.0 / higher as f64;
                    if higher != current_fps && avg_secs < ENCODER_FAST_RATIO * higher_interval {
                        self.fast_ticks += 1;
                        if self.fast_ticks >= ENCODER_FAST_TICKS {
                            self.fast_ticks = 0;
                            (higher, true)
                        } else {
                            (current_fps, false)
                        }
                    } else {
                        self.fast_ticks = 0;
                        (current_fps, false)
                    }
                }
            }
        };

        let bitrate_fps = self.bitrate_fps_step(&steps, b);
        let new_fps = self
            .cfg
            .max_fps
            .min(encoder_fps)
            .min(bitrate_fps)
            .max(self.cfg.min_fps);
        let bitrate_limited_fps =
            new_fps == bitrate_fps && bitrate_fps < self.cfg.max_fps && bitrate_fps <= encoder_fps;

        self.window.reset();
        self.last_tick_at = now;

        let fps_changed = new_fps != current_fps;
        let bitrate_changed = old_b > 0.0 && ((b - old_b).abs() / old_b) >= HYSTERESIS;
        if !fps_changed && !bitrate_changed {
            return None;
        }

        let reason = match bitrate_reason {
            Some("loss") => "loss",
            Some("remb") => "remb",
            _ if encoder_fired => "encoder",
            _ if bitrate_limited_fps => "bitrate",
            Some("probe") => "probe",
            _ => "bitrate",
        };

        let new_target = RateTarget {
            bitrate_kbps: b.round() as u32,
            fps: new_fps,
        };
        self.target = new_target;
        Some(Decision {
            target: new_target,
            reason,
        })
    }

    /// Largest ladder step for which `b` (kbps) affords at least
    /// `MIN_BITS_PER_PIXEL_PER_FRAME` bits/pixel/frame at this controller's
    /// resolution; falls back to `min_fps` if even the smallest step doesn't
    /// (see `MIN_BITS_PER_PIXEL_PER_FRAME`'s doc comment: fewer, better
    /// frames beat more, worse ones).
    fn bitrate_fps_step(&self, steps: &[u32], b_kbps: f64) -> u32 {
        let threshold_bits =
            self.cfg.width as f64 * self.cfg.height as f64 * MIN_BITS_PER_PIXEL_PER_FRAME;
        for &fps in steps {
            let budget_bits = b_kbps * 1000.0 / fps as f64;
            if budget_bits >= threshold_bits {
                return fps;
            }
        }
        self.cfg.min_fps
    }
}

/// The next lower rung than `current` in `steps` (sorted highest first), or
/// `current` if it's already the lowest rung.
fn step_down(steps: &[u32], current: u32) -> u32 {
    match steps.iter().position(|&s| s == current) {
        Some(i) if i + 1 < steps.len() => steps[i + 1],
        _ => current,
    }
}

/// The next higher rung than `current` in `steps` (sorted highest first), or
/// `current` if it's already the highest rung.
fn step_up(steps: &[u32], current: u32) -> u32 {
    match steps.iter().position(|&s| s == current) {
        Some(i) if i > 0 => steps[i - 1],
        _ => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1280x720, 30fps/6000kbps ceiling, matching the `--synthetic` default
    /// (see `main.rs::run_serve`) -- large enough a bit budget that the fps
    /// ladder never limits fps at the full bitrate (see `bitrate_fps_step`),
    /// so tests that aren't specifically about the bit budget don't have to
    /// think about it.
    fn cfg() -> AdaptConfig {
        AdaptConfig {
            max_bitrate_kbps: 6000,
            min_bitrate_kbps: MIN_BITRATE_KBPS,
            max_fps: 30,
            min_fps: MIN_FPS,
            width: 1280,
            height: 720,
        }
    }

    fn frame(c: &mut Controller, now: Instant, bytes: usize, latency: Duration) {
        c.feedback(
            Feedback::Frame {
                bytes,
                capture_to_encoded: latency,
            },
            now,
        );
    }

    #[test]
    fn starts_at_max_and_holds_without_feedback() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        assert_eq!(
            c.target(),
            RateTarget {
                bitrate_kbps: 6000,
                fps: 30
            }
        );

        let mut now = t0;
        for _ in 0..3 {
            now += TICK;
            // A fast encoder (low capture-to-encode latency) every tick --
            // already at the top fps rung, so this must never turn into a
            // step up (there's nowhere higher to go).
            frame(&mut c, now, 25_000, Duration::from_millis(2));
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target().bitrate_kbps, 6000);
        assert_eq!(c.target().fps, 30);
    }

    #[test]
    fn remb_below_sent_rate_caps_bitrate() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let now = t0 + TICK;

        // 30 frames * 25_000 bytes = 750_000 bytes/s = 6000 kbps sent.
        frame(&mut c, now, 750_000, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 2_000_000,
            },
            now,
        );

        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "remb");
        assert_eq!(decision.target.bitrate_kbps, 2000);
        assert_eq!(decision.target.fps, 30);
    }

    #[test]
    fn remb_above_sent_rate_is_not_congestion() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let now = t0 + TICK;

        // 500 kbps sent, REMB 700 kbps: REMB >= 0.9 * sent (450) and REMB <
        // b (6000), so neither congestion nor probing applies -- hold.
        frame(&mut c, now, 62_500, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 700_000,
            },
            now,
        );

        assert_eq!(c.tick(now), None);
        assert_eq!(c.target().bitrate_kbps, 6000);
    }

    #[test]
    fn probe_ramps_up_after_remb_cap() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        // Reach the same state as `remb_below_sent_rate_caps_bitrate`:
        // b = 2000 after a REMB cap.
        let mut now = t0 + TICK;
        frame(&mut c, now, 750_000, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 2_000_000,
            },
            now,
        );
        assert_eq!(c.tick(now).unwrap().target.bitrate_kbps, 2000);

        // Now sending only 2000 kbps, but REMB says the path can carry
        // 5000: not congestion (2000 < 0.9*2000 is false), and REMB > b, so
        // probe.
        now += TICK;
        frame(&mut c, now, 250_000, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 5_000_000,
            },
            now,
        );
        let d1 = c.tick(now).expect("expected a probe decision");
        assert_eq!(d1.reason, "probe");
        assert_eq!(d1.target.bitrate_kbps, 2200);

        now += TICK;
        frame(&mut c, now, 250_000, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 5_000_000,
            },
            now,
        );
        let d2 = c.tick(now).expect("expected a probe decision");
        assert_eq!(d2.reason, "probe");
        assert_eq!(d2.target.bitrate_kbps, 2420);
        assert!(d2.target.bitrate_kbps <= 6000);
    }

    #[test]
    fn high_loss_cuts_bitrate() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let now = t0 + TICK;

        c.feedback(Feedback::Loss { fraction: 0.2 }, now);
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "loss");
        assert_eq!(decision.target.bitrate_kbps, 5400);
        assert_eq!(decision.target.fps, 30);
    }

    #[test]
    fn slow_encoder_steps_fps_down() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;
        let expected_fps = [24u32, 20, 15];

        for expected in expected_fps {
            now += TICK;
            frame(&mut c, now, 25_000, Duration::from_millis(67));
            let decision = c.tick(now).expect("expected an encoder step down");
            assert_eq!(decision.reason, "encoder");
            assert_eq!(decision.target.fps, expected);
        }

        // At fps=15 the frame interval is 66.7ms; 1.2x that is 80ms, and
        // 67ms no longer exceeds it -- holds.
        now += TICK;
        frame(&mut c, now, 25_000, Duration::from_millis(67));
        assert_eq!(c.tick(now), None);
        assert_eq!(c.target().fps, 15);
    }

    #[test]
    fn fast_encoder_steps_fps_back_up_after_three_ticks() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;

        // Walk fps down to 15 first (see `slow_encoder_steps_fps_down`).
        for _ in 0..3 {
            now += TICK;
            frame(&mut c, now, 25_000, Duration::from_millis(67));
            c.tick(now);
        }
        assert_eq!(c.target().fps, 15);

        // Fast encoder (10ms << 0.5 * 1/20 = 25ms) for three ticks in a row.
        now += TICK;
        frame(&mut c, now, 25_000, Duration::from_millis(10));
        assert_eq!(c.tick(now), None);

        now += TICK;
        frame(&mut c, now, 25_000, Duration::from_millis(10));
        assert_eq!(c.tick(now), None);

        now += TICK;
        frame(&mut c, now, 25_000, Duration::from_millis(10));
        let decision = c.tick(now).expect("expected an encoder step up");
        assert_eq!(decision.reason, "encoder");
        assert_eq!(decision.target.fps, 20);
    }

    #[test]
    fn low_bitrate_limits_fps() {
        // 1920x1080: threshold = 1920*1080*0.02 = 41_472 bits/frame.
        let cfg = AdaptConfig {
            max_bitrate_kbps: 6000,
            min_bitrate_kbps: MIN_BITRATE_KBPS,
            max_fps: 30,
            min_fps: MIN_FPS,
            width: 1920,
            height: 1080,
        };
        let t0 = Instant::now();
        let mut c = Controller::new(cfg, t0);
        let now = t0 + TICK;

        // 6000 kbps sent, REMB caps it to 300 kbps.
        frame(&mut c, now, 750_000, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 300_000,
            },
            now,
        );

        let decision = c.tick(now).expect("expected a decision");
        // 300_000 bits/s / 41_472 bits/frame = 7.23: no ladder step
        // (30/24/20/15/12/10/8) fits -- even the smallest, 8, needs
        // 300_000/8 = 37_500 < 41_472 -- so fps falls back to min_fps (5).
        assert_eq!(decision.reason, "remb");
        assert_eq!(decision.target.bitrate_kbps, 300);
        assert_eq!(decision.target.fps, 5);
    }

    #[test]
    fn bitrate_change_under_hysteresis_is_not_published() {
        // The prompt's own illustrative numbers (b=6000, sent=6000, REMB
        // 5800/5500) don't actually cross the REMB_CONGESTION_RATIO
        // threshold (0.9*6000=5400 > both), so neither would enter the
        // "remb" branch at all under the algorithm as specified -- the
        // congestion check compares REMB against *sent*, not against the
        // current target. Using a target already sitting near that
        // threshold (5400) exercises the same hysteresis boundary the
        // prompt intended: a small REMB-driven drop (1.85%) held, a bigger
        // one (7.4%) published.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 5400,
            fps: 30,
        };

        let mut now = t0 + TICK;
        // 6000 kbps sent; congestion threshold = 0.9*6000 = 5400.
        frame(&mut c, now, 750_000, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 5_300_000,
            },
            now,
        );
        // (5400-5300)/5400 = 1.85% < 5%: held.
        assert_eq!(c.tick(now), None);
        assert_eq!(c.target().bitrate_kbps, 5400);

        now += TICK;
        frame(&mut c, now, 750_000, Duration::from_millis(5));
        c.feedback(
            Feedback::Remb {
                bitrate_bps: 5_000_000,
            },
            now,
        );
        // (5400-5000)/5400 = 7.4% >= 5%: published.
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "remb");
        assert_eq!(decision.target.bitrate_kbps, 5000);
    }

    #[test]
    fn clamps_to_min_bitrate() {
        // Sustained loss halves-ish b every tick (x0.55) until the *clamped*
        // candidate lands within HYSTERESIS of the last published value, at
        // which point it stops being published at all (see `tick`'s doc
        // comment: an unpublished tick doesn't move the committed target) --
        // so the sequence settles just above MIN_BITRATE_KBPS rather than
        // exactly on it: 6000 -> 3300 -> 1815 -> 998 -> 549 -> 302, then
        // 302*0.55=166.1 clamps to 300, but (302-300)/302 = 0.66% < 5% is
        // never published, so it freezes at 302. The invariant that matters
        // (and that the encoder/pipeline actually depend on) is the floor
        // itself: never below MIN_BITRATE_KBPS.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;

        c.feedback(Feedback::Loss { fraction: 0.9 }, now);
        for _ in 0..10 {
            now += TICK;
            c.tick(now);
            assert!(
                c.target().bitrate_kbps >= MIN_BITRATE_KBPS,
                "bitrate must never drop below the floor, got {}",
                c.target().bitrate_kbps
            );
        }
        assert_eq!(c.target().bitrate_kbps, 302);
    }
}
