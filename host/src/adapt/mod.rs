//! Bitrate/fps adaptation controller (slice 2.3): watches REMB/loss feedback
//! from the peer and the capture pipeline's own overrun signal, and decides
//! the `encode::RateTarget` the pipeline's `pipeline::RateControl` should be
//! driving the encoder at.
//!
//! Deliberately free of tokio/async and any I/O: `signaling/mod.rs` owns the
//! task that feeds this controller `Feedback` and calls `tick()` once a
//! second, and applies the resulting `Decision` to `RateControl` and the
//! `control` data channel. Time is always an explicit `Instant` argument so
//! the whole thing is deterministic and unit-testable without sleeping.
//!
//! Lessons from the first live run on the owner's Windows bench (slice 2.3,
//! 2026-09-14), which drove the rules below:
//! - The browser's REMB is a *receive-side* estimate capped at ~1.5x what it
//!   actually received, and it only grows ~8 %/s. On a mostly static screen
//!   it therefore sits far below the encoder's target, and any burst of
//!   changes (typing, a window opening) sends more than REMB "allows". A
//!   naive "REMB < sent means congestion" spiralled the target down to the
//!   floor (0.3 Mbit/s @ 5 fps) on an idle LAN. REMB is only meaningful when
//!   the link is actually loaded -- the encoder is sending close to its
//!   target -- and only when it stays below the sent rate for a while.
//! - Absolute capture-to-encode latency is a poor "encoder can't keep up"
//!   signal: Media Foundation on the bench sits at 30-50 ms per frame under
//!   load and kept up fine at 20 fps, yet a fixed multiple of the target
//!   frame interval read that as overload. The honest signal is the capture
//!   thread overwriting a frame the encoder hasn't taken yet
//!   (`pipeline::PipelineStats::overwritten`): that is exactly "frames arrive
//!   faster than they are encoded".

use std::time::{Duration, Instant};

use proto::control::QualityPreset;

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
/// congestion -- but only while the link is loaded (`REMB_LOADED_RATIO`)
/// and only after `REMB_CONGESTION_TICKS` ticks in a row (see the module
/// doc comment for why a single tick is not enough).
pub const REMB_CONGESTION_RATIO: f64 = 0.9;

/// REMB is only trusted as a congestion signal when the sent rate is at
/// least this fraction of the current target: below that the encoder is
/// content-limited (static screen, typing), the browser's estimate is
/// capped by the tiny received rate, and comparing the two says nothing
/// about the network.
pub const REMB_LOADED_RATIO: f64 = 0.8;

/// How many consecutive ticks the loaded-link REMB signal must persist
/// before the bitrate is cut to it -- filters the burst at session start
/// (a 300 KB IDR in the first second) and one-off REMB dips.
pub const REMB_CONGESTION_TICKS: u32 = 2;

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

/// If the capture thread overwrote at least this many not-yet-encoded
/// frames during a tick (`Feedback::Overrun`), the encoder isn't keeping up
/// with the source and fps steps down.
pub const ENCODER_OVERRUN_FRAMES: u64 = 2;

/// How many consecutive ticks with encoded frames and no overruns are
/// required before stepping fps back up -- avoids flapping between two
/// rungs every couple of seconds.
pub const ENCODER_FAST_TICKS: u32 = 5;

/// Minimum bits-per-pixel-per-frame the target bitrate/fps combination must
/// afford. Below this, more fps just spreads the same bits over more (worse)
/// frames -- ARCHITECTURE.md §5 says to sacrifice fps before quality. This is
/// the `QualityPreset::Auto` budget; `Sharp`/`Smooth` scale it (see
/// `SHARP_BPP_MULTIPLIER`/`SMOOTH_BPP_MULTIPLIER`).
pub const MIN_BITS_PER_PIXEL_PER_FRAME: f64 = 0.02;

/// `QualityPreset::Sharp` (slice 3.5e, "Чёткость"): multiplies the bit budget
/// so the controller gives up fps for frame quality much sooner than `auto`
/// -- meant for text-heavy sessions where legibility matters more than
/// motion smoothness.
pub const SHARP_BPP_MULTIPLIER: f64 = 3.0;

/// `QualityPreset::Smooth` (slice 3.5e, "Плавность"): shrinks the bit budget
/// so the controller tolerates a much softer frame in exchange for keeping
/// fps up -- meant for video/animation-heavy sessions.
pub const SMOOTH_BPP_MULTIPLIER: f64 = 0.5;

/// `QualityPreset::Sharp`'s fps ceiling: text doesn't need more than this to
/// read as smooth, and giving up the headroom lets the bit-budget check
/// (`Controller::bitrate_fps_step`) step fps down before quality earlier
/// than `auto` would. Combined with the session's own configured `max_fps`
/// (the lower of the two applies -- see `QualityPolicy::for_preset`).
pub const SHARP_FPS_CEILING: u32 = 15;

/// `QualityPreset::Smooth`'s fps floor: below this a video/animation reads as
/// broken, so the controller cuts bitrate (down to `MIN_BITRATE_KBPS`)
/// rather than fps once this floor is reached. Combined with the session's
/// own configured `min_fps` (the higher of the two applies -- see
/// `QualityPolicy::for_preset`).
pub const SMOOTH_FPS_FLOOR: u32 = 15;

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
    Frame { bytes: usize },
    /// Captured frames the encoder never got to see because the next one
    /// overwrote them first (`pipeline::PipelineStats::overwritten`, delta
    /// since the previous tick) -- the "encoder can't keep up" signal.
    Overrun { frames: u64 },
    /// `proto::control::ControlMessage::SetQuality` (slice 3.5e), forwarded
    /// as-is from `signaling::handle_session_event`'s `control` channel
    /// handling rather than a separate command channel -- `Controller`
    /// already processes every `Feedback` on arrival (see `feedback`), so
    /// this reuses that same plumbing. Applied immediately (not batched
    /// until the next `tick()` like the other variants): see
    /// `Controller::set_preset`.
    Preset { preset: QualityPreset },
}

/// A change to the encoder's rate target the controller decided on, with the
/// reason shown in the client's overlay (see `proto::control::ControlMessage::Quality`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub target: RateTarget,
    pub reason: &'static str,
}

/// The fps bounds and bit budget `Controller` drives its ladder
/// (`Controller::ladder`) and bit-budget check (`Controller::bitrate_fps_step`)
/// from, derived from `AdaptConfig`'s session ceiling and the current
/// `QualityPreset` (slice 3.5e). `AdaptConfig` itself never changes for a
/// session (it's the `--bitrate`/`--fps` ceiling); this is the layer a
/// preset switch (`Controller::set_preset`) actually mutates.
#[derive(Debug, Clone, Copy)]
struct QualityPolicy {
    max_fps: u32,
    min_fps: u32,
    bpp_budget: f64,
}

impl QualityPolicy {
    /// `auto` uses the session's own ceiling/floor and `MIN_BITS_PER_PIXEL_PER_FRAME`
    /// unchanged; `sharp`/`smooth` narrow the fps range and scale the bit
    /// budget as documented on `SHARP_FPS_CEILING`/`SHARP_BPP_MULTIPLIER`/
    /// `SMOOTH_FPS_FLOOR`/`SMOOTH_BPP_MULTIPLIER`. Bitrate bounds
    /// (`AdaptConfig::min_bitrate_kbps`/`max_bitrate_kbps`) are not part of
    /// this policy -- no preset changes them (see this module's plan doc).
    fn for_preset(preset: QualityPreset, cfg: &AdaptConfig) -> Self {
        match preset {
            QualityPreset::Auto => QualityPolicy {
                max_fps: cfg.max_fps,
                min_fps: cfg.min_fps,
                bpp_budget: MIN_BITS_PER_PIXEL_PER_FRAME,
            },
            QualityPreset::Sharp => QualityPolicy {
                max_fps: cfg.max_fps.min(SHARP_FPS_CEILING),
                min_fps: cfg.min_fps,
                bpp_budget: MIN_BITS_PER_PIXEL_PER_FRAME * SHARP_BPP_MULTIPLIER,
            },
            QualityPreset::Smooth => QualityPolicy {
                max_fps: cfg.max_fps,
                min_fps: cfg.min_fps.max(SMOOTH_FPS_FLOOR).min(cfg.max_fps),
                bpp_budget: MIN_BITS_PER_PIXEL_PER_FRAME * SMOOTH_BPP_MULTIPLIER,
            },
        }
    }
}

/// Accumulates feedback between ticks; see this module's doc comment.
#[derive(Debug, Default)]
struct Window {
    bytes: u64,
    frames: u32,
    overruns: u64,
}

impl Window {
    fn reset(&mut self) {
        *self = Window::default();
    }
}

/// The bitrate/fps adaptation controller. See this module's doc comment for
/// how it's driven.
pub struct Controller {
    cfg: AdaptConfig,
    /// The current quality preset's derived fps bounds/bit budget (slice
    /// 3.5e) -- see `QualityPolicy`'s doc comment. Starts at `Auto`
    /// (`QualityPolicy::for_preset(QualityPreset::Auto, &cfg)`), changed only
    /// by `set_preset`.
    policy: QualityPolicy,
    preset: QualityPreset,
    /// Set by `set_preset`, consumed by the next `tick()`: forces that tick
    /// to report the (already clamped, see `set_preset`) target with
    /// `reason: "preset"` regardless of `HYSTERESIS`, so the client's overlay
    /// picks up the switch right away instead of waiting for unrelated
    /// feedback to move the target past the hysteresis threshold.
    preset_announce_pending: bool,
    target: RateTarget,
    last_remb_kbps: Option<f64>,
    last_remb_at: Option<Instant>,
    /// The fresh REMB seen by the previous `tick()`, to tell a rising
    /// estimate (still ramping, not congestion) from a falling one.
    prev_tick_remb_kbps: Option<f64>,
    last_loss: f32,
    window: Window,
    /// Consecutive ticks the loaded-link REMB signal has said "congested"
    /// (see `REMB_CONGESTION_TICKS`).
    congested_ticks: u32,
    /// Consecutive ticks the encoder has kept up (frames encoded, no
    /// overruns) at the current rung (see `ENCODER_FAST_TICKS`).
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
            policy: QualityPolicy::for_preset(QualityPreset::Auto, &cfg),
            preset: QualityPreset::Auto,
            preset_announce_pending: false,
            target,
            last_remb_kbps: None,
            last_remb_at: None,
            prev_tick_remb_kbps: None,
            last_loss: 0.0,
            window: Window::default(),
            congested_ticks: 0,
            fast_ticks: 0,
            last_tick_at: now,
        }
    }

    /// The controller's current committed rate target (only changes when a
    /// `tick()` call returns `Some`).
    pub fn target(&self) -> RateTarget {
        self.target
    }

    /// The controller's current quality preset (slice 3.5e), starting at
    /// `Auto` and changed only by `set_preset`.
    pub fn preset(&self) -> QualityPreset {
        self.preset
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
            Feedback::Frame { bytes } => {
                self.window.bytes += bytes as u64;
                self.window.frames += 1;
            }
            Feedback::Overrun { frames } => {
                self.window.overruns += frames;
            }
            Feedback::Preset { preset } => {
                self.set_preset(preset, now);
            }
        }
    }

    /// Switches the controller's quality preset (slice 3.5e): rederives
    /// `policy` (`QualityPolicy::for_preset`) and immediately moves the
    /// committed `target` into the new bounds -- fps to the new ceiling
    /// (`policy.max_fps`), the same place `Controller::new` starts: an
    /// explicit choice should show at once, not after `ENCODER_FAST_TICKS`
    /// of probing back up (seen live: sharp -> smooth sat at 15 fps for
    /// ~8 s); the usual feedback steps it down if the encoder or link can't
    /// keep up. Bitrate is clamped to
    /// `[cfg.min_bitrate_kbps, cfg.max_bitrate_kbps]` (defensive: no preset
    /// actually changes these bounds, see `QualityPolicy::for_preset`).
    /// Resets the feedback window and `last_tick_at` to `now` -- the window
    /// accumulated under the old policy shouldn't carry into a decision made
    /// under the new one. Sets `preset_announce_pending` so the very next
    /// `tick()` reports this new target with `reason: "preset"` (see that
    /// field's doc comment) instead of running the usual feedback-driven
    /// logic for that tick.
    pub fn set_preset(&mut self, preset: QualityPreset, now: Instant) {
        self.preset = preset;
        self.policy = QualityPolicy::for_preset(preset, &self.cfg);
        let fps = self.policy.max_fps;
        let bitrate_kbps = self
            .target
            .bitrate_kbps
            .clamp(self.cfg.min_bitrate_kbps, self.cfg.max_bitrate_kbps);
        self.target = RateTarget { bitrate_kbps, fps };
        self.window.reset();
        self.last_tick_at = now;
        self.preset_announce_pending = true;
    }

    /// The fps ladder for this controller's current policy (slice 3.5e:
    /// `self.policy`, not the session's raw `AdaptConfig` -- narrower for
    /// `Sharp`/`Smooth`, see `QualityPolicy::for_preset`): entries of
    /// `FPS_LADDER` within `[policy.min_fps, policy.max_fps]`, plus
    /// `policy.max_fps` itself if it isn't one of them, sorted highest
    /// first.
    fn ladder(&self) -> Vec<u32> {
        let mut steps: Vec<u32> = FPS_LADDER
            .iter()
            .copied()
            .filter(|&f| f >= self.policy.min_fps && f <= self.policy.max_fps)
            .collect();
        if !steps.contains(&self.policy.max_fps) {
            steps.push(self.policy.max_fps);
        }
        steps.sort_unstable_by(|a, b| b.cmp(a));
        steps.dedup();
        steps
    }

    /// Called once per `TICK`. Returns the new target if it changed enough
    /// to be worth publishing (see `HYSTERESIS`), `None` otherwise -- in
    /// which case the controller's committed target is left untouched.
    pub fn tick(&mut self, now: Instant) -> Option<Decision> {
        if self.preset_announce_pending {
            self.preset_announce_pending = false;
            self.window.reset();
            self.last_tick_at = now;
            return Some(Decision {
                target: self.target,
                reason: "preset",
            });
        }

        let elapsed = now
            .saturating_duration_since(self.last_tick_at)
            .as_secs_f64();
        let sent_kbps = if elapsed > 0.0 {
            self.window.bytes as f64 * 8.0 / 1000.0 / elapsed
        } else {
            0.0
        };

        let old_b = self.target.bitrate_kbps as f64;
        let remb_fresh = self
            .last_remb_at
            .is_some_and(|at| now.saturating_duration_since(at) <= REMB_FRESH);
        let remb_kbps = if remb_fresh {
            self.last_remb_kbps
        } else {
            None
        };

        // REMB says "congested" only on a loaded link (see
        // `REMB_LOADED_RATIO`), only when it undercuts what we sent, and
        // only while it is *not rising*: the browser's estimate ramps at
        // ~8 %/s, so right after the content gets heavier (a video starts)
        // it trails our sent rate for many seconds while the network is
        // fine -- seen on the bench as a spurious 4.0 -> 2.6 Mbit/s cut at
        // the start of a video. Real overuse makes the receive-side
        // estimator *lower* its estimate (multiplicative decrease), which
        // is what this looks for.
        let link_loaded = sent_kbps >= REMB_LOADED_RATIO * old_b;
        let remb_rising = match (remb_kbps, self.prev_tick_remb_kbps) {
            (Some(now), Some(prev)) => now > prev,
            _ => false,
        };
        self.prev_tick_remb_kbps = remb_kbps;
        let remb_congested = link_loaded
            && !remb_rising
            && remb_kbps.is_some_and(|remb| remb < sent_kbps * REMB_CONGESTION_RATIO);
        if remb_congested {
            self.congested_ticks += 1;
        } else {
            self.congested_ticks = 0;
        }

        let (mut b, bitrate_reason) = if self.last_loss > LOSS_HIGH {
            (old_b * (1.0 - 0.5 * self.last_loss as f64), Some("loss"))
        } else if remb_congested && self.congested_ticks >= REMB_CONGESTION_TICKS {
            // `remb_congested` implies `remb_kbps` is `Some`.
            let remb = remb_kbps.unwrap_or(old_b);
            (old_b.min(remb), Some("remb"))
        } else if !remb_congested
            && self.last_loss < LOSS_LOW
            && old_b < self.cfg.max_bitrate_kbps as f64
        {
            // Nothing says the path is short of bandwidth: probe upward.
            // REMB itself can't tell us to go up (it is capped by what it
            // received, i.e. by our own content), so this is the only way
            // the target ever recovers after a cut.
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

        let (encoder_fps, encoder_fired) = if self.window.overruns >= ENCODER_OVERRUN_FRAMES {
            self.fast_ticks = 0;
            let stepped = step_down(&steps, current_fps);
            (stepped, stepped != current_fps)
        } else if self.window.frames > 0 && self.window.overruns == 0 {
            let higher = step_up(&steps, current_fps);
            if higher != current_fps {
                self.fast_ticks += 1;
                if self.fast_ticks >= ENCODER_FAST_TICKS {
                    self.fast_ticks = 0;
                    (higher, true)
                } else {
                    (current_fps, false)
                }
            } else {
                (current_fps, false)
            }
        } else {
            // No frames this tick (static screen) or a stray overrun below
            // the threshold: no evidence either way, keep the streak.
            (current_fps, false)
        };

        let bitrate_fps = self.bitrate_fps_step(&steps, b);
        let new_fps = self
            .policy
            .max_fps
            .min(encoder_fps)
            .min(bitrate_fps)
            .max(self.policy.min_fps);
        let bitrate_limited_fps = new_fps == bitrate_fps
            && bitrate_fps < self.policy.max_fps
            && bitrate_fps <= encoder_fps;

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

    /// Largest ladder step for which `b` (kbps) affords at least the current
    /// policy's `bpp_budget` bits/pixel/frame at this controller's
    /// resolution (slice 3.5e: `Sharp`/`Smooth` scale this budget, see
    /// `QualityPolicy::for_preset`); falls back to `policy.min_fps` if even
    /// the smallest step doesn't (see `MIN_BITS_PER_PIXEL_PER_FRAME`'s doc
    /// comment: fewer, better frames beat more, worse ones).
    fn bitrate_fps_step(&self, steps: &[u32], b_kbps: f64) -> u32 {
        let threshold_bits =
            self.cfg.width as f64 * self.cfg.height as f64 * self.policy.bpp_budget;
        for &fps in steps {
            let budget_bits = b_kbps * 1000.0 / fps as f64;
            if budget_bits >= threshold_bits {
                return fps;
            }
        }
        self.policy.min_fps
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

    /// One tick's worth of encoded frames totalling `bytes` -- fed as a
    /// single `Frame` since only the byte sum and "any frames at all" matter
    /// to the controller.
    fn frames(c: &mut Controller, now: Instant, bytes: usize) {
        c.feedback(Feedback::Frame { bytes }, now);
    }

    fn remb(c: &mut Controller, now: Instant, bitrate_bps: u64) {
        c.feedback(Feedback::Remb { bitrate_bps }, now);
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
        for _ in 0..6 {
            now += TICK;
            // Frames and no overruns every tick -- already at the top fps
            // rung and the max bitrate, so nothing can step or probe up.
            frames(&mut c, now, 25_000);
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target().bitrate_kbps, 6000);
        assert_eq!(c.target().fps, 30);
    }

    #[test]
    fn remb_below_sent_rate_on_loaded_link_caps_bitrate_after_two_ticks() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);

        // 750_000 bytes/s = 6000 kbps sent (>= 0.8 * 6000: loaded), REMB
        // 2000 kbps < 0.9 * 6000: congested. First tick only counts.
        let mut now = t0 + TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 2_000_000);
        assert_eq!(c.tick(now), None);
        assert_eq!(c.target().bitrate_kbps, 6000);

        // Second consecutive congested tick: cut to REMB.
        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 2_000_000);
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "remb");
        assert_eq!(decision.target.bitrate_kbps, 2000);
        assert_eq!(decision.target.fps, 30);
    }

    #[test]
    fn remb_below_sent_rate_on_unloaded_link_is_ignored() {
        // The bench scenario: static screen with typing bursts. 1000 kbps
        // sent is far below 0.8 * 6000, so REMB (300 kbps, capped by the
        // tiny received rate) says nothing about the network -- and since
        // b is already at max there's nothing to probe either: hold.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;
        for _ in 0..4 {
            now += TICK;
            frames(&mut c, now, 125_000);
            remb(&mut c, now, 300_000);
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target().bitrate_kbps, 6000);
        assert_eq!(c.target().fps, 30);
    }

    #[test]
    fn rising_remb_below_sent_rate_is_ramping_not_congestion() {
        // The bench's false positive: a video starts, 6000 kbps go out, and
        // the browser's estimate trails behind while climbing 8 %/s. While
        // it rises no cut happens even though it undercuts the sent rate
        // for many ticks in a row.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;
        let mut estimate = 2_000_000u64;
        for _ in 0..6 {
            now += TICK;
            frames(&mut c, now, 750_000);
            remb(&mut c, now, estimate);
            assert_eq!(c.tick(now), None);
            estimate = estimate * 108 / 100;
        }
        assert_eq!(c.target().bitrate_kbps, 6000);

        // The estimate then *falls* two ticks in a row: that is overuse.
        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 2_500_000);
        assert_eq!(c.tick(now), None);
        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 2_400_000);
        let decision = c.tick(now).expect("expected a remb cut");
        assert_eq!(decision.reason, "remb");
        assert_eq!(decision.target.bitrate_kbps, 2400);
    }

    #[test]
    fn remb_above_sent_rate_is_not_congestion() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let now = t0 + TICK;

        // 6000 kbps sent, REMB 9000 >= 0.9 * sent: not congestion, and b is
        // already at max: hold.
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 9_000_000);
        assert_eq!(c.tick(now), None);
        assert_eq!(c.target().bitrate_kbps, 6000);
    }

    #[test]
    fn probe_ramps_up_after_remb_cap() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;
        for _ in 0..2 {
            now += TICK;
            frames(&mut c, now, 750_000);
            remb(&mut c, now, 2_000_000);
            c.tick(now);
        }
        assert_eq!(c.target().bitrate_kbps, 2000);

        // Now sending 2000 kbps and REMB says 5000: not congested, so
        // probe +10 % per tick.
        now += TICK;
        frames(&mut c, now, 250_000);
        remb(&mut c, now, 5_000_000);
        let d1 = c.tick(now).expect("expected a probe decision");
        assert_eq!(d1.reason, "probe");
        assert_eq!(d1.target.bitrate_kbps, 2200);

        now += TICK;
        frames(&mut c, now, 275_000);
        remb(&mut c, now, 5_000_000);
        let d2 = c.tick(now).expect("expected a probe decision");
        assert_eq!(d2.reason, "probe");
        assert_eq!(d2.target.bitrate_kbps, 2420);
        assert!(d2.target.bitrate_kbps <= 6000);
    }

    #[test]
    fn probe_continues_on_static_screen_regardless_of_remb() {
        // After a cut, a mostly static screen (100 kbps sent, REMB 50 kbps
        // -- below 0.9 * sent, but the link isn't loaded) must not pin the
        // target: without probing here the target could never recover
        // until the user happened to produce enough motion.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 2000,
            fps: 30,
        };
        let now = t0 + TICK;
        frames(&mut c, now, 12_500);
        remb(&mut c, now, 50_000);
        let decision = c.tick(now).expect("expected a probe decision");
        assert_eq!(decision.reason, "probe");
        assert_eq!(decision.target.bitrate_kbps, 2200);
    }

    #[test]
    fn probe_without_any_remb() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 2000,
            fps: 30,
        };
        let now = t0 + TICK;
        frames(&mut c, now, 250_000);
        let decision = c.tick(now).expect("expected a probe decision");
        assert_eq!(decision.reason, "probe");
        assert_eq!(decision.target.bitrate_kbps, 2200);
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
    fn encoder_overruns_step_fps_down() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;

        for expected in [24u32, 20, 15] {
            now += TICK;
            frames(&mut c, now, 25_000);
            c.feedback(Feedback::Overrun { frames: 5 }, now);
            let decision = c.tick(now).expect("expected an encoder step down");
            assert_eq!(decision.reason, "encoder");
            assert_eq!(decision.target.fps, expected);
        }

        // A single stray overrun is below the threshold: hold.
        now += TICK;
        frames(&mut c, now, 25_000);
        c.feedback(Feedback::Overrun { frames: 1 }, now);
        assert_eq!(c.tick(now), None);
        assert_eq!(c.target().fps, 15);
    }

    #[test]
    fn clean_ticks_step_fps_back_up_after_five() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;

        for _ in 0..3 {
            now += TICK;
            frames(&mut c, now, 25_000);
            c.feedback(Feedback::Overrun { frames: 5 }, now);
            c.tick(now);
        }
        assert_eq!(c.target().fps, 15);

        // Frames and no overruns: four ticks hold, the fifth steps up.
        for _ in 0..4 {
            now += TICK;
            frames(&mut c, now, 25_000);
            assert_eq!(c.tick(now), None);
        }
        now += TICK;
        frames(&mut c, now, 25_000);
        let decision = c.tick(now).expect("expected an encoder step up");
        assert_eq!(decision.reason, "encoder");
        assert_eq!(decision.target.fps, 20);
    }

    #[test]
    fn static_screen_does_not_count_toward_step_up() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0 + TICK;
        frames(&mut c, now, 25_000);
        c.feedback(Feedback::Overrun { frames: 5 }, now);
        c.tick(now);
        assert_eq!(c.target().fps, 24);

        // Ten ticks with no frames at all: no evidence the encoder keeps
        // up, so no step up.
        for _ in 0..10 {
            now += TICK;
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target().fps, 24);
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
        let mut now = t0;

        // 6000 kbps sent on a loaded link, REMB 300 kbps for two ticks.
        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 300_000);
        assert_eq!(c.tick(now), None);
        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 300_000);
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
        // Target already at 5400 with 6000 kbps sent (loaded). REMB 5300
        // is a 1.85 % drop: congested but held; REMB 5000 (7.4 %) publishes.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 5400,
            fps: 30,
        };

        let mut now = t0;
        for _ in 0..2 {
            now += TICK;
            frames(&mut c, now, 750_000);
            remb(&mut c, now, 5_300_000);
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target().bitrate_kbps, 5400);

        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 5_000_000);
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "remb");
        assert_eq!(decision.target.bitrate_kbps, 5000);
    }

    #[test]
    fn clamps_to_min_bitrate() {
        // Sustained loss halves-ish b every tick (x0.55) until the *clamped*
        // candidate lands within HYSTERESIS of the last published value, at
        // which point it stops being published at all (an unpublished tick
        // doesn't move the committed target) -- so the sequence settles
        // just above MIN_BITRATE_KBPS rather than exactly on it:
        // 6000 -> 3300 -> 1815 -> 998 -> 549 -> 302, then 302*0.55=166.1
        // clamps to 300, but (302-300)/302 = 0.66% < 5% is never published.
        // The invariant that matters is the floor itself.
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

    // --- Slice 3.5e: quality presets ---------------------------------

    #[test]
    fn set_preset_sharp_clamps_fps_ceiling_and_announces_next_tick() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        assert_eq!(c.target().fps, 30);

        // Via `Feedback::Preset`, the same path `signaling::run_adapt_task`
        // uses for a `ControlMessage::SetQuality` arriving from the client.
        let now = t0 + TICK;
        c.feedback(
            Feedback::Preset {
                preset: QualityPreset::Sharp,
            },
            now,
        );
        assert_eq!(c.preset(), QualityPreset::Sharp);
        // Clamped immediately by `set_preset`, before any `tick()` call.
        assert_eq!(c.target().fps, 15);
        assert_eq!(c.target().bitrate_kbps, 6000);

        let decision = c
            .tick(now + TICK)
            .expect("expected the forced preset announcement");
        assert_eq!(decision.reason, "preset");
        assert_eq!(decision.target.fps, 15);
        assert_eq!(decision.target.bitrate_kbps, 6000);
    }

    #[test]
    fn set_preset_starts_at_the_new_ceiling_even_from_a_low_fps() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        // Drive fps down well below 15 via sustained encoder overruns
        // (auto's floor is `MIN_FPS` = 5).
        let mut now = t0;
        for _ in 0..5 {
            now += TICK;
            frames(&mut c, now, 25_000);
            c.feedback(Feedback::Overrun { frames: 5 }, now);
            c.tick(now);
        }
        assert!(
            c.target().fps < 15,
            "fps must have dropped below 15 by now, got {}",
            c.target().fps
        );

        now += TICK;
        c.set_preset(QualityPreset::Smooth, now);
        assert_eq!(c.target().fps, 30);
    }

    #[test]
    fn sharp_bit_budget_steps_fps_down_before_auto_at_the_same_bitrate() {
        // max_fps=10 keeps `SHARP_FPS_CEILING` (15) from being the reason
        // sharp differs here -- only the 3x bit budget is under test.
        let cfg = AdaptConfig {
            max_bitrate_kbps: 6000,
            min_bitrate_kbps: MIN_BITRATE_KBPS,
            max_fps: 10,
            min_fps: MIN_FPS,
            width: 1280,
            height: 720,
        };
        let t0 = Instant::now();
        let auto = Controller::new(cfg, t0);
        let mut sharp = Controller::new(cfg, t0);
        sharp.set_preset(QualityPreset::Sharp, t0);

        // 1280x720: auto threshold = 921_600 * 0.02 = 18_432 bits/frame,
        // sharp threshold = 921_600 * 0.06 = 55_296 bits/frame. At 450 kbps,
        // auto's top rung (10fps, 45_000 bits/frame) affords it; sharp's
        // doesn't (45_000 < 55_296), so sharp steps down to 8fps (56_250
        // bits/frame, which does fit) on the exact same input.
        let steps_auto = auto.ladder();
        let steps_sharp = sharp.ladder();
        assert_eq!(auto.bitrate_fps_step(&steps_auto, 450.0), 10);
        assert_eq!(sharp.bitrate_fps_step(&steps_sharp, 450.0), 8);
    }

    #[test]
    fn smooth_never_steps_fps_below_the_floor_on_sustained_overruns() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0 + TICK;
        c.set_preset(QualityPreset::Smooth, now);
        let announce = c.tick(now).expect("expected the preset announcement");
        assert_eq!(announce.reason, "preset");
        assert_eq!(announce.target.fps, 30);

        for expected in [24u32, 20, 15] {
            now += TICK;
            frames(&mut c, now, 25_000);
            c.feedback(Feedback::Overrun { frames: 5 }, now);
            let decision = c.tick(now).expect("expected an encoder step down");
            assert_eq!(decision.target.fps, expected);
        }

        // Further overruns cannot push fps below the smooth floor (15):
        // `ladder()` itself has no rung below it while this preset is
        // active, so `step_down` has nowhere left to go.
        for _ in 0..5 {
            now += TICK;
            frames(&mut c, now, 25_000);
            c.feedback(Feedback::Overrun { frames: 5 }, now);
            assert_eq!(c.tick(now), None);
            assert_eq!(c.target().fps, 15);
        }
    }

    #[test]
    fn smooth_floors_fps_and_cuts_bitrate_instead_when_the_bit_budget_is_too_tight() {
        // 1920x1080, smooth budget = 1920*1080*0.01 = 20_736 bits/frame --
        // even the smooth floor (15fps) doesn't afford a REMB-capped 300
        // kbps (300_000/15 = 20_000 < 20_736), but `bitrate_fps_step` must
        // still floor at 15 rather than falling through to `MIN_FPS` (5) the
        // way `low_bitrate_limits_fps` (auto) does at the same resolution.
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
        c.set_preset(QualityPreset::Smooth, t0);
        c.tick(t0).expect("expected the preset announcement");
        let mut now = t0;

        // Loaded-link REMB pins bitrate to 300 kbps over two ticks (same
        // scenario as `low_bitrate_limits_fps`).
        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 300_000);
        assert_eq!(c.tick(now), None);
        now += TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 300_000);
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "remb");
        assert_eq!(decision.target.bitrate_kbps, 300);
        assert_eq!(
            decision.target.fps, 15,
            "smooth must floor fps at 15, never fall through to MIN_FPS"
        );
    }

    #[test]
    fn switching_back_to_auto_restores_the_original_policy() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;

        c.set_preset(QualityPreset::Sharp, now);
        c.tick(now).expect("expected the preset announcement");
        assert_eq!(c.target().fps, 15);

        now += TICK;
        c.set_preset(QualityPreset::Auto, now);
        assert_eq!(c.preset(), QualityPreset::Auto);
        let announce = c.tick(now).expect("expected the preset announcement");
        assert_eq!(announce.reason, "preset");

        // Bounds/budget are back to auto's, even though the committed fps
        // itself only recovers through the usual feedback-driven probing
        // (not an automatic jump on preset switch -- see `set_preset`'s doc
        // comment).
        let steps = c.ladder();
        assert_eq!(steps, vec![30, 24, 20, 15, 12, 10, 8]);
        assert_eq!(c.bitrate_fps_step(&steps, 6000.0), 30);
    }
}
