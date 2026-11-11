//! Traffic and video statistics: the per-session traffic report sent to the
//! signaling server (`send_session_stats`) and the adaptation task's rolling
//! pipeline summary (`StatsWindow`, `run_adapt_task`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use proto::control::ControlMessage;
use proto::signal::SignalMessage;

use crate::adapt::{AdaptConfig, Controller, Feedback};
use crate::transport::PeerSession;

/// How often the host reports cumulative session traffic to the server.
pub(super) const SESSION_STATS_INTERVAL: Duration = Duration::from_secs(60);
/// Upper bound for reading WebRTC stats so ending a session is never held up.
const SESSION_STATS_TIMEOUT: Duration = Duration::from_secs(1);

/// Builds `SignalMessage::SessionStats` from a traffic snapshot.
fn session_stats_message(
    session_id: &str,
    stats: crate::transport::TrafficStats,
    final_report: bool,
) -> SignalMessage {
    SignalMessage::SessionStats {
        session_id: session_id.to_string(),
        bytes_sent: stats.bytes_sent,
        bytes_received: stats.bytes_received,
        relay: stats.relay,
        final_report,
    }
}

/// Reads the peer's traffic (bounded by `SESSION_STATS_TIMEOUT`) and queues a
/// `SessionStats` for the server. Silently does nothing without stats.
pub(super) async fn send_session_stats(
    peer: &PeerSession,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    session_id: &str,
    final_report: bool,
) {
    let stats = tokio::time::timeout(SESSION_STATS_TIMEOUT, peer.traffic_stats())
        .await
        .ok()
        .flatten();
    if let Some(stats) = stats {
        let _ = out_tx.send(session_stats_message(session_id, stats, final_report));
    }
}

/// Number of adapt ticks (1 s each) per `StatsWindow` summary.
const SUMMARY_TICKS: u32 = 5;

/// One finished `StatsWindow`: deltas over the window, not cumulative totals.
struct StatsSummary {
    seconds: u32,
    captured: u64,
    encoded: u64,
    dropped: u64,
    paced_out: u64,
    overwritten: u64,
    overruns: u64,
    keyframes: u64,
    keyframe_repeats: u64,
    idle_repeats: u64,
    kbps: u64,
    encode_ms_avg: f64,
    encode_ms_max: f64,
    loss: f32,
    lost_packets: u32,
    remb_kbps: Option<u32>,
    rr_rtt_ms: Option<f64>,
    rr_jitter_ms: Option<f32>,
    gap_ms_p50: Option<f64>,
    gap_ms_p95: Option<f64>,
    gap_ms_max: Option<f64>,
}

/// Nearest-rank percentile of an ascending-sorted slice: index
/// `ceil(p/100 * n) - 1`, clamped to `0..n`. `None` for an empty slice.
fn percentile_sorted(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    Some(sorted[idx])
}

/// `1.2`, or `-` when the value is unknown (log fields).
fn fmt_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"))
}

/// Rolls the pipeline counters up into one `info!` line every `SUMMARY_TICKS`
/// adapt ticks (5 s at `adapt::TICK` = 1 s) -- the per-session diagnostic the
/// 2026-10-13 audit found missing: without it `agent.log` says nothing about
/// whether frames were captured, encoded or sent when a user reports a freeze.
/// Deterministic (no clock inside): the caller drives it with `tick`.
struct StatsWindow {
    ticks: u32,
    bytes: u64,
    last_loss: f32,
    /// Lost packets accumulated in this window (delta of the receiver
    /// report's cumulative counter).
    lost_packets: u32,
    /// Cumulative counter of the previous report. The first report only
    /// seeds it (counts as 0 for the window) so the session's earlier
    /// history isn't dumped into one window.
    prev_total_lost: Option<u32>,
    last_remb_kbps: Option<u32>,
    last_rr_rtt_ms: Option<f64>,
    last_rr_jitter_ms: Option<f32>,
    captured: u64,
    encoded: u64,
    dropped: u64,
    paced_out: u64,
    overwritten: u64,
    overruns: u64,
    keyframes: u64,
    keyframe_repeats: u64,
    idle_repeats: u64,
    encode_us_total: u64,
    /// Capture time of the previous frame; deliberately kept across window
    /// boundaries so the first gap of a window is measured from the last
    /// frame of the previous one.
    prev_captured_at: Option<Instant>,
    /// Intervals between consecutive captured frames in this window, in us.
    gaps_us: Vec<u64>,
}

impl StatsWindow {
    fn new(stats: &crate::pipeline::PipelineStats) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        Self {
            ticks: 0,
            bytes: 0,
            last_loss: 0.0,
            lost_packets: 0,
            prev_total_lost: None,
            last_remb_kbps: None,
            last_rr_rtt_ms: None,
            last_rr_jitter_ms: None,
            captured: stats.captured.load(Relaxed),
            encoded: stats.encoded.load(Relaxed),
            dropped: stats.dropped.load(Relaxed),
            paced_out: stats.paced_out.load(Relaxed),
            overwritten: stats.overwritten.load(Relaxed),
            overruns: stats.overruns.load(Relaxed),
            keyframes: stats.keyframes.load(Relaxed),
            keyframe_repeats: stats.keyframe_repeats.load(Relaxed),
            idle_repeats: stats.idle_repeats.load(Relaxed),
            encode_us_total: stats.encode_us_total.load(Relaxed),
            prev_captured_at: None,
            gaps_us: Vec::new(),
        }
    }

    fn frame(&mut self, bytes: usize, captured_at: Instant) {
        self.bytes += bytes as u64;
        if let Some(prev) = self.prev_captured_at {
            self.gaps_us
                .push(captured_at.saturating_duration_since(prev).as_micros() as u64);
        }
        self.prev_captured_at = Some(captured_at);
    }

    fn loss(&mut self, fraction: f32, total_lost: u32) {
        self.last_loss = fraction;
        if let Some(prev) = self.prev_total_lost {
            self.lost_packets = self
                .lost_packets
                .saturating_add(total_lost.saturating_sub(prev));
        }
        self.prev_total_lost = Some(total_lost);
    }

    fn remb(&mut self, kbps: u32) {
        self.last_remb_kbps = Some(kbps);
    }

    fn receiver_stats(&mut self, rtt: Option<Duration>, jitter_ms: f32) {
        self.last_rr_rtt_ms = rtt.map(|d| d.as_secs_f64() * 1000.0);
        self.last_rr_jitter_ms = Some(jitter_ms);
    }

    fn tick(&mut self, stats: &crate::pipeline::PipelineStats) -> Option<StatsSummary> {
        use std::sync::atomic::Ordering::Relaxed;
        self.ticks += 1;
        if self.ticks < SUMMARY_TICKS {
            return None;
        }
        let seconds = self.ticks;
        let captured = stats.captured.load(Relaxed);
        let encoded = stats.encoded.load(Relaxed);
        let dropped = stats.dropped.load(Relaxed);
        let paced_out = stats.paced_out.load(Relaxed);
        let overwritten = stats.overwritten.load(Relaxed);
        let overruns = stats.overruns.load(Relaxed);
        let keyframes = stats.keyframes.load(Relaxed);
        let keyframe_repeats = stats.keyframe_repeats.load(Relaxed);
        let idle_repeats = stats.idle_repeats.load(Relaxed);
        let encode_us_total = stats.encode_us_total.load(Relaxed);
        let d_encoded = encoded.saturating_sub(self.encoded);
        let d_us = encode_us_total.saturating_sub(self.encode_us_total);
        let mut gaps = std::mem::take(&mut self.gaps_us);
        gaps.sort_unstable();
        let gap_ms = |v: Option<u64>| v.map(|us| us as f64 / 1000.0);
        let summary = StatsSummary {
            seconds,
            captured: captured.saturating_sub(self.captured),
            encoded: d_encoded,
            dropped: dropped.saturating_sub(self.dropped),
            paced_out: paced_out.saturating_sub(self.paced_out),
            overwritten: overwritten.saturating_sub(self.overwritten),
            overruns: overruns.saturating_sub(self.overruns),
            keyframes: keyframes.saturating_sub(self.keyframes),
            keyframe_repeats: keyframe_repeats.saturating_sub(self.keyframe_repeats),
            idle_repeats: idle_repeats.saturating_sub(self.idle_repeats),
            kbps: self.bytes * 8 / 1000 / u64::from(seconds),
            encode_ms_avg: if d_encoded == 0 {
                0.0
            } else {
                d_us as f64 / d_encoded as f64 / 1000.0
            },
            encode_ms_max: stats.encode_us_max.swap(0, Relaxed) as f64 / 1000.0,
            loss: self.last_loss,
            lost_packets: self.lost_packets,
            remb_kbps: self.last_remb_kbps,
            rr_rtt_ms: self.last_rr_rtt_ms,
            rr_jitter_ms: self.last_rr_jitter_ms,
            gap_ms_p50: gap_ms(percentile_sorted(&gaps, 50.0)),
            gap_ms_p95: gap_ms(percentile_sorted(&gaps, 95.0)),
            gap_ms_max: gap_ms(gaps.last().copied()),
        };
        self.ticks = 0;
        self.bytes = 0;
        self.lost_packets = 0;
        self.captured = captured;
        self.encoded = encoded;
        self.dropped = dropped;
        self.paced_out = paced_out;
        self.overwritten = overwritten;
        self.overruns = overruns;
        self.keyframes = keyframes;
        self.keyframe_repeats = keyframe_repeats;
        self.idle_repeats = idle_repeats;
        self.encode_us_total = encode_us_total;
        Some(summary)
    }
}

/// Drives the adaptation controller for one session: applies every
/// `Feedback` as it arrives, ticks the controller once a second, and on
/// every `Decision` updates the encoder's rate (via `rate_control`) and
/// tells the client (`ControlMessage::Quality`, see `proto/src/control.rs`).
/// Runs until its `Feedback` channel closes or `ActiveSession::shutdown`
/// aborts the task.
pub(super) async fn run_adapt_task(
    cfg: AdaptConfig,
    mut rx: mpsc::UnboundedReceiver<Feedback>,
    rate_control: Arc<crate::pipeline::RateControl>,
    pipeline_stats: Arc<crate::pipeline::PipelineStats>,
    peer: Arc<PeerSession>,
) {
    let now = Instant::now();
    let mut controller = Controller::new(cfg, now);
    let mut window = StatsWindow::new(&pipeline_stats);
    // Encoder overruns are read off the pipeline's counter as a delta per
    // tick (see `Feedback::Overrun`), not pushed per event. `overruns`, not
    // `overwritten`: see `PipelineStats::overruns`.
    let mut overruns_seen = pipeline_stats
        .overruns
        .load(std::sync::atomic::Ordering::Relaxed);
    // `interval` would fire immediately; the first tick must wait a full
    // `TICK` so it sees a real window of frames/REMB instead of deciding on
    // nothing at session start.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::from_std(now + crate::adapt::TICK),
        crate::adapt::TICK,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The last decision the client hasn't been told about yet: the first
    // decisions often land before the `control` channel is open (the
    // controller runs from session start, the channel opens after DTLS), and
    // `send_control` drops messages into a closed channel -- so the
    // announcement is retried on every tick until it is actually delivered.
    let mut unannounced: Option<ControlMessage> = None;
    // Selected ICE pair as last logged, and the pair's discard counter at the
    // previous summary (for the per-window delta).
    let mut logged_path: Option<crate::transport::PathInfo> = None;
    let mut discarded_seen: u32 = 0;
    // The preset QP ceiling the encoder currently has (it starts with none).
    let mut applied_max_qp: Option<u8> = None;

    loop {
        tokio::select! {
            fb = rx.recv() => {
                match fb {
                    Some(fb) => {
                        match &fb {
                            Feedback::Frame { bytes, captured_at } => {
                                window.frame(*bytes, *captured_at)
                            }
                            Feedback::Loss {
                                fraction,
                                total_lost,
                            } => window.loss(*fraction, *total_lost),
                            Feedback::Remb { bitrate_bps } => window.remb((bitrate_bps / 1000) as u32),
                            Feedback::ReceiverStats { rtt, jitter_ms } => {
                                window.receiver_stats(*rtt, *jitter_ms)
                            }
                            _ => {}
                        }
                        controller.feedback(fb, Instant::now());
                        if controller.max_qp() != applied_max_qp {
                            applied_max_qp = controller.max_qp();
                            rate_control.set_max_qp(applied_max_qp);
                            tracing::info!(max_qp = ?applied_max_qp, "adapt: preset qp ceiling");
                        }
                    }
                    None => break,
                }
            }
            _ = ticker.tick() => {
                let overruns_now = pipeline_stats
                    .overruns
                    .load(std::sync::atomic::Ordering::Relaxed);
                let overrun = overruns_now.saturating_sub(overruns_seen);
                overruns_seen = overruns_now;
                if overrun > 0 {
                    controller.feedback(Feedback::Overrun { frames: overrun }, Instant::now());
                }
                if let Some(s) = window.tick(&pipeline_stats) {
                    let target = controller.target();
                    let path = peer.selected_path().await;
                    match &path {
                        Some(p) => {
                            if !logged_path.as_ref().is_some_and(|l| l.same_pair(p)) {
                                tracing::info!(
                                    "ice path selected local={} remote={} ice_rtt_ms={}",
                                    p.local.describe(),
                                    p.remote.describe(),
                                    fmt_opt(p.rtt_ms)
                                );
                                discarded_seen = 0;
                            }
                            logged_path = Some(p.clone());
                        }
                        None => tracing::debug!("ice path: no selected candidate pair"),
                    }
                    let send_discarded = path.as_ref().map_or(0, |p| {
                        let d = p.packets_discarded_on_send.saturating_sub(discarded_seen);
                        discarded_seen = p.packets_discarded_on_send;
                        d
                    });
                    tracing::info!(
                        captured = s.captured,
                        encoded = s.encoded,
                        gap_ms_p50 = %fmt_opt(s.gap_ms_p50),
                        gap_ms_p95 = %fmt_opt(s.gap_ms_p95),
                        gap_ms_max = %fmt_opt(s.gap_ms_max),
                        dropped = s.dropped,
                        paced_out = s.paced_out,
                        overwritten = s.overwritten,
                        overruns = s.overruns,
                        keyframes = s.keyframes,
                        keyframe_repeats = s.keyframe_repeats,
                        idle_repeats = s.idle_repeats,
                        kbps = s.kbps,
                        encode_ms_avg = format_args!("{:.1}", s.encode_ms_avg),
                        encode_ms_max = format_args!("{:.1}", s.encode_ms_max),
                        loss = format_args!("{:.3}", s.loss),
                        lost_packets = s.lost_packets,
                        remb_kbps = ?s.remb_kbps,
                        target_kbps = target.bitrate_kbps,
                        target_fps = target.fps,
                        path = %crate::transport::PathInfo::path_label(path.as_ref()),
                        ice_rtt_ms = %fmt_opt(path.as_ref().and_then(|p| p.rtt_ms)),
                        rr_rtt_ms = %fmt_opt(s.rr_rtt_ms),
                        rr_jitter_ms = %fmt_opt(s.rr_jitter_ms),
                        send_discarded,
                        "video: {}s summary",
                        s.seconds
                    );
                }
                if let Some(decision) = controller.tick(Instant::now()) {
                    rate_control.set(decision.target);
                    tracing::info!(
                        bitrate_kbps = decision.target.bitrate_kbps,
                        fps = decision.target.fps,
                        reason = decision.reason,
                        "adapt: new rate target"
                    );
                    unannounced = Some(ControlMessage::Quality {
                        bitrate_kbps: decision.target.bitrate_kbps,
                        fps: decision.target.fps,
                        reason: decision.reason.to_string(),
                    });
                }
                if let Some(msg) = &unannounced {
                    match peer.send_control(msg).await {
                        Ok(true) => unannounced = None,
                        Ok(false) => {}
                        Err(err) => {
                            tracing::warn!(?err, "failed to send quality control message");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_window_summarises_every_five_ticks() {
        use std::sync::atomic::Ordering::Relaxed;
        let stats = crate::pipeline::PipelineStats::default();
        let mut w = StatsWindow::new(&stats);
        for _ in 0..4 {
            assert!(w.tick(&stats).is_none());
        }
        stats.captured.store(150, Relaxed);
        stats.encoded.store(140, Relaxed);
        stats.encode_us_total.store(140 * 3000, Relaxed);
        stats.encode_us_max.store(12_000, Relaxed);
        for _ in 0..140 {
            w.frame(5000, Instant::now());
        }
        w.loss(0.02, 100);
        w.loss(0.02, 107);
        w.receiver_stats(Some(Duration::from_millis(12)), 3.5);
        let s = w.tick(&stats).expect("fifth tick yields a summary");
        assert_eq!(s.rr_rtt_ms.map(|r| r.round()), Some(12.0));
        assert_eq!(s.rr_jitter_ms, Some(3.5));
        assert_eq!(s.captured, 150);
        assert_eq!(s.encoded, 140);
        assert_eq!(s.kbps, 1120);
        assert!((s.encode_ms_avg - 3.0).abs() < 0.01);
        assert!((s.encode_ms_max - 12.0).abs() < 0.01);
        assert_eq!(s.loss, 0.02);
        // The first report only seeds the counter; the second adds 7.
        assert_eq!(s.lost_packets, 7);
        assert_eq!(stats.encode_us_max.load(Relaxed), 0);
    }

    /// Feeds `n` frames spaced by `gaps_ms` (first frame at `start`), returns
    /// the capture time of the last one.
    fn feed_gaps(w: &mut StatsWindow, start: Instant, gaps_ms: &[u64]) -> Instant {
        let mut t = start;
        w.frame(1000, t);
        for g in gaps_ms {
            t += Duration::from_millis(*g);
            w.frame(1000, t);
        }
        t
    }

    fn summary_after_ticks(
        w: &mut StatsWindow,
        stats: &crate::pipeline::PipelineStats,
    ) -> StatsSummary {
        for _ in 0..4 {
            assert!(w.tick(stats).is_none());
        }
        w.tick(stats).expect("fifth tick yields a summary")
    }

    #[test]
    fn gap_steady_stream_has_flat_percentiles() {
        let stats = crate::pipeline::PipelineStats::default();
        let mut w = StatsWindow::new(&stats);
        feed_gaps(&mut w, Instant::now(), &[33; 30]);
        let s = summary_after_ticks(&mut w, &stats);
        assert_eq!(s.gap_ms_p50, Some(33.0));
        assert_eq!(s.gap_ms_p95, Some(33.0));
        assert_eq!(s.gap_ms_max, Some(33.0));
    }

    #[test]
    fn gap_outlier_shows_in_max_not_median() {
        let stats = crate::pipeline::PipelineStats::default();
        let mut w = StatsWindow::new(&stats);
        let mut gaps = vec![33_u64; 30];
        gaps.push(100);
        feed_gaps(&mut w, Instant::now(), &gaps);
        let s = summary_after_ticks(&mut w, &stats);
        assert_eq!(s.gap_ms_max, Some(100.0));
        assert_eq!(s.gap_ms_p50, Some(33.0));
    }

    #[test]
    fn gap_none_for_empty_and_single_frame_windows() {
        let stats = crate::pipeline::PipelineStats::default();
        let mut w = StatsWindow::new(&stats);
        let s = summary_after_ticks(&mut w, &stats);
        assert_eq!(
            (s.gap_ms_p50, s.gap_ms_p95, s.gap_ms_max),
            (None, None, None)
        );
        let mut w = StatsWindow::new(&stats);
        w.frame(1000, Instant::now());
        let s = summary_after_ticks(&mut w, &stats);
        assert_eq!(
            (s.gap_ms_p50, s.gap_ms_p95, s.gap_ms_max),
            (None, None, None)
        );
    }

    #[test]
    fn gap_carries_over_window_boundary() {
        let stats = crate::pipeline::PipelineStats::default();
        let mut w = StatsWindow::new(&stats);
        let last = feed_gaps(&mut w, Instant::now(), &[33; 10]);
        summary_after_ticks(&mut w, &stats);
        w.frame(1000, last + Duration::from_millis(50));
        let s = summary_after_ticks(&mut w, &stats);
        assert_eq!(s.gap_ms_p50, Some(50.0));
        assert_eq!(s.gap_ms_p95, Some(50.0));
        assert_eq!(s.gap_ms_max, Some(50.0));
    }

    #[test]
    fn percentile_sorted_is_nearest_rank() {
        assert_eq!(percentile_sorted(&[10, 20, 30, 40], 50.0), Some(20));
        assert_eq!(percentile_sorted(&[10, 20, 30, 40], 95.0), Some(40));
        assert_eq!(percentile_sorted(&[7], 50.0), Some(7));
        assert_eq!(percentile_sorted(&[7], 95.0), Some(7));
        assert_eq!(percentile_sorted(&[], 50.0), None);
    }

    #[test]
    fn fmt_opt_prints_value_or_dash() {
        assert_eq!(fmt_opt(Some(1.23_f64)), "1.2");
        assert_eq!(fmt_opt(None::<f32>), "-");
    }

    #[test]
    fn stats_window_reports_deltas_not_totals() {
        use std::sync::atomic::Ordering::Relaxed;
        let stats = crate::pipeline::PipelineStats::default();
        let mut w = StatsWindow::new(&stats);
        stats.captured.store(150, Relaxed);
        for _ in 0..4 {
            assert!(w.tick(&stats).is_none());
        }
        assert!(w.tick(&stats).is_some());
        stats.captured.fetch_add(10, Relaxed);
        for _ in 0..4 {
            assert!(w.tick(&stats).is_none());
        }
        let s = w.tick(&stats).expect("second summary");
        assert_eq!(s.captured, 10);
        assert_eq!(s.kbps, 0);
    }
}
