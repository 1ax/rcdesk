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
//!   thread overwriting a frame the encoder hasn't taken yet -- but only
//!   after the encoder has been busy for a whole target frame interval
//!   (`pipeline::PipelineStats::overruns`). A raw overwrite count also
//!   fires when the source is merely faster than the encoder (WGC at the
//!   monitor refresh rate): Windows host 0.2.0 live log, 2026-10-23, fps
//!   pinned at 8-15 with ~28 ms encodes against a 125 ms budget.
//! - Ignoring REMB on an unloaded link let the target sit at 3x the real
//!   capacity (second user's live log, 2026-10-14: ~2 Mbit/s channel, target
//!   5800, loss up to 42 %): a burst overran the channel, each loss cut the
//!   target by 10-20 % and probing climbed straight back. After observed loss
//!   REMB therefore becomes a ceiling for `LOSS_CAP_HOLD`.
//! - On the second live check (2026-10-14) fps got stuck at 5 for 36 minutes:
//!   the `min_fps` floor returned by `bitrate_fps_step` is not a rung of the
//!   fps ladder, and `step_up` could not find it there.
//! - A REMB at the browser estimator's floor is garbage, not a channel
//!   measurement (Windows host 0.1.15 live log, 2026-10-21, 2560x1440: REMB
//!   5-11 kbit/s for ~33 minutes at zero loss while 4.5 Mbit/s flowed fine).
//!   Values below `REMB_IGNORE_BELOW_KBPS` are treated as "no REMB"; real
//!   overload is still cut by the loss path (`LOSS_HIGH`).
//! - With that junk REMB discarded the REMB ceiling never armed, and probing
//!   took the target from a `loss` cut back to 6000 within 1-3 s -- a saw
//!   on a congested channel (Windows host 0.2.0 live log, 2026-10-23,
//!   08:00:45-08:00:53). Loss itself therefore pauses probing for
//!   `LOSS_PROBE_HOLD` and, on a loaded link, caps the target at what was
//!   actually sent, independent of REMB.
//! - `fraction_lost` of a report covering a few packets is noise: Mac host
//!   0.2.3 live log, 2026-10-26, 1 lost of 2 sent (0.496) on a static screen
//!   cut the target every 30-60 s for hours. A report counts as loss only if
//!   the cumulative lost counter grew by `MIN_LOST_PACKETS` or more.

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

/// A REMB below this (kbit/s) is treated as absent. libwebrtc's estimator
/// bottoms out at ~5 kbit/s; the live log of Windows host 0.1.15
/// (2026-10-21) showed REMB 5-11 kbit/s for most of a 33-minute session at
/// `loss=0.000` with a 4.5 Mbit/s stream flowing. A real channel that narrow
/// is not a thing for a remote desktop, and genuine overload is still cut by
/// the `LOSS_HIGH` loss path, so such values are estimator junk, not a signal.
pub const REMB_IGNORE_BELOW_KBPS: f64 = 50.0;

/// A loss fraction from an RTCP RR older than this counts as zero. Chrome
/// sends RRs about once a second; when they stop, acting on the last value
/// forever would cut the target to the floor every tick, so instead the
/// target recovers by probing.
pub const LOSS_FRESH: Duration = Duration::from_secs(3);

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

/// How long after the last observed loss (with a fresh REMB at the time) the
/// REMB ceiling (`LOSS_CAP_RATIO`) stays in force even though the link looks
/// unloaded. Live log of the second user, 2026-10-14: channel ~2 Mbit/s,
/// REMB 1986, target 5800, loss up to 42 %.
pub const LOSS_CAP_HOLD: Duration = Duration::from_secs(10);

/// How long after the last observed loss >= `LOSS_LOW` probing stays off,
/// with or without REMB. Without it a `loss` cut was undone by probing within
/// 1-3 s while the browser's REMB lay at its floor (Windows host 0.2.0 live
/// log, 2026-10-23).
pub const LOSS_PROBE_HOLD: Duration = Duration::from_secs(10);

/// While the loss ceiling is active the target may not exceed this fraction
/// of the fresh REMB (second user's live log, 2026-10-14: channel ~2 Mbit/s,
/// REMB 1986, target 5800, loss up to 42 %).
pub const LOSS_CAP_RATIO: f64 = 0.9;

/// A receiver report counts as loss only if at least this many packets were
/// lost since the previous report. `fraction_lost` of a report covering a
/// handful of packets is sampling noise: Mac host 0.2.3 live log, 2026-10-26,
/// 05:49-09:03Z, static screen sent 19-31 kbit/s (3-15 packets/s) and 211
/// `loss` cuts followed, e.g. loss=0.496 from 1 lost packet out of 2, each
/// cutting the target to 1000/8 then 834/5 for hours.
pub const MIN_LOST_PACKETS: u32 = 3;

/// The loss ceiling never drops below this (kbps). On a static screen the
/// browser's REMB is "hungry" (capped at ~1.5x of what it received) and says
/// nothing about the channel capacity: Mac log 0.1.9, 1920x1080, REMB 68-115
/// kbps collapsed the target to `MIN_BITRATE_KBPS`. A genuinely narrow channel
/// is still cut by the `LOSS_HIGH` and REMB-congestion paths; this is only an
/// additional ceiling.
pub const LOSS_CAP_FLOOR_KBPS: f64 = 1000.0;

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
    /// Derived from `transport::SessionEvent::ReceiverReport`: `fraction_lost`
    /// plus `total_lost`, the report's cumulative count of lost packets (the
    /// controller works with its delta between reports, see `MIN_LOST_PACKETS`).
    Loss { fraction: f32, total_lost: u32 },
    /// RTT and jitter from the same RTCP receiver report as `Loss`. Pure
    /// diagnostics for the `video: 5s summary` line; the controller ignores it.
    ReceiverStats {
        rtt: Option<std::time::Duration>,
        jitter_ms: f32,
    },
    /// One encoded frame, reported by the forward task in `signaling/mod.rs`
    /// for every frame the pipeline produces.
    ///
    /// `captured_at` is the moment the frame was captured; it only feeds the
    /// frame-pacing diagnostics in the summary line, the controller ignores it.
    Frame {
        bytes: usize,
        captured_at: std::time::Instant,
    },
    /// Captured frames overwritten while the encoder had been busy for a
    /// whole target frame interval (`pipeline::PipelineStats::overruns`,
    /// delta since the previous tick) -- the "encoder can't keep up" signal.
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
    last_loss_at: Option<Instant>,
    /// Cumulative lost-packet counter of the previous receiver report, to
    /// turn it into a per-report delta (see `MIN_LOST_PACKETS`).
    prev_total_lost: Option<u32>,
    /// Until when REMB acts as a bitrate ceiling after observed loss (see
    /// `LOSS_CAP_HOLD`); re-armed by every tick that sees fresh loss
    /// >= `LOSS_LOW` together with a fresh REMB.
    loss_cap_until: Option<Instant>,
    /// Until when probing is off after observed loss (see `LOSS_PROBE_HOLD`);
    /// re-armed by every tick that sees fresh loss >= `LOSS_LOW`.
    probe_hold_until: Option<Instant>,
    /// The bitrate ceiling (kbps) during `probe_hold_until`: the lowest sent
    /// rate of a loaded tick (`REMB_LOADED_RATIO`) that saw the loss, never
    /// below `LOSS_CAP_FLOOR_KBPS`. `None` if no such tick armed it -- on an
    /// unloaded link the sent rate is content, not channel, and says nothing.
    sent_cap_kbps: Option<f64>,
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
            last_loss_at: None,
            prev_total_lost: None,
            loss_cap_until: None,
            probe_hold_until: None,
            sent_cap_kbps: None,
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
            Feedback::Loss {
                fraction,
                total_lost,
            } => {
                let lost = total_lost.saturating_sub(self.prev_total_lost.unwrap_or(0));
                self.prev_total_lost = Some(total_lost);
                // An insignificant report is still a fresh "no loss" sample.
                self.last_loss = if lost >= MIN_LOST_PACKETS {
                    fraction
                } else {
                    0.0
                };
                self.last_loss_at = Some(now);
            }
            Feedback::ReceiverStats { .. } => {}
            Feedback::Frame { bytes, .. } => {
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
            self.last_remb_kbps.filter(|&r| r >= REMB_IGNORE_BELOW_KBPS)
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

        let loss = if self
            .last_loss_at
            .is_some_and(|at| now.saturating_duration_since(at) <= LOSS_FRESH)
        {
            self.last_loss
        } else {
            0.0
        };
        if loss >= LOSS_LOW && remb_kbps.is_some() {
            self.loss_cap_until = Some(now + LOSS_CAP_HOLD);
        }
        let hold_active = self.probe_hold_until.is_some_and(|until| now < until);
        if !hold_active {
            self.sent_cap_kbps = None;
        }
        if loss >= LOSS_LOW {
            self.probe_hold_until = Some(now + LOSS_PROBE_HOLD);
            if link_loaded {
                let sent = sent_kbps.max(LOSS_CAP_FLOOR_KBPS);
                self.sent_cap_kbps = Some(self.sent_cap_kbps.map_or(sent, |c| c.min(sent)));
            }
        }
        let probe_held = self.probe_hold_until.is_some_and(|until| now < until);
        let loss_cap = remb_kbps
            .filter(|_| self.loss_cap_until.is_some_and(|until| now < until))
            .map(|r| (r * LOSS_CAP_RATIO).max(LOSS_CAP_FLOOR_KBPS));
        let (mut b, mut bitrate_reason) = if loss > LOSS_HIGH {
            (old_b * (1.0 - 0.5 * loss as f64), Some("loss"))
        } else if remb_congested && self.congested_ticks >= REMB_CONGESTION_TICKS {
            // `remb_congested` implies `remb_kbps` is `Some`.
            let remb = remb_kbps.unwrap_or(old_b);
            (old_b.min(remb), Some("remb"))
        } else if !remb_congested
            && !probe_held
            && loss < LOSS_LOW
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
        if let Some(cap) = loss_cap {
            if b > cap {
                b = cap;
                if matches!(bitrate_reason, Some("probe") | None) {
                    bitrate_reason = Some("remb");
                }
            }
        }
        if let Some(cap) = self.sent_cap_kbps.filter(|_| probe_held) {
            if b > cap {
                b = cap;
                if bitrate_reason.is_none() {
                    bitrate_reason = Some("loss");
                }
            }
        }
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
/// `current` if there is none. `current` may be off the ladder
/// (`policy.min_fps` is the fallback floor of `bitrate_fps_step`); then the
/// nearest rung below it is taken.
fn step_down(steps: &[u32], current: u32) -> u32 {
    steps
        .iter()
        .copied()
        .find(|&s| s < current)
        .unwrap_or(current)
}

/// The next higher rung than `current` in `steps` (sorted highest first), or
/// `current` if there is none. `current` may be off the ladder
/// (`policy.min_fps` is the fallback floor of `bitrate_fps_step`); then the
/// nearest rung above it is taken.
fn step_up(steps: &[u32], current: u32) -> u32 {
    steps
        .iter()
        .copied()
        .rev()
        .find(|&s| s > current)
        .unwrap_or(current)
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

    /// Feeds a receiver report with `fraction` whose cumulative lost counter
    /// grows by a large step, so it always counts as significant loss.
    fn loss(c: &mut Controller, now: Instant, fraction: f32) {
        let total_lost = c.prev_total_lost.unwrap_or(0) + 100;
        c.feedback(
            Feedback::Loss {
                fraction,
                total_lost,
            },
            now,
        );
    }

    /// One tick's worth of encoded frames totalling `bytes` -- fed as a
    /// single `Frame` since only the byte sum and "any frames at all" matter
    /// to the controller.
    fn frames(c: &mut Controller, now: Instant, bytes: usize) {
        c.feedback(
            Feedback::Frame {
                bytes,
                captured_at: now,
            },
            now,
        );
    }

    fn remb(c: &mut Controller, now: Instant, bitrate_bps: u64) {
        c.feedback(Feedback::Remb { bitrate_bps }, now);
    }

    #[test]
    fn receiver_stats_feedback_is_ignored() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let before = c.target();
        let mut now = t0;
        for _ in 0..3 {
            now += TICK;
            frames(&mut c, now, 25_000);
            c.feedback(
                Feedback::ReceiverStats {
                    rtt: Some(Duration::from_millis(900)),
                    jitter_ms: 500.0,
                },
                now,
            );
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target(), before);
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
    fn junk_remb_on_loaded_link_does_not_cut_target() {
        // Live log 2026-10-21: REMB 5 kbit/s at zero loss on a loaded link.
        // Treated as no REMB: no "remb" cut, the target stays at max.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;
        for _ in 0..6 {
            now += TICK;
            frames(&mut c, now, 750_000);
            remb(&mut c, now, 5_000);
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target().bitrate_kbps, 6000);
    }

    #[test]
    fn junk_remb_on_loaded_link_lets_probe_climb() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 2000,
            fps: 30,
        };
        let mut now = t0;
        for _ in 0..3 {
            now += TICK;
            let sent = c.target().bitrate_kbps as usize * 1000 / 8;
            frames(&mut c, now, sent);
            remb(&mut c, now, 5_000);
            if let Some(d) = c.tick(now) {
                assert_eq!(d.reason, "probe");
            }
        }
        assert!(c.target().bitrate_kbps > 2000, "{:?}", c.target());
    }

    #[test]
    fn junk_remb_with_moderate_loss_does_not_arm_loss_cap() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 4000,
            fps: 30,
        };
        let mut now = t0;
        for _ in 0..5 {
            now += TICK;
            frames(&mut c, now, 500_000);
            remb(&mut c, now, 5_000);
            loss(&mut c, now, 0.05);
            c.tick(now);
            assert!(
                c.target().bitrate_kbps as f64 > LOSS_CAP_FLOOR_KBPS,
                "{:?}",
                c.target()
            );
        }
        assert!(c.target().bitrate_kbps >= 4000, "{:?}", c.target());
    }

    #[test]
    fn remb_above_ignore_threshold_on_loaded_link_still_cuts() {
        // Control for the junk-REMB tests: 2000 kbit/s is a real signal.
        // (Same scenario as `remb_below_sent_rate_on_loaded_link_caps_...`.)
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;
        let mut last = None;
        for _ in 0..2 {
            now += TICK;
            frames(&mut c, now, 750_000);
            remb(&mut c, now, 2_000_000);
            last = c.tick(now);
        }
        let d = last.expect("expected a remb cut");
        assert_eq!(d.reason, "remb");
        assert_eq!(d.target.bitrate_kbps, 2000);
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

        loss(&mut c, now, 0.2);
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "loss");
        assert_eq!(decision.target.bitrate_kbps, 5400);
        assert_eq!(decision.target.fps, 30);
    }

    #[test]
    fn loss_with_fresh_remb_caps_target_at_remb_and_probe_waits_for_hold() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 5800,
            fps: 30,
        };
        let mut now = t0 + TICK;

        frames(&mut c, now, 375_000);
        remb(&mut c, now, 1_986_000);
        loss(&mut c, now, 0.42);
        let d = c.tick(now).expect("expected a loss decision");
        assert_eq!(d.reason, "loss");
        assert_eq!(d.target.bitrate_kbps, 1787);
        let last_loss_at = now;

        // The old loss stays fresh for +1..+3 s and keeps re-arming both
        // holds (last arming at +3 s); until +13 s the target never exceeds
        // the cap and nothing probes.
        let hold_end = last_loss_at + LOSS_FRESH + LOSS_PROBE_HOLD;
        assert_eq!(LOSS_PROBE_HOLD, LOSS_CAP_HOLD);
        while now + TICK < hold_end {
            now += TICK;
            frames(&mut c, now, 100_000);
            remb(&mut c, now, 1_986_000);
            if let Some(d) = c.tick(now) {
                assert_ne!(d.reason, "probe", "{d:?}");
            }
            assert!(c.target().bitrate_kbps <= 1787, "{:?}", c.target());
        }

        // Both holds are over: probing resumes and climbs past the old cap.
        now += TICK;
        assert!(now >= hold_end);
        frames(&mut c, now, 100_000);
        remb(&mut c, now, 1_986_000);
        let d = c.tick(now).expect("expected a probe");
        assert_eq!(d.reason, "probe");
        while c.target().bitrate_kbps <= 1787 {
            now += TICK;
            frames(&mut c, now, 100_000);
            remb(&mut c, now, 1_986_000);
            c.tick(now);
            assert!(now < hold_end + 10 * TICK, "{:?}", c.target());
        }
    }
    #[test]
    fn loss_without_remb_pauses_probe_for_hold() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0 + TICK;

        loss(&mut c, now, 0.2);
        let d = c.tick(now).expect("expected a decision");
        assert_eq!(d.reason, "loss");
        assert_eq!(d.target.bitrate_kbps, 5400);
        let loss_at = now;

        // Same cuts while the loss stays fresh (last arming at +3 s)...
        for _ in 0..3 {
            now += TICK;
            c.tick(now).expect("expected a loss cut");
        }
        assert_eq!(c.target().bitrate_kbps, 3937);
        // ...then no probe until LOSS_PROBE_HOLD after that arming.
        let hold_end = loss_at + LOSS_FRESH + LOSS_PROBE_HOLD;
        while now + TICK < hold_end {
            now += TICK;
            assert_eq!(c.tick(now), None);
        }
        now += TICK;
        let d = c.tick(now).expect("expected a probe");
        assert_eq!(d.reason, "probe");
        assert_eq!(d.target.bitrate_kbps, 4331);
    }
    #[test]
    fn small_loss_below_loss_low_does_not_arm_the_cap() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 3000,
            fps: 30,
        };
        let now = t0 + TICK;

        frames(&mut c, now, 125_000);
        remb(&mut c, now, 2_000_000);
        loss(&mut c, now, 0.01);
        let d = c.tick(now).expect("expected a probe");
        assert_eq!(d.reason, "probe");
        assert_eq!(d.target.bitrate_kbps, 3300);
    }

    #[test]
    fn cap_is_dropped_when_remb_goes_stale() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 5800,
            fps: 30,
        };
        let mut now = t0 + TICK;

        frames(&mut c, now, 375_000);
        remb(&mut c, now, 1_986_000);
        loss(&mut c, now, 0.42);
        let d = c.tick(now).expect("expected a loss decision");
        assert_eq!(d.target.bitrate_kbps, 1787);
        let armed_at = now;

        // No more REMB: it is fresh for REMB_FRESH (+1..+3 s), stale from
        // +4 s on. Probing waits for LOSS_PROBE_HOLD after the last fresh
        // loss (+3 s), then runs without the REMB ceiling and climbs past
        // the old cap well before the stale REMB could have lifted it.
        let hold_end = armed_at + LOSS_FRESH + LOSS_PROBE_HOLD;
        while now + TICK < hold_end {
            now += TICK;
            c.tick(now);
            assert!(c.target().bitrate_kbps <= 1787, "{:?}", c.target());
        }
        now += TICK;
        let d = c.tick(now).expect("expected an uncapped probe");
        assert_eq!(d.reason, "probe");
        while c.target().bitrate_kbps <= 1787 {
            now += TICK;
            assert!(now < hold_end + 10 * TICK, "{:?}", c.target());
            c.tick(now);
        }
    }
    #[test]
    fn stale_loss_does_not_keep_cutting() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0 + TICK;

        loss(&mut c, now, 0.2);
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.reason, "loss");
        assert_eq!(decision.target.bitrate_kbps, 5400);

        // Within LOSS_FRESH (ages 1 s, 2 s, 3 s) the old RR still counts:
        // three more cuts of x0.9, 5400 -> 4860 -> 4374 -> 3937.
        for expected in [4860u32, 4374, 3937] {
            now += TICK;
            let d = c.tick(now).expect("expected a loss cut");
            assert_eq!(d.reason, "loss");
            assert_eq!(d.target.bitrate_kbps, expected);
        }
        // Age 4 s > LOSS_FRESH: loss counts as zero -- no more cuts (and
        // no probe yet, see `LOSS_PROBE_HOLD`).
        for _ in 0..5 {
            now += TICK;
            assert_eq!(c.tick(now), None);
        }
        assert_eq!(c.target().bitrate_kbps, 3937);
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
    fn step_up_and_step_down_handle_off_ladder_current() {
        let steps = [30, 24, 20, 15, 12, 10, 8];
        assert_eq!(step_up(&steps, 5), 8);
        assert_eq!(step_down(&steps, 5), 5);
        assert_eq!(step_up(&steps, 12), 15);
        assert_eq!(step_down(&steps, 12), 10);
        assert_eq!(step_up(&steps, 30), 30);
        assert_eq!(step_down(&steps, 8), 8);
        assert_eq!(step_up(&steps, 9), 10);
        assert_eq!(step_down(&steps, 9), 8);
    }

    #[test]
    fn fps_recovers_from_min_fps_floor_after_bitrate_recovers() {
        // 2560x1440: 73_728 bits/frame, so 8 fps needs ~590 kbps; below that
        // `bitrate_fps_step` falls through to `MIN_FPS`, which is not a rung
        // of the ladder (live log, 2026-10-14).
        let cfg = AdaptConfig {
            width: 2560,
            height: 1440,
            ..cfg()
        };
        let t0 = Instant::now();
        let mut c = Controller::new(cfg, t0);
        let mut now = t0;
        for _ in 0..5 {
            now += TICK;
            frames(&mut c, now, 1_000);
            loss(&mut c, now, 0.9);
            c.tick(now);
        }
        assert_eq!(c.target().fps, MIN_FPS);
        assert!(c.target().bitrate_kbps < 590);

        // Loss gone, frames every tick, no overruns: probing grows the
        // bitrate and the encoder streak must lift fps off the floor.
        let mut max_fps_seen = MIN_FPS;
        for _ in 0..60 {
            now += TICK;
            frames(&mut c, now, 1_000);
            c.tick(now);
            max_fps_seen = max_fps_seen.max(c.target().fps);
        }
        assert!(c.target().bitrate_kbps > 590);
        assert!(c.target().fps > MIN_FPS, "fps stuck at {}", c.target().fps);
        assert!(max_fps_seen >= 8);
    }

    #[test]
    fn loss_with_junk_remb_does_not_saw_back_to_max() {
        // Windows host 0.2.0 live log, 2026-10-23, 08:00:45-08:00:53: REMB
        // at the estimator floor, a loss burst cuts the target, and probing
        // used to take it back to 6000 within 1-3 s. Now it holds.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0 + TICK;
        frames(&mut c, now, 750_000);
        remb(&mut c, now, 9_000);
        loss(&mut c, now, 0.3);
        let d = c.tick(now).expect("expected a loss cut");
        assert_eq!(d.reason, "loss");
        assert_eq!(d.target.bitrate_kbps, 5100);
        let loss_at = now;

        // RRs keep coming with zero loss; the screen keeps sending the
        // whole target. No decision may raise the target during the hold.
        let mut floor = c.target().bitrate_kbps;
        while now + TICK < loss_at + LOSS_PROBE_HOLD {
            now += TICK;
            let sent = c.target().bitrate_kbps as usize * 1000 / 8;
            frames(&mut c, now, sent);
            remb(&mut c, now, 9_000);
            loss(&mut c, now, 0.0);
            c.tick(now);
            assert!(c.target().bitrate_kbps <= floor, "{:?}", c.target());
            floor = c.target().bitrate_kbps;
        }
        assert_eq!(c.target().bitrate_kbps, 5100);

        now += TICK;
        frames(&mut c, now, 637_500);
        remb(&mut c, now, 9_000);
        loss(&mut c, now, 0.0);
        let d = c.tick(now).expect("expected a probe after the hold");
        assert_eq!(d.reason, "probe");
        assert_eq!(d.target.bitrate_kbps, 5610);
    }

    #[test]
    fn moderate_loss_on_loaded_link_caps_target_at_sent_rate() {
        // 4200 kbps sent against a 5000 target (>= 0.8: loaded) with 5 %
        // loss and no usable REMB: the channel didn't carry 4200 cleanly,
        // so the target drops to it and stays there for the hold.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 5000,
            fps: 30,
        };
        let mut now = t0 + TICK;
        frames(&mut c, now, 525_000);
        loss(&mut c, now, 0.05);
        let d = c.tick(now).expect("expected a cut to the sent rate");
        assert_eq!(d.reason, "loss");
        assert_eq!(d.target.bitrate_kbps, 4200);
        let loss_at = now;

        // The RR stays fresh for LOSS_FRESH and re-arms the hold. Heavier
        // content meanwhile doesn't lift the ceiling.
        while now + TICK < loss_at + LOSS_FRESH + LOSS_PROBE_HOLD {
            now += TICK;
            frames(&mut c, now, 750_000);
            assert_eq!(c.tick(now), None);
        }
        now += TICK;
        frames(&mut c, now, 525_000);
        let d = c.tick(now).expect("expected a probe after the hold");
        assert_eq!(d.reason, "probe");
        assert_eq!(d.target.bitrate_kbps, 4620);
    }

    #[test]
    fn moderate_loss_on_unloaded_link_only_pauses_probe() {
        // Static screen: 1000 kbps sent against a 3000 target. The sent
        // rate is content, not channel, so it doesn't become a ceiling --
        // but probing still waits.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        c.target = RateTarget {
            bitrate_kbps: 3000,
            fps: 30,
        };
        let mut now = t0 + TICK;
        frames(&mut c, now, 125_000);
        loss(&mut c, now, 0.05);
        assert_eq!(c.tick(now), None);
        let loss_at = now;
        while now + TICK < loss_at + LOSS_FRESH + LOSS_PROBE_HOLD {
            now += TICK;
            frames(&mut c, now, 125_000);
            assert_eq!(c.tick(now), None);
        }
        now += TICK;
        frames(&mut c, now, 125_000);
        let d = c.tick(now).expect("expected a probe after the hold");
        assert_eq!(d.reason, "probe");
        assert_eq!(d.target.bitrate_kbps, 3300);
    }

    #[test]
    fn loss_cap_never_drops_below_floor_on_starved_remb() {
        // Static screen: little is sent, the browser's REMB is starved
        // (115 kbps). 0.9 * 115 would collapse the target to 300; the floor
        // keeps it at 1000. At 1280x720 (18_432 bits/frame) 1000 kbps
        // affords 30 fps (33_333 bits/frame), so fps stays 30.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let now = t0 + TICK;
        frames(&mut c, now, 1_000);
        remb(&mut c, now, 115_000);
        loss(&mut c, now, 0.05);
        let decision = c.tick(now).expect("expected a decision");
        assert_eq!(decision.target.bitrate_kbps, 1000);
        assert_eq!(decision.target.fps, 30);
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
        // The invariant that matters is the floor itself. Loss is re-fed
        // before every tick, since it goes stale after LOSS_FRESH.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;

        for _ in 0..10 {
            now += TICK;
            loss(&mut c, now, 0.9);
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

    /// Feeds a receiver report with an explicit cumulative lost counter.
    fn loss_total(c: &mut Controller, now: Instant, fraction: f32, total_lost: u32) {
        c.feedback(
            Feedback::Loss {
                fraction,
                total_lost,
            },
            now,
        );
    }

    #[test]
    fn loss_report_with_too_few_lost_packets_is_ignored() {
        // Mac host 0.2.3 live log: 1 lost of 2 packets (0.496) on a static
        // screen. Unloaded link (1000 kbps sent vs 6000 target), fresh REMB
        // ~500 kbps, one lost packet per report: no cut of any kind.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0;
        for i in 0..8u32 {
            now += TICK;
            frames(&mut c, now, 125_000);
            remb(&mut c, now, 500_000);
            loss_total(&mut c, now, 0.496, 1 + i);
            assert_eq!(c.tick(now), None, "tick {i}");
        }
        assert_eq!(c.target().bitrate_kbps, 6000);
        assert_eq!(c.target().fps, 30);
    }

    #[test]
    fn loss_report_with_enough_lost_packets_still_cuts() {
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let now = t0 + TICK;
        frames(&mut c, now, 125_000);
        remb(&mut c, now, 500_000);
        loss_total(&mut c, now, 0.2, MIN_LOST_PACKETS);
        let d = c.tick(now).expect("expected a loss cut");
        assert_eq!(d.reason, "loss");
        assert!(d.target.bitrate_kbps < 6000, "{d:?}");
    }

    #[test]
    fn first_report_uses_cumulative_total_and_decreasing_counter_is_zero() {
        // First report: the delta is the cumulative total itself.
        let t0 = Instant::now();
        let mut c = Controller::new(cfg(), t0);
        let now = t0 + TICK;
        frames(&mut c, now, 125_000);
        loss_total(&mut c, now, 0.2, MIN_LOST_PACKETS);
        let d = c.tick(now).expect("first report counts its whole total");
        assert_eq!(d.reason, "loss");

        // A counter going backwards (reset) saturates to 0 lost: ignored.
        let mut c = Controller::new(cfg(), t0);
        let mut now = t0 + TICK;
        frames(&mut c, now, 125_000);
        loss_total(&mut c, now, 0.0, 500);
        c.tick(now);
        now += TICK;
        frames(&mut c, now, 125_000);
        loss_total(&mut c, now, 0.5, 10);
        assert_eq!(c.tick(now), None);
        assert_eq!(c.target().bitrate_kbps, 6000);
    }
}
