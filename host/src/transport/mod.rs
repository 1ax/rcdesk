//! WebRTC transport for one host&lt;-&gt;client peer session.
//!
//! Sets up a `PeerConnection` with a single H.264 video track (see
//! ARCHITECTURE.md §5) and the four fixed data channels (`input`, `pointer`,
//! `control`, `file`), and forwards session-level events (trickle ICE
//! candidates, connection state, keyframe requests from PLI/FIR) to the
//! caller over an `mpsc` channel. The signaling exchange itself (offer/answer
//! over the wire) lives in `crate::signaling`; this module only speaks
//! WebRTC.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::sync::mpsc;

use rtc::interceptor::{Interceptor, Packet as InterceptorPacket, TaggedPacket};
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use rtc::rtcp::receiver_report::ReceiverReport as RtcpReceiverReport;
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::extension::abs_send_time_extension::AbsSendTimeExtension;
use rtc::rtp::extension::HeaderExtension;
use rtc::rtp::packetizer::{new_packetizer, Packetizer};
use rtc::rtp::sequence::new_random_sequencer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RTCRtpHeaderExtensionCapability, RtpCodecKind,
};
use rtc::sansio::Protocol;
use rtc::sdp::extmap::ABS_SEND_TIME_URI;
use rtc::shared::error::Error;

use rtc::statistics::stats::ice_candidate_pair::RTCStatsIceCandidatePairState;
use webrtc::data_channel::{
    DataChannel, DataChannelEvent, RTCDataChannelInit, RTCDataChannelState,
};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCIceTransportPolicy, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription, RTCStatsReport, RTCStatsReportEntry, Registry, StatsSelector,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::Runtime;

use crate::encode::EncodedFrame;

/// Debug switch: when set to a truthy value (`1`, `true`, `yes`), the host
/// gathers and uses only TURN relay candidates (`RTCIceTransportPolicy::Relay`).
/// Meant for live checks of the host's TURN client (debt D75: ICE rarely
/// selects relay on its own). Without a TURN server in `ice_servers`,
/// `PeerSession::new` fails with a configuration validation error. Never set
/// this in production.
pub const RELAY_ONLY_ENV: &str = "RCDESK_ICE_RELAY_ONLY";

/// Whether a `RELAY_ONLY_ENV` value enables relay-only mode.
fn relay_only_from_env_value(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes")
    )
}

/// Payload type for the (only) negotiated video codec. Fixed rather than
/// negotiated because the media engine registers exactly one video codec.
const VIDEO_PAYLOAD_TYPE: u8 = 102;

/// RTP outbound MTU, matching `TrackLocalStaticSample`'s private
/// `RTP_OUTBOUND_MTU` (`static_sample.rs`, not `pub`) -- this transport
/// packetizes by hand (see `PeerSession::start_video`) instead of using that
/// type, precisely to control RTP timestamps.
const RTP_MTU: usize = 1200;

/// RTP clock rate for the (only) negotiated video codec, matching
/// `H264_FMTP_LINE`'s codec registration (`clock_rate: 90000` below).
const VIDEO_CLOCK_RATE: u32 = 90_000;

/// Upper bound on how far an inter-frame capture gap may advance the RTP
/// timestamp (see `PeerSession::start_video`). Screen capture only produces
/// frames when the screen changes, so a long idle stretch (nobody touching
/// the remote desktop) can leave a multi-minute gap between two captured
/// frames; feeding that raw gap to `skip_samples` would jump the RTP clock
/// minutes ahead in one step; capped well above any realistic inter-frame
/// gap during active use.
const MAX_CAPTURE_GAP: Duration = Duration::from_secs(10);

/// How often `PeerSession::start_video` may re-arm the pipeline's keyframe
/// request while the video SSRC/payload type still isn't negotiated. Each
/// re-arm costs one keyframe encode, so this bounds encoder load on a
/// session that never finishes negotiating.
const KEYFRAME_REARM_INTERVAL: Duration = Duration::from_millis(250);

/// Packets per pacing burst in `PeerSession::start_video` (about 38 KB at
/// `RTP_MTU`). A frame that fits one burst is sent back to back, as before.
const PACE_BURST_PACKETS: usize = 32;

/// Pause between two pacing bursts of one video frame.
const PACE_BURST_GAP: Duration = Duration::from_millis(1);

/// Ceiling on the total time spent sending one frame (about one frame
/// interval at 30 fps). Once exceeded (e.g. the Windows timer granularity,
/// up to 15.6 ms per sleep, ate the budget) the rest of the frame goes out
/// without pauses, so pacing never adds more than one frame of latency.
const PACE_MAX_SPREAD: Duration = Duration::from_millis(30);

/// Says whether to pause before packet `next` of a `total`-packet frame
/// whose first packet went out `elapsed` ago. Pauses only at burst
/// boundaries (`next` a non-zero multiple of `PACE_BURST_PACKETS`, with
/// packets still left to send) and only while `elapsed < PACE_MAX_SPREAD`.
fn pace_gap(next: usize, total: usize, elapsed: Duration) -> Option<Duration> {
    if next > 0
        && next < total
        && next.is_multiple_of(PACE_BURST_PACKETS)
        && elapsed < PACE_MAX_SPREAD
    {
        Some(PACE_BURST_GAP)
    } else {
        None
    }
}

/// How often `PeerSession::start_video`'s startup keyframe task re-requests a
/// keyframe after the connection reaches `Connected`, until the receiver
/// confirms it is actually getting video via an RTCP Receiver Report (see
/// `StartupKeyframe`).
///
/// The host's `RTCPeerConnectionState` reaches `Connected` on the DTLS
/// *server* side of the handshake, which finishes its final flight roughly
/// half an RTT before the DTLS *client* (Chrome) derives its SRTP keys and
/// can decrypt anything. Any packet sent in that window -- in particular the
/// forced startup keyframe `start_video`'s frame-writer task sends right
/// after `negotiated_ssrc_and_pt()` succeeds -- is silently dropped by
/// Chrome's SRTP layer before it ever reaches the decoder. Chrome never
/// sends a PLI for a packet it never received, so on a static host screen
/// (no further frame ever captured to retry with) the session can sit at a
/// black screen indefinitely. Measured live on the Win10 stand (2026-09-21,
/// RTT ~350 ms): Chrome reported `connected` at 3.4 s with zero video
/// packets received in 30+ s in one run, and the first frame only arriving
/// at the 9th second in another. The task driven by `StartupKeyframe` covers
/// this window by re-requesting a keyframe every
/// `STARTUP_KEYFRAME_INTERVAL` until the receiver confirms delivery or
/// `STARTUP_KEYFRAME_DEADLINE` elapses.
const STARTUP_KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);

/// Upper bound on how long `PeerSession::start_video`'s startup keyframe task
/// keeps re-requesting after `Connected` without a confirming receiver
/// report -- see `STARTUP_KEYFRAME_INTERVAL`'s doc comment.
const STARTUP_KEYFRAME_DEADLINE: Duration = Duration::from_secs(15);

/// Startup keyframe re-arm after the connection is up, until the receiver
/// confirms it is getting video (see `STARTUP_KEYFRAME_INTERVAL`'s doc
/// comment on the DTLS server/client asymmetry). Plain, non-async and
/// webrtc-free by design so it can be driven with explicit `Instant`s in
/// tests instead of a real clock.
struct StartupKeyframe {
    started: Instant,
    last: Option<Instant>,
}

impl StartupKeyframe {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            last: None,
        }
    }

    /// `true` when a keyframe should be requested now.
    fn should_request(&mut self, now: Instant, delivered: bool) -> bool {
        if delivered || self.finished(now, delivered) {
            return false;
        }
        let due = match self.last {
            None => true,
            Some(last) => now.duration_since(last) >= STARTUP_KEYFRAME_INTERVAL,
        };
        if due {
            self.last = Some(now);
        }
        due
    }

    /// `true` once there's nothing more to do: delivered, or past the deadline.
    fn finished(&self, now: Instant, delivered: bool) -> bool {
        delivered || now.duration_since(self.started) >= STARTUP_KEYFRAME_DEADLINE
    }
}

/// Constrained Baseline `42e01f`: the one profile both Safari and Chrome are
/// guaranteed to decode in hardware (see ARCHITECTURE.md §5).
const H264_FMTP_LINE: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

/// Forwards incoming RTCP packets (PLI, FIR, receiver reports, ...) up to the
/// application.
///
/// `rtc`'s `NoopInterceptor` -- the innermost layer of every interceptor
/// chain, including the one `register_default_interceptors` builds -- drops
/// RTCP packets on read by design ("RTCP message read must end here. If any
/// rtcp packet needs to be forwarded to PeerConnection, just add a new
/// interceptor to forward it", see `rtc_interceptor::NoopInterceptor::handle_read`).
/// Without this interceptor, a remote PLI/FIR never reaches
/// `TrackLocalEvent::OnRtcpPacket` (`PeerSession::start_video`'s RTCP task
/// never sees it) even though `register_default_interceptors` is in place --
/// that registry only wires up NACK/TWCC/report interceptors, none of which
/// re-surface arbitrary RTCP to the application. Confirmed via the loopback
/// test in `tests/loopback.rs`: without this interceptor, a real PLI sent
/// over the wire never triggers `SessionEvent::KeyframeRequested`.
///
/// Hand-written rather than built with the `#[derive(Interceptor)]` /
/// `#[interceptor]` macros: those expand to code that references the
/// `sansio`, `shared` and `interceptor` crates by their bare names, which
/// requires depending on them directly (they're only transitive, via `rtc`,
/// for this crate) -- out of scope for this slice. Every method not doing
/// RTCP-forwarding work below simply delegates to `next`, mirroring
/// `rtc_interceptor::NoopInterceptor`'s hand-written impl.
struct RtcpForwarder<P: Interceptor> {
    next: P,
    read_queue: VecDeque<TaggedPacket>,
}

impl<P: Interceptor> Protocol<TaggedPacket, TaggedPacket, ()> for RtcpForwarder<P> {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = std::time::Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let InterceptorPacket::Rtcp(_) = &msg.message {
            self.read_queue.push_back(TaggedPacket {
                now: msg.now,
                transport: msg.transport,
                message: msg.message.clone(),
            });
        }
        self.next.handle_read(msg)
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue
            .pop_front()
            .or_else(|| self.next.poll_read())
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.next.handle_write(msg)
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.next.poll_write()
    }

    fn handle_event(&mut self, evt: ()) -> Result<(), Self::Error> {
        self.next.handle_event(evt)
    }

    fn poll_event(&mut self) -> Option<Self::Eout> {
        self.next.poll_event()
    }

    fn handle_timeout(&mut self, now: Self::Time) -> Result<(), Self::Error> {
        self.next.handle_timeout(now)
    }

    fn poll_timeout(&mut self) -> Option<Self::Time> {
        self.next.poll_timeout()
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        self.next.close()
    }
}

impl<P: Interceptor> Interceptor for RtcpForwarder<P> {
    fn bind_local_stream(&mut self, info: &rtc::interceptor::StreamInfo) {
        self.next.bind_local_stream(info);
    }
    fn unbind_local_stream(&mut self, info: &rtc::interceptor::StreamInfo) {
        self.next.unbind_local_stream(info);
    }
    fn bind_remote_stream(&mut self, info: &rtc::interceptor::StreamInfo) {
        self.next.bind_remote_stream(info);
    }
    fn unbind_remote_stream(&mut self, info: &rtc::interceptor::StreamInfo) {
        self.next.unbind_remote_stream(info);
    }
}

/// Configuration for one `PeerSession`.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// STUN/TURN servers (from the signaling server's `Registered` reply,
    /// merged with any `--stun` CLI overrides; see `crate::signaling`).
    pub ice_servers: Vec<proto::signal::IceServer>,
    /// Local UDP addresses to bind (see `with_udp_addrs`); `["0.0.0.0:0"]` in
    /// production, `["127.0.0.1:0"]` for the loopback test.
    pub udp_addrs: Vec<String>,
    /// Target video frame rate. No longer used by this module directly
    /// (RTP timestamps now come from each frame's real capture time, see
    /// `PeerSession::start_video`); kept here because callers build this
    /// `SessionConfig` alongside the encoder's `EncoderConfig`, which still
    /// needs it (see `crate::signaling::start_session`).
    pub fps: u32,
}

/// Hands out a fresh, process-wide unique `PeerSession` tag (fix to slice
/// 3.5b) -- see `SessionEvent::ConnectionState`/`DataChannelClosed`'s doc
/// comments for why events that can end a session need one. `Relaxed`: the
/// only property needed is that two sessions never get the same value, not
/// any particular ordering relative to other memory operations.
static NEXT_SESSION_TAG: AtomicU64 = AtomicU64::new(1);

fn next_session_tag() -> u64 {
    NEXT_SESSION_TAG.fetch_add(1, Ordering::Relaxed)
}

/// One message received on the `file` data channel (slice 3.4a). Unlike every
/// other channel, `file` traffic does not go through `SessionEvent` /
/// `handle_session_event` (bulk data must not queue up behind, or stall, the
/// session's control loop): the reader delivers it to a dedicated mpsc that
/// `crate::files` drains in its own task, see `PeerSession::take_file_channel`.
#[derive(Debug, Clone)]
pub struct FileChannelMessage {
    pub data: Bytes,
    pub is_string: bool,
}

/// Capacity of the `file` channel's incoming mpsc. A full queue blocks the
/// reader task, which back-pressures the SCTP association -- intended.
const FILE_QUEUE_CAPACITY: usize = 32;

/// Events a `PeerSession` reports back to its owner (the signaling client).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A local ICE candidate was gathered and should be sent to the peer.
    LocalIce(proto::signal::IceCandidate),
    /// The peer connection's aggregate connection state changed. `tag` is
    /// the `PeerSession` this came from (`PeerSession::tag`) -- fix to slice
    /// 3.5b: during automatic client reconnection (bye -> new
    /// `connect_device` -> `PeerJoined`, all while the old `PeerConnection`
    /// is still tearing down), a state change from the *old*, already
    /// superseded session can land on the shared event channel after a new
    /// session has already taken its place; `handle_session_event` compares
    /// this against `ActiveSession::session_tag` and ignores a mismatch
    /// instead of wrongly ending the new session.
    ConnectionState {
        state: RTCPeerConnectionState,
        tag: u64,
    },
    /// A message arrived on one of the fixed data channels.
    DataChannelMessage {
        label: String,
        data: Bytes,
        is_string: bool,
    },
    /// The remote peer asked for a keyframe (PLI or FIR).
    KeyframeRequested,
    /// Receiver Estimated Maximum Bitrate (REMB, draft-alvestrand-rmcat-remb) from the
    /// browser's receive-side bandwidth estimator: its current estimate of what the path
    /// can carry, in bits per second. Chrome sends one every ~200 ms alongside its RR
    /// (verified live in slice 2.3 -- see docs/host-libs-api-notes.md).
    Remb { bitrate_bps: u64 },
    /// One reception report block about our video SSRC from an RTCP Receiver Report.
    ReceiverReport {
        /// Fraction of packets lost since the previous report, 0.0..=1.0 (`fraction_lost / 256`).
        fraction_lost: f32,
        /// Cumulative packets lost.
        total_lost: u32,
        /// Interarrival jitter in RTP clock ticks (90 kHz).
        jitter: u32,
        /// Round-trip time computed from LSR/DLSR against our NTP clock, `None` if the
        /// report carries no LSR yet (no SR received on the far side) or the result is implausible.
        rtt: Option<Duration>,
    },
    /// One of the four fixed data channels finished its WebRTC handshake and
    /// is now open. The signaling layer uses this to know when it is safe to
    /// send the client its initial state over `control` (e.g. the display
    /// list, slice 2.4).
    DataChannelOpen { label: String },
    /// One of the four fixed data channels was closed by the remote peer
    /// (its `RTCDataChannelState` reached `Closed`). Slice 3.5b: the
    /// signaling layer treats the `control` channel closing as a
    /// deliberate end of session (the client closed the tab/`PeerConnection`)
    /// and ends the session right away, rather than waiting out the
    /// `Disconnected` grace window -- see `crate::signaling::handle_session_event`.
    /// `tag` is the `PeerSession` this came from -- same reasoning as
    /// `ConnectionState`'s `tag`.
    DataChannelClosed { label: String, tag: u64 },
    /// Slice 3.5b: a `ConnectionState(Disconnected)` grace window (see
    /// `crate::signaling::handle_session_event`) expired without the
    /// connection recovering to `Connected`. Not sent by anything in this
    /// module -- the signaling layer arms a timer that sends this back to
    /// itself over the same `SessionEvent` channel it reads from, so it
    /// keeps working across a signaling reconnect and while offline
    /// (`SessionSlot::run_while_offline`). `generation` identifies *which*
    /// grace window this is for, so a timeout from a window a later
    /// `Connected` already cancelled is recognized as stale and ignored.
    DisconnectTimeout { session_id: String, generation: u64 },
    /// Slice 3.2c: a pending OPAQUE login (`crate::signaling::PendingAuth`,
    /// armed on `AuthRequired` while waiting for the client to complete
    /// `PakeStart`/`PakeFinish`) has been waiting too long. Not sent by
    /// anything in this module -- same self-timer pattern as
    /// `DisconnectTimeout` above: the signaling layer arms a timer that
    /// sends this back to itself over the same `SessionEvent` channel.
    /// `generation` identifies which pending login this is for, so a
    /// timeout from a login a later `PakeFinish` already completed (or a
    /// new `PeerJoined` already superseded) is recognized as stale and
    /// ignored.
    AuthTimeout { session_id: String, generation: u64 },
    /// D58: the video pipeline (screen capture or encoder) stopped with an
    /// error. Sent by the signaling layer's frame-forwarding task, not by
    /// anything in this module. `tag` is `PeerSession::tag()`, so
    /// `handle_session_event` can drop the event of a superseded session,
    /// like for `ConnectionState`.
    VideoFailed { tag: u64 },
}

fn ice_candidate_from_rtc(c: RTCIceCandidateInit) -> proto::signal::IceCandidate {
    proto::signal::IceCandidate {
        candidate: c.candidate,
        sdp_mid: c.sdp_mid,
        sdp_mline_index: c.sdp_mline_index,
    }
}

fn ice_candidate_to_rtc(c: proto::signal::IceCandidate) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: c.candidate,
        sdp_mid: c.sdp_mid,
        sdp_mline_index: c.sdp_mline_index,
        username_fragment: None,
        url: None,
    }
}

/// Forwards `PeerConnection` events onto the session's `SessionEvent` channel.
/// Data-channel and video-track events are handled separately (the channels
/// and the track are set up by `PeerSession` itself, not received via
/// `on_data_channel`/`on_track`), so this handler only needs the two
/// connection-level callbacks.
struct Handler {
    events: mpsc::Sender<SessionEvent>,
    /// Flipped once the aggregate connection state first reaches
    /// `Connected`. `start_video` waits on this before sending anything:
    /// `RtpSender::get_parameters()` can return a negotiated payload type as
    /// soon as SDP is exchanged, well before DTLS/SRTP finishes -- writing a
    /// sample that early is silently dropped by the SRTP layer (no local
    /// SRTP context yet), which is exactly the case that must not eat the
    /// forced startup keyframe below.
    connected: Arc<AtomicBool>,
    /// This `Handler`'s owning `PeerSession`'s tag -- see
    /// `SessionEvent::ConnectionState`'s doc comment.
    tag: u64,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        let Ok(candidate) = event.candidate.to_json() else {
            tracing::warn!("failed to serialize local ICE candidate");
            return;
        };
        let _ = self
            .events
            .send(SessionEvent::LocalIce(ice_candidate_from_rtc(candidate)))
            .await;
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            self.connected.store(true, Ordering::Release);
        }
        let _ = self
            .events
            .send(SessionEvent::ConnectionState {
                state,
                tag: self.tag,
            })
            .await;
    }
}

/// One WebRTC peer session: a `PeerConnection`, its single video track and
/// the four fixed data channels.
pub struct PeerSession {
    pc: Box<dyn PeerConnection>,
    video_track: Arc<TrackLocalStaticRTP>,
    video_sender: Arc<dyn RtpSender>,
    events: mpsc::Sender<SessionEvent>,
    connected: Arc<AtomicBool>,
    /// The `control` data channel (cursor shape, app-level ping/pong; see
    /// ARCHITECTURE.md §5), kept aside from the other three so
    /// `send_control` can write to it directly.
    control_channel: Arc<dyn DataChannel>,
    /// The `file` data channel and the receiving end of its incoming
    /// messages (slice 3.4a), handed out once by `take_file_channel`.
    file_channel: Arc<dyn DataChannel>,
    file_rx: std::sync::Mutex<Option<mpsc::Receiver<FileChannelMessage>>>,
    /// This session's unique tag (fix to slice 3.5b) -- see
    /// `SessionEvent::ConnectionState`'s doc comment.
    tag: u64,
}

/// One end of the selected ICE candidate pair, for the `agent.log` path line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateInfo {
    /// `host`, `srflx`, `prflx` or `relay`.
    pub typ: String,
    pub protocol: String,
    /// `ip:port`.
    pub address: String,
}

impl CandidateInfo {
    fn from_stats(c: &rtc::statistics::stats::ice_candidate::RTCIceCandidateStats) -> Self {
        Self {
            typ: c.candidate_type.to_string(),
            protocol: c.protocol.clone(),
            address: format!("{}:{}", c.address.as_deref().unwrap_or("?"), c.port),
        }
    }

    /// `host/udp 192.168.1.77:55544`.
    pub fn describe(&self) -> String {
        format!("{}/{} {}", self.typ, self.protocol, self.address)
    }
}

/// The selected ICE candidate pair plus what the ICE layer knows about it.
#[derive(Debug, Clone, PartialEq)]
pub struct PathInfo {
    pub local: CandidateInfo,
    pub remote: CandidateInfo,
    /// `current_round_trip_time` in ms; `None` while it is still 0.
    pub rtt_ms: Option<f64>,
    pub packets_discarded_on_send: u32,
}

impl PathInfo {
    /// Compact `local>remote` candidate types, e.g. `host>srflx`; `?` when unknown.
    pub fn path_label(path: Option<&PathInfo>) -> String {
        match path {
            Some(p) => format!("{}>{}", p.local.typ, p.remote.typ),
            None => "?".to_string(),
        }
    }

    /// Same pair (candidates identical), ignoring RTT and counters.
    pub fn same_pair(&self, other: &PathInfo) -> bool {
        self.local == other.local && self.remote == other.remote
    }
}

/// webrtc-rs 0.20.5 names candidate entries `RTCLocalIceCandidate_<id>` /
/// `RTCRemoteIceCandidate_<id>` while a pair refers to the bare `<id>`
/// (`candidate:56o...`), so accept an exact match or a `_<id>` suffix.
fn candidate_id_matches(entry_id: &str, wanted: &str) -> bool {
    !wanted.is_empty() && (entry_id == wanted || entry_id.ends_with(&format!("_{wanted}")))
}

/// Picks the selected pair out of a stats report: the one named by the
/// transport's `selected_candidate_pair_id`, else a nominated, succeeded pair.
fn path_from_report(report: &RTCStatsReport) -> Option<PathInfo> {
    let selected_id = report
        .transport()
        .map(|t| t.selected_candidate_pair_id.as_str())
        .filter(|id| !id.is_empty());
    let pair = selected_id
        .and_then(|id| match report.get(id) {
            Some(RTCStatsReportEntry::IceCandidatePair(p)) => Some(p),
            _ => None,
        })
        .or_else(|| {
            report
                .candidate_pairs()
                .find(|p| p.nominated && p.state == RTCStatsIceCandidatePairState::Succeeded)
        })?;
    let local = report.iter().find_map(|e| match e {
        RTCStatsReportEntry::LocalCandidate(c)
            if candidate_id_matches(&c.stats.id, &pair.local_candidate_id) =>
        {
            Some(CandidateInfo::from_stats(c))
        }
        _ => None,
    })?;
    let remote = report.iter().find_map(|e| match e {
        RTCStatsReportEntry::RemoteCandidate(c)
            if candidate_id_matches(&c.stats.id, &pair.remote_candidate_id) =>
        {
            Some(CandidateInfo::from_stats(c))
        }
        _ => None,
    })?;
    Some(PathInfo {
        local,
        remote,
        rtt_ms: (pair.current_round_trip_time > 0.0)
            .then_some(pair.current_round_trip_time * 1000.0),
        packets_discarded_on_send: pair.packets_discarded_on_send,
    })
}

/// Cumulative traffic of one peer connection plus whether it goes through a
/// TURN relay; reported to the server as `SignalMessage::SessionStats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrafficStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub relay: bool,
}

impl TrafficStats {
    /// Pure core of `traffic_from_report`: `transport_bytes` is
    /// `(sent, received)` of the bundled transport; `None` means the report
    /// has no transport entry yet. `relay` is true when either end of the
    /// selected pair is a relay candidate.
    fn from_parts(transport_bytes: Option<(u64, u64)>, path: Option<&PathInfo>) -> Option<Self> {
        let (bytes_sent, bytes_received) = transport_bytes?;
        Some(Self {
            bytes_sent,
            bytes_received,
            relay: path.is_some_and(|p| p.local.typ == "relay" || p.remote.typ == "relay"),
        })
    }
}

/// Transport-level counters (not the selected candidate pair's): the single
/// bundled transport carries video (SRTP), all data channels (SCTP/DTLS)
/// and ICE/DTLS overhead, and stays cumulative across a pair change.
fn traffic_from_report(report: &RTCStatsReport) -> Option<TrafficStats> {
    TrafficStats::from_parts(
        report.transport().map(|t| (t.bytes_sent, t.bytes_received)),
        path_from_report(report).as_ref(),
    )
}

/// `(label, RTCDataChannelInit)` for the four channels every session opens
/// before the offer is created (see ARCHITECTURE.md §5).
fn data_channel_specs() -> [(&'static str, RTCDataChannelInit); 4] {
    [
        (
            "input",
            RTCDataChannelInit {
                ordered: true,
                ..default_data_channel_init()
            },
        ),
        (
            "pointer",
            RTCDataChannelInit {
                ordered: false,
                max_retransmits: Some(0),
                ..default_data_channel_init()
            },
        ),
        (
            "control",
            RTCDataChannelInit {
                ordered: true,
                ..default_data_channel_init()
            },
        ),
        (
            "file",
            RTCDataChannelInit {
                ordered: true,
                ..default_data_channel_init()
            },
        ),
    ]
}

fn default_data_channel_init() -> RTCDataChannelInit {
    RTCDataChannelInit {
        ordered: true,
        max_packet_life_time: None,
        max_retransmits: None,
        protocol: String::new(),
        negotiated: None,
    }
}

/// Opens the four fixed data channels and spawns a task per channel that
/// forwards `OnMessage` events and logs `OnOpen`/`OnClose`. Message handling
/// beyond logging/forwarding is out of scope for this slice, except for the
/// `control` channel's handle, which this returns so `PeerSession::new` can
/// keep it aside for `send_control`.
///
/// A free function (rather than a `PeerSession` method) because it runs
/// before the session exists -- `PeerSession::new` builds it from this
/// call's result, see `data_channel_specs`'s doc comment on why the
/// channels are opened before the offer.
async fn open_data_channels(
    pc: &dyn PeerConnection,
    events: mpsc::Sender<SessionEvent>,
    tag: u64,
    file_tx: mpsc::Sender<FileChannelMessage>,
) -> anyhow::Result<(Arc<dyn DataChannel>, Arc<dyn DataChannel>)> {
    let mut control_channel = None;
    let mut file_channel = None;
    for (label, init) in data_channel_specs() {
        let dc = pc.create_data_channel(label, Some(init)).await?;
        let mut direct = None;
        if label == "control" {
            control_channel = Some(Arc::clone(&dc));
        }
        if label == "file" {
            file_channel = Some(Arc::clone(&dc));
            direct = Some(file_tx.clone());
        }
        spawn_data_channel_reader(label, dc, events.clone(), tag, direct);
    }
    // `file_tx` itself is dropped here: only the reader task keeps a sender,
    // so the receiver sees the end of the stream when the channel closes.
    Ok((
        control_channel
            .ok_or_else(|| anyhow::anyhow!("data_channel_specs did not include \"control\""))?,
        file_channel
            .ok_or_else(|| anyhow::anyhow!("data_channel_specs did not include \"file\""))?,
    ))
}

impl PeerSession {
    /// Builds a `PeerConnection` with the single H.264 video codec
    /// registered, adds the video track, opens the four fixed data
    /// channels, and returns the session ready for `create_offer`.
    pub async fn new(
        cfg: SessionConfig,
        events: mpsc::Sender<SessionEvent>,
        runtime: Arc<dyn Runtime>,
    ) -> anyhow::Result<Arc<PeerSession>> {
        let mut media_engine = MediaEngine::default();
        let video_codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: H264_FMTP_LINE.to_owned(),
                rtcp_feedback: vec![
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: String::new(),
                    },
                    RTCPFeedback {
                        typ: "nack".to_owned(),
                        parameter: "pli".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "ccm".to_owned(),
                        parameter: "fir".to_owned(),
                    },
                    RTCPFeedback {
                        typ: "goog-remb".to_owned(),
                        parameter: String::new(),
                    },
                ],
            },
            payload_type: VIDEO_PAYLOAD_TYPE,
        };
        media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;
        // D76 (task 010): without abs-send-time the browser's REMB estimator
        // (libwebrtc, same in Chrome and Firefox) falls back to RTP
        // timestamps, i.e. capture time, so encoder latency and pacing look
        // like a growing queue and REMB sits on the floor. Each packet is
        // stamped with its real send time in `start_video`.
        media_engine.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: ABS_SEND_TIME_URI.to_owned(),
            },
            RtpCodecKind::Video,
            None,
        )?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
        // See `RtcpForwarder`'s doc comment: without it, PLI/FIR from the
        // client never reaches `TrackLocalEvent::OnRtcpPacket`.
        let registry = registry.with(|next| RtcpForwarder {
            next,
            read_queue: VecDeque::new(),
        });

        let ice_servers = cfg
            .ice_servers
            .iter()
            .map(|server| RTCIceServer {
                urls: server.urls.clone(),
                username: server.username.clone().unwrap_or_default(),
                credential: server.credential.clone().unwrap_or_default(),
            })
            .collect();
        let mut builder = RTCConfigurationBuilder::new().with_ice_servers(ice_servers);
        let relay_only = std::env::var(RELAY_ONLY_ENV).ok();
        if relay_only_from_env_value(relay_only.as_deref()) {
            tracing::warn!("ICE relay-only mode ({RELAY_ONLY_ENV}): only TURN candidates");
            builder = builder.with_ice_transport_policy(RTCIceTransportPolicy::Relay);
        }
        let configuration = builder.build();

        let tag = next_session_tag();
        let connected = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(Handler {
            events: events.clone(),
            connected: Arc::clone(&connected),
            tag,
        });

        let pc: Box<dyn PeerConnection> = Box::new(
            PeerConnectionBuilder::new()
                .with_configuration(configuration)
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_handler(handler)
                .with_runtime(runtime)
                .with_udp_addrs(cfg.udp_addrs.clone())
                .build()
                .await?,
        );

        let ssrc = rand::random::<u32>();
        let video_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "rcdesk-stream".to_owned(),
            "rcdesk-video".to_owned(),
            "screen".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: video_codec.rtp_codec.clone(),
                ..Default::default()
            }],
        )));

        let video_sender = pc
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>)
            .await?;

        let (file_tx, file_rx) = mpsc::channel(FILE_QUEUE_CAPACITY);
        let (control_channel, file_channel) =
            open_data_channels(pc.as_ref(), events.clone(), tag, file_tx).await?;

        let session = Arc::new(PeerSession {
            pc,
            video_track,
            video_sender,
            events: events.clone(),
            connected,
            control_channel,
            file_channel,
            file_rx: std::sync::Mutex::new(Some(file_rx)),
            tag,
        });

        Ok(session)
    }

    /// The currently selected ICE candidate pair (host/srflx/relay, RTT), or
    /// `None` while ICE has not selected one. Diagnostics only.
    pub async fn selected_path(&self) -> Option<PathInfo> {
        let report = self.pc.get_stats(Instant::now(), StatsSelector::None).await;
        path_from_report(&report)
    }

    /// Cumulative bytes sent/received over the connection since it started,
    /// and whether the selected path is a relay. `None` while the transport
    /// has no stats yet.
    pub async fn traffic_stats(&self) -> Option<TrafficStats> {
        let report = self.pc.get_stats(Instant::now(), StatsSelector::None).await;
        traffic_from_report(&report)
    }

    /// This session's unique tag (fix to slice 3.5b) -- see
    /// `SessionEvent::ConnectionState`'s doc comment. `crate::signaling`
    /// stashes this in `ActiveSession::session_tag` right after
    /// construction, to recognize a stale event from a superseded session.
    pub fn tag(&self) -> u64 {
        self.tag
    }

    /// A clone of the session's event sender (D58: lets the video forward
    /// task report `SessionEvent::VideoFailed`).
    pub fn events(&self) -> mpsc::Sender<SessionEvent> {
        self.events.clone()
    }

    /// Hands out the `file` data channel together with the receiver of its
    /// incoming messages (slice 3.4a). Only the first call returns `Some`;
    /// `crate::files::run` owns both for the rest of the session.
    pub fn take_file_channel(
        &self,
    ) -> Option<(Arc<dyn DataChannel>, mpsc::Receiver<FileChannelMessage>)> {
        let rx = self
            .file_rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()?;
        Some((Arc::clone(&self.file_channel), rx))
    }

    /// Creates an SDP offer, sets it as the local description, and returns
    /// its SDP text for the signaling layer to send to the peer.
    pub async fn create_offer(&self) -> anyhow::Result<String> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        Ok(offer.sdp)
    }

    /// Applies a remote answer received over signaling.
    pub async fn set_answer(&self, sdp: String) -> anyhow::Result<()> {
        let answer = RTCSessionDescription::answer(sdp)?;
        self.pc.set_remote_description(answer).await?;
        Ok(())
    }

    /// Applies a remote ICE candidate received over signaling (trickle ICE).
    pub async fn add_remote_ice(
        &self,
        candidate: proto::signal::IceCandidate,
    ) -> anyhow::Result<()> {
        self.pc
            .add_ice_candidate(ice_candidate_to_rtc(candidate))
            .await?;
        Ok(())
    }

    /// Closes the underlying peer connection.
    pub async fn close(&self) {
        if let Err(err) = self.pc.close().await {
            tracing::warn!(?err, "error closing peer connection");
        }
    }

    /// Sends a `ControlMessage` down the `control` data channel, JSON
    /// encoded (see ARCHITECTURE.md §5). A channel that isn't open yet (or
    /// any more) is not an error -- there is nobody to receive the message,
    /// and the caller (see `crate::cursor`'s watcher task in
    /// `crate::signaling`) has no queue to retry into; the next state change
    /// will be sent once the channel does open. Returns `Ok(false)` in that
    /// case so a caller that *does* want to retry (the adaptation task's
    /// `Quality` announcement, see `crate::signaling::run_adapt_task`) can
    /// tell "dropped" from "delivered" (`Ok(true)`).
    pub async fn send_control(&self, msg: &proto::control::ControlMessage) -> anyhow::Result<bool> {
        let ready_state = self
            .control_channel
            .ready_state()
            .await
            .unwrap_or(RTCDataChannelState::Closed);
        if ready_state != RTCDataChannelState::Open {
            tracing::trace!(?ready_state, "control channel not open, dropping message");
            return Ok(false);
        }
        let json = serde_json::to_string(msg)?;
        self.control_channel.send_text(&json).await?;
        Ok(true)
    }

    /// Starts the two tasks that drive the video track:
    /// - one writes every `EncodedFrame` coming from the pipeline out as an
    ///   RTP sample, once the sender/track have negotiated an SSRC/payload
    ///   type (frames before that point are dropped -- there's nobody to
    ///   send them to yet). The pipeline starts encoding before negotiation
    ///   completes, so its own startup keyframe is typically among the
    ///   dropped frames; this task forces a fresh one the moment negotiation
    ///   completes (ARCHITECTURE.md §4.1: keyframe "on PLI/FIR from the
    ///   client, and on connect"), so the first frame actually delivered to
    ///   a newly connected peer is always decodable rather than waiting for
    ///   the encoder's periodic interval or a client-side PLI;
    /// - one polls the track for RTCP feedback and sets `request_keyframe`
    ///   (mirroring `PipelineHandle::request_keyframe`) plus emits
    ///   `SessionEvent::KeyframeRequested` when the remote peer asks for a
    ///   keyframe via PLI or FIR.
    pub fn start_video(
        self: &Arc<Self>,
        mut frames: mpsc::Receiver<EncodedFrame>,
        request_keyframe: Arc<AtomicBool>,
    ) {
        // Set by the RTCP task below on the first receiver report that
        // covers our video SSRC -- i.e. the first proof Chrome is actually
        // getting our video, not just that the peer connection is up. Read
        // by the startup keyframe task to know when to stop re-arming (see
        // `STARTUP_KEYFRAME_INTERVAL`'s doc comment).
        let video_delivered = Arc::new(AtomicBool::new(false));

        let session = Arc::clone(self);
        let request_keyframe_on_connect = Arc::clone(&request_keyframe);
        // Cloned now, before the RTCP task below moves `request_keyframe`
        // into its closure.
        let request_keyframe_startup = Arc::clone(&request_keyframe);
        tokio::spawn(async move {
            let request_keyframe = request_keyframe_on_connect;
            let mut negotiated: Option<(u32, u8)> = None;
            // Built lazily once `negotiated` is known (the payload type
            // can't be changed on an existing `Packetizer`), then lives for
            // the rest of this task -- see `docs/host-libs-api-notes.md`'s
            // webrtc section for why this transport packetizes by hand
            // instead of using `TrackLocalStaticSample`.
            let mut packetizer: Option<Box<dyn Packetizer>> = None;
            let mut prev_captured_at: Option<Instant> = None;
            let mut last_rearm: Option<Instant> = None;
            let send_clock = NtpClock::now();
            while let Some(frame) = frames.recv().await {
                // Wait for the connection to actually be up (DTLS/SRTP
                // ready), not just SDP-negotiated: `get_parameters()` below
                // can return a payload type well before that, and writing a
                // sample that early is silently dropped by the SRTP layer
                // (no local SRTP context yet) -- including, worst case, the
                // forced keyframe this loop sends on the first frame it
                // actually forwards.
                if negotiated.is_none() && session.connected.load(Ordering::Acquire) {
                    negotiated = session.negotiated_ssrc_and_pt().await;
                    if negotiated.is_some() {
                        request_keyframe.store(true, Ordering::Release);
                    }
                }
                let Some((ssrc, pt)) = negotiated else {
                    // Not connected/negotiated yet: nobody to send this
                    // frame to. Dropping it silently used to park this loop
                    // on `frames.recv()` forever whenever the host screen was
                    // static, because no further frame is ever captured --
                    // measured on the Win10 stand (2026-09-20): 15-17s of
                    // black screen on connect, then a picture only once per
                    // client PLI. Re-arm the keyframe request instead: the
                    // pipeline's idle keyframe repeat (slice 2.6f) answers
                    // within `SLOT_WAIT_TIMEOUT` even with nothing new
                    // captured. Rate-limited so a session that never
                    // negotiates can't spin the encoder.
                    if last_rearm.is_none_or(|at| at.elapsed() >= KEYFRAME_REARM_INTERVAL) {
                        last_rearm = Some(Instant::now());
                        request_keyframe.store(true, Ordering::Release);
                        tracing::debug!("video not negotiated yet, re-requesting a keyframe");
                    }
                    continue;
                };
                let packetizer = packetizer.get_or_insert_with(|| {
                    Box::new(new_packetizer(
                        RTP_MTU,
                        pt,
                        ssrc,
                        Box::new(H264Payloader::default()),
                        Box::new(new_random_sequencer()),
                        VIDEO_CLOCK_RATE,
                    ))
                });

                // Real inter-frame gap (screen capture only produces frames
                // when the screen changes, so this varies -- it is not
                // `1/fps`), converted to RTP clock ticks and applied via
                // `skip_samples` *before* packetizing so the RTP timestamp
                // tracks wall-clock capture time instead of a fixed step.
                // `skip_samples` only advances the packetizer's timestamp,
                // it does not touch the sequencer (`packetizer/mod.rs`).
                let gap = prev_captured_at
                    .map(|prev| frame.captured_at.duration_since(prev))
                    .unwrap_or_default()
                    .min(MAX_CAPTURE_GAP);
                prev_captured_at = Some(frame.captured_at);
                let skip_samples = (gap.as_secs_f64() * f64::from(VIDEO_CLOCK_RATE)) as u32;
                if skip_samples > 0 {
                    packetizer.skip_samples(skip_samples);
                }

                let keyframe = frame.keyframe;
                let packets = match packetizer.packetize(&Bytes::from(frame.data), 0) {
                    Ok(packets) => packets,
                    Err(err) => {
                        tracing::warn!(?err, "failed to packetize video frame");
                        continue;
                    }
                };
                // Pacing (audit 2026-10-13, slice F2): an IDR of 126-322 KB
                // is 110-270 packets; sent back to back that burst overflows
                // Wi-Fi/TURN queues -> loss -> PLI -> new IDR -> loop, and
                // Chrome's GCC cuts REMB. Send in bursts of
                // `PACE_BURST_PACKETS` with `PACE_BURST_GAP` between them,
                // but stop pausing after `PACE_MAX_SPREAD` so pacing never
                // costs more than about one frame interval of latency.
                let started = Instant::now();
                let total = packets.len();
                for (i, mut pkt) in packets.into_iter().enumerate() {
                    if let Some(gap) = pace_gap(i, total, started.elapsed()) {
                        tokio::time::sleep(gap).await;
                    }
                    pkt.header.payload_type = pt;
                    // Stamped after the pacing pause, right before the write:
                    // the receiver's estimator needs the send time, not the
                    // frame's capture time.
                    let send_time = abs_send_time(send_clock.ntp64(Instant::now()));
                    if let Err(err) = session
                        .video_track
                        .write_rtp_with_extensions(pkt, &[send_time])
                        .await
                    {
                        tracing::warn!(?err, "failed to write video packet");
                    }
                }
                if total > PACE_BURST_PACKETS {
                    tracing::debug!(
                        packets = total,
                        keyframe,
                        spread_ms = started.elapsed().as_secs_f64() * 1000.0,
                        "paced video frame"
                    );
                }
            }
        });

        let session = Arc::clone(self);
        let video_delivered_rtcp = Arc::clone(&video_delivered);
        tokio::spawn(async move {
            // The video track's SSRC is assigned explicitly at track creation
            // (see `PeerSession::new`'s `RTCRtpCodingParameters { ssrc: Some(ssrc),
            // .. }`), not negotiated, so it is already available before the
            // connection is up; fetched once here rather than per packet.
            let video_ssrc = session.video_track.ssrcs().await.first().copied();
            let ntp_clock = NtpClock::now();
            while let Some(event) = session.video_track.poll().await {
                match event {
                    TrackLocalEvent::OnRtcpPacket(packets) => {
                        let mut keyframe_requested = false;
                        for pkt in &packets {
                            let pkt_any = pkt.as_any();
                            if pkt_any.downcast_ref::<PictureLossIndication>().is_some()
                                || pkt_any.downcast_ref::<FullIntraRequest>().is_some()
                            {
                                keyframe_requested = true;
                            } else if let Some(remb) =
                                pkt_any.downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                            {
                                let _ = session
                                    .events
                                    .send(SessionEvent::Remb {
                                        bitrate_bps: remb.bitrate as u64,
                                    })
                                    .await;
                            } else if let Some(rr) = pkt_any.downcast_ref::<RtcpReceiverReport>() {
                                for report in &rr.reports {
                                    if Some(report.ssrc) != video_ssrc {
                                        continue;
                                    }
                                    // First proof Chrome is actually
                                    // receiving our video (see
                                    // `STARTUP_KEYFRAME_INTERVAL`'s doc
                                    // comment) -- `swap` so this logs only
                                    // once, on the first matching report.
                                    if !video_delivered_rtcp.swap(true, Ordering::Release) {
                                        tracing::info!(
                                            "video delivery confirmed by receiver report"
                                        );
                                    }
                                    let rtt = rtt_from_report(
                                        ntp_clock.mid32(Instant::now()),
                                        report.last_sender_report,
                                        report.delay,
                                    );
                                    let _ = session
                                        .events
                                        .send(SessionEvent::ReceiverReport {
                                            fraction_lost: f32::from(report.fraction_lost) / 256.0,
                                            total_lost: report.total_lost,
                                            jitter: report.jitter,
                                            rtt,
                                        })
                                        .await;
                                }
                            } else {
                                tracing::trace!(packet = %pkt, "video track rtcp feedback");
                            }
                        }
                        if keyframe_requested {
                            request_keyframe.store(true, Ordering::Release);
                            let _ = session.events.send(SessionEvent::KeyframeRequested).await;
                        }
                    }
                }
            }
        });

        // Startup keyframe re-arm: covers the DTLS server/client asymmetry
        // window between the host's `Connected` and Chrome's own -- see
        // `STARTUP_KEYFRAME_INTERVAL`'s doc comment. Holds a `Weak` (not an
        // `Arc`) to `self` so this task never keeps the session alive on its
        // own; it bails out via `upgrade()` failing as soon as the session
        // is dropped instead of running for up to 15s past close.
        let session_weak: Weak<PeerSession> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match session_weak.upgrade() {
                    Some(session) if session.connected.load(Ordering::Acquire) => break,
                    Some(_) => {}
                    None => return,
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            let mut keyframe = StartupKeyframe::new(Instant::now());
            loop {
                if session_weak.upgrade().is_none() {
                    return;
                }
                let now = Instant::now();
                let delivered = video_delivered.load(Ordering::Acquire);
                if keyframe.should_request(now, delivered) {
                    request_keyframe_startup.store(true, Ordering::Release);
                    tracing::debug!(
                        "startup keyframe re-arm: requesting keyframe until receiver \
                         confirms video delivery"
                    );
                }
                if keyframe.finished(now, delivered) {
                    if !delivered {
                        tracing::warn!("no receiver report for video within 15s");
                    }
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }

    /// Resolves the sender's negotiated payload type and the track's SSRC,
    /// once negotiation has completed. `None` before that.
    async fn negotiated_ssrc_and_pt(&self) -> Option<(u32, u8)> {
        let params = self.video_sender.get_parameters().await.ok()?;
        let pt = params.rtp_parameters.codecs.first()?.payload_type;
        let ssrc = *self.video_track.ssrcs().await.first()?;
        Some((ssrc, pt))
    }
}

fn spawn_data_channel_reader(
    label: &'static str,
    dc: Arc<dyn DataChannel>,
    events: mpsc::Sender<SessionEvent>,
    tag: u64,
    direct: Option<mpsc::Sender<FileChannelMessage>>,
) {
    tokio::spawn(async move {
        while let Some(event) = dc.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    tracing::info!(label, "data channel open");
                    let _ = events
                        .send(SessionEvent::DataChannelOpen {
                            label: label.to_owned(),
                        })
                        .await;
                }
                DataChannelEvent::OnClose => {
                    tracing::info!(label, "data channel closed");
                    let _ = events
                        .send(SessionEvent::DataChannelClosed {
                            label: label.to_owned(),
                            tag,
                        })
                        .await;
                    break;
                }
                DataChannelEvent::OnMessage(msg) => {
                    tracing::trace!(label, len = msg.data.len(), "data channel message");
                    if let Some(direct) = &direct {
                        // Bulk `file` traffic bypasses `SessionEvent`; see
                        // `FileChannelMessage`.
                        let _ = direct
                            .send(FileChannelMessage {
                                data: msg.data.freeze(),
                                is_string: msg.is_string,
                            })
                            .await;
                        continue;
                    }
                    let _ = events
                        .send(SessionEvent::DataChannelMessage {
                            label: label.to_owned(),
                            data: msg.data.freeze(),
                            is_string: msg.is_string,
                        })
                        .await;
                }
                _ => {}
            }
        }
    });
}

/// Round-trip time computed from an RTCP receiver report's LSR/DLSR fields
/// against our own NTP clock (RFC 3550 §6.4.1): `now - LSR - DLSR`, all in
/// units of 1/65536 s (the middle 32 bits of the 64-bit NTP timestamp).
/// `None` when the report carries no LSR yet (no SR seen on the far side, so
/// `last_sender_report == 0`) or the result is implausible (more than 10s,
/// which for a report sent roughly once a second means a clock jump or a
/// wraparound rather than a real RTT).
///
/// A *slightly* negative result (down to `-RTT_SKEW_TOLERANCE`) is reported
/// as zero rather than rejected: `now` comes from an `NtpClock` captured by
/// us, while the SR's NTP stamp comes from `rtc`'s `SystemInstant` (a
/// `SystemTime`/`Instant` pair captured once per stream, then advanced by
/// `Instant`). Both are "baseline + monotonic", so they differ only by the
/// wall-clock drift between the two captures (microseconds) -- unlike a
/// per-report `SystemTime::now()`, which drifted from the monotonic clock
/// over the whole session and grew the RTT linearly. On a real network the RTT
/// dwarfs the residual; in the in-process loopback test (true RTT ~0.1 ms) it
/// made the RTT come out as garbage on roughly one run in eight.
fn rtt_from_report(now_ntp_mid32: u32, last_sender_report: u32, delay: u32) -> Option<Duration> {
    if last_sender_report == 0 {
        return None;
    }
    // Interpreted as signed: a `now` slightly *before* `LSR + DLSR` (see the
    // doc comment) shows up as a small negative number rather than a huge
    // unsigned one.
    let diff = now_ntp_mid32
        .wrapping_sub(last_sender_report)
        .wrapping_sub(delay) as i32;
    if !(-RTT_SKEW_TOLERANCE..=RTT_MAX_PLAUSIBLE).contains(&diff) {
        return None;
    }
    Some(Duration::from_secs_f64(f64::from(diff.max(0)) / 65536.0))
}

/// Largest negative `now - LSR - DLSR` (in 1/65536 s; 100 ms) still
/// attributed to clock skew between our `NtpClock` and the SR's NTP stamp
/// rather than to a bogus report -- see `rtt_from_report`.
const RTT_SKEW_TOLERANCE: i32 = 65536 / 10;

/// Largest `now - LSR - DLSR` (in 1/65536 s; 10 s) still taken as a real
/// round trip -- see `rtt_from_report`.
const RTT_MAX_PLAUSIBLE: i32 = 65536 * 10;

/// A wall-clock reading built the same way as `rtc`'s `SystemInstant`, which
/// stamps our RTCP sender reports: a `SystemTime` (as a duration since the
/// Unix epoch) and an `Instant` captured together once, after which time is
/// `baseline + (now - baseline_instant)` on the monotonic clock. Reading
/// `SystemTime::now()` per report instead would let the NTP-disciplined wall
/// clock and the monotonic clock drift apart by some ppm over a whole
/// session, making the RTT grow linearly. With our own pair captured shortly
/// after rtc's, the two baselines differ only by the clock drift over that
/// gap (seconds, i.e. microseconds), not over the session.
struct NtpClock {
    baseline_instant: Instant,
    baseline_unix: Duration,
}

impl NtpClock {
    fn now() -> Self {
        let baseline_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            baseline_instant: Instant::now(),
            baseline_unix,
        }
    }

    /// The middle 32 bits of the 64-bit NTP timestamp (seconds since
    /// 1900-01-01 plus a binary fraction of a second, RFC 3550 §4) for the
    /// monotonic instant `now`, as used by RTCP SR/RR LSR/DLSR fields.
    fn mid32(&self, now: Instant) -> u32 {
        let unix = self.baseline_unix + now.saturating_duration_since(self.baseline_instant);
        let secs = unix.as_secs() + 2_208_988_800;
        let nanos_fraction = (u64::from(unix.subsec_nanos()) * (1u64 << 32) / 1_000_000_000) as u32;
        (((secs & 0xFFFF) as u32) << 16) | (nanos_fraction >> 16)
    }

    /// The full 64-bit NTP timestamp (32.32 fixed point) for `now`.
    fn ntp64(&self, now: Instant) -> u64 {
        let unix = self.baseline_unix + now.saturating_duration_since(self.baseline_instant);
        let secs = unix.as_secs() + 2_208_988_800;
        let fraction = u64::from(unix.subsec_nanos()) * (1u64 << 32) / 1_000_000_000;
        (secs << 32) | fraction
    }
}

/// The abs-send-time header extension for a packet sent at NTP time `ntp`:
/// 24 bits, 6.18 fixed-point seconds (wraps every 64 s; the receiver only
/// uses differences).
fn abs_send_time(ntp: u64) -> HeaderExtension {
    HeaderExtension::AbsSendTime(AbsSendTimeExtension::new(ntp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_only_env_value_table() {
        for v in ["1", "true", "yes", "TRUE", "Yes", " 1 ", "\ttrue\n"] {
            assert!(relay_only_from_env_value(Some(v)), "{v:?}");
        }
        for v in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("on"),
            Some("garbage"),
            Some("  "),
        ] {
            assert!(!relay_only_from_env_value(v), "{v:?}");
        }
    }

    fn cand(typ: &str, addr: &str) -> CandidateInfo {
        CandidateInfo {
            typ: typ.into(),
            protocol: "udp".into(),
            address: addr.into(),
        }
    }

    fn path(l: &str, r: &str) -> PathInfo {
        PathInfo {
            local: cand(l, "192.168.1.77:55544"),
            remote: cand(r, "192.168.1.50:61234"),
            rtt_ms: Some(1.2),
            packets_discarded_on_send: 0,
        }
    }

    #[test]
    fn path_label_formats_types_or_question_mark() {
        assert_eq!(
            PathInfo::path_label(Some(&path("host", "srflx"))),
            "host>srflx"
        );
        assert_eq!(PathInfo::path_label(None), "?");
    }

    #[test]
    fn candidate_describe_and_same_pair() {
        assert_eq!(
            cand("host", "192.168.1.77:55544").describe(),
            "host/udp 192.168.1.77:55544"
        );
        let a = path("host", "host");
        let mut b = a.clone();
        b.rtt_ms = Some(9.0);
        assert!(a.same_pair(&b));
        assert!(!a.same_pair(&path("host", "relay")));
    }

    #[test]
    fn candidate_id_matches_real_webrtc_rs_ids() {
        assert!(candidate_id_matches("candidate:56o", "candidate:56o"));
        assert!(candidate_id_matches(
            "RTCLocalIceCandidate_candidate:56o",
            "candidate:56o"
        ));
        assert!(candidate_id_matches(
            "RTCRemoteIceCandidate_candidate:bXv",
            "candidate:bXv"
        ));
        assert!(!candidate_id_matches(
            "RTCLocalIceCandidate_candidate:56o",
            "candidate:bXv"
        ));
        assert!(!candidate_id_matches("anything", ""));
    }

    #[test]
    fn traffic_from_parts_sums_transport_and_flags_relay() {
        let t = |l: &str, r: &str| TrafficStats::from_parts(Some((10, 20)), Some(&path(l, r)));
        assert_eq!(
            t("host", "srflx"),
            Some(TrafficStats {
                bytes_sent: 10,
                bytes_received: 20,
                relay: false
            })
        );
        assert!(t("relay", "host").unwrap().relay);
        assert!(t("host", "relay").unwrap().relay);
        assert!(!TrafficStats::from_parts(Some((1, 2)), None).unwrap().relay);
        assert!(TrafficStats::from_parts(None, Some(&path("host", "host"))).is_none());
        assert!(traffic_from_report(&RTCStatsReport::default()).is_none());
    }

    #[test]
    fn empty_report_has_no_path() {
        assert!(path_from_report(&RTCStatsReport::default()).is_none());
    }

    #[test]
    fn pace_gap_small_frame_never_pauses() {
        for next in 0..=10 {
            assert_eq!(pace_gap(next, 10, Duration::ZERO), None);
        }
    }

    #[test]
    fn pace_gap_pauses_at_burst_boundaries_only() {
        for next in 0..100 {
            let expected = matches!(next, 32 | 64 | 96);
            assert_eq!(
                pace_gap(next, 100, Duration::ZERO).is_some(),
                expected,
                "next = {next}"
            );
        }
        assert_eq!(pace_gap(32, 100, Duration::ZERO), Some(PACE_BURST_GAP));
    }

    #[test]
    fn pace_gap_exact_burst_frame_has_no_trailing_pause() {
        assert_eq!(pace_gap(32, 32, Duration::ZERO), None);
    }

    #[test]
    fn pace_gap_stops_after_max_spread() {
        assert_eq!(pace_gap(32, 100, PACE_MAX_SPREAD), None);
        assert_eq!(pace_gap(64, 100, PACE_MAX_SPREAD * 2), None);
    }

    #[test]
    fn rtt_from_report_computes_expected_round_trip() {
        let lsr: u32 = 0x1234_5678;
        let delay: u32 = 655;
        let now = lsr.wrapping_add(3277).wrapping_add(delay);
        let rtt = rtt_from_report(now, lsr, delay).expect("expected Some(rtt)");
        let expected = Duration::from_secs_f64(3277.0 / 65536.0);
        let diff = rtt.abs_diff(expected);
        assert!(
            diff <= Duration::from_millis(1),
            "expected ~{expected:?}, got {rtt:?} (diff {diff:?})"
        );
    }

    #[test]
    fn rtt_from_report_none_without_lsr() {
        assert_eq!(rtt_from_report(12345, 0, 0), None);
    }

    #[test]
    fn rtt_from_report_none_when_implausible() {
        // `now` is far earlier than `last_sender_report + delay`: the
        // wrapping subtraction produces a huge value, well past the 10s
        // sanity bound.
        assert_eq!(rtt_from_report(0, 1_000_000, 0), None);
    }

    #[test]
    fn rtt_from_report_treats_small_negative_skew_as_zero() {
        // `now` 5 ms (328 units) before LSR + DLSR: clock skew, not a bogus
        // report -- RTT 0.
        let lsr: u32 = 0x1234_5678;
        let delay: u32 = 655;
        let now = lsr.wrapping_add(delay).wrapping_sub(328);
        assert_eq!(rtt_from_report(now, lsr, delay), Some(Duration::ZERO));
        // 200 ms before: beyond the tolerance -- None.
        let now = lsr.wrapping_add(delay).wrapping_sub(13_107);
        assert_eq!(rtt_from_report(now, lsr, delay), None);
    }

    #[test]
    fn startup_keyframe_requests_immediately() {
        let base = Instant::now();
        let mut sk = StartupKeyframe::new(base);
        assert!(sk.should_request(base, false));
    }

    #[test]
    fn startup_keyframe_reissues_after_interval() {
        let base = Instant::now();
        let mut sk = StartupKeyframe::new(base);
        assert!(sk.should_request(base, false));
        assert!(!sk.should_request(base + Duration::from_millis(200), false));
        assert!(sk.should_request(base + Duration::from_millis(500), false));
    }

    #[test]
    fn startup_keyframe_stops_once_delivered() {
        let base = Instant::now();
        let mut sk = StartupKeyframe::new(base);
        assert!(sk.should_request(base, false));
        let now = base + Duration::from_secs(1);
        assert!(!sk.should_request(now, true));
        assert!(sk.finished(now, true));
    }

    #[test]
    fn startup_keyframe_stops_after_deadline() {
        let base = Instant::now();
        let mut sk = StartupKeyframe::new(base);
        assert!(sk.should_request(base, false));
        let now = base + Duration::from_secs(15);
        assert!(sk.finished(now, false));
        assert!(!sk.should_request(now, false));
    }

    fn test_clock() -> NtpClock {
        NtpClock {
            baseline_instant: Instant::now(),
            baseline_unix: Duration::new(1_700_000_000, 123_456_789),
        }
    }

    #[test]
    fn ntp_clock_advances_one_second_by_65536_units() {
        let clock = test_clock();
        let base = clock.mid32(clock.baseline_instant);
        let later = clock.mid32(clock.baseline_instant + Duration::from_secs(1));
        assert_eq!(later.wrapping_sub(base), 65536);
    }

    #[test]
    fn ntp_clock_advances_quarter_second_by_16384_units() {
        let clock = test_clock();
        let base = clock.mid32(clock.baseline_instant);
        let later = clock.mid32(clock.baseline_instant + Duration::from_millis(250));
        let diff = later.wrapping_sub(base);
        assert!(
            (16383..=16385).contains(&diff),
            "250ms should be ~16384 units, got {diff}"
        );
    }

    #[test]
    fn ntp_clock_baseline_matches_direct_conversion() {
        let clock = test_clock();
        let unix = clock.baseline_unix;
        let secs = unix.as_secs() + 2_208_988_800;
        let fraction = (u64::from(unix.subsec_nanos()) * (1u64 << 32) / 1_000_000_000) as u32;
        let expected = (((secs & 0xFFFF) as u32) << 16) | (fraction >> 16);
        assert_eq!(clock.mid32(clock.baseline_instant), expected);
    }

    #[test]
    fn ntp_clock_ntp64_middle_bits_match_mid32() {
        let clock = test_clock();
        for offset in [
            Duration::ZERO,
            Duration::from_millis(1234),
            Duration::from_secs(90),
        ] {
            let now = clock.baseline_instant + offset;
            assert_eq!((clock.ntp64(now) >> 16) as u32, clock.mid32(now));
        }
    }

    /// The 24-bit 6.18 value abs-send-time puts on the wire.
    fn abs_send_time_wire(ntp: u64) -> u32 {
        let HeaderExtension::AbsSendTime(ext) = abs_send_time(ntp) else {
            panic!("abs_send_time must build an AbsSendTime extension");
        };
        (ext.timestamp & 0xFF_FFFF) as u32
    }

    #[test]
    fn abs_send_time_advances_2_pow_18_per_second() {
        let clock = test_clock();
        let base = abs_send_time_wire(clock.ntp64(clock.baseline_instant));
        let later =
            abs_send_time_wire(clock.ntp64(clock.baseline_instant + Duration::from_millis(1500)));
        let diff = later.wrapping_sub(base) & 0xFF_FFFF;
        assert!(
            ((3 << 17) - 1..=(3 << 17) + 1).contains(&diff),
            "1.5 s should be ~{} units, got {diff}",
            3 << 17
        );
    }

    #[test]
    fn abs_send_time_wraps_every_64_seconds() {
        let clock = test_clock();
        let base = abs_send_time_wire(clock.ntp64(clock.baseline_instant));
        let later =
            abs_send_time_wire(clock.ntp64(clock.baseline_instant + Duration::from_secs(64)));
        assert_eq!(base, later);
    }
}
