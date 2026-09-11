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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

use rtc::interceptor::{Interceptor, Packet as InterceptorPacket, TaggedPacket};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

use webrtc::data_channel::{
    DataChannel, DataChannelEvent, RTCDataChannelInit, RTCDataChannelState,
};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry,
};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::runtime::Runtime;

use crate::encode::EncodedFrame;

/// Payload type for the (only) negotiated video codec. Fixed rather than
/// negotiated because the media engine registers exactly one video codec.
const VIDEO_PAYLOAD_TYPE: u8 = 102;

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
    /// Target video frame rate, used to stamp sample durations.
    pub fps: u32,
}

/// Events a `PeerSession` reports back to its owner (the signaling client).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A local ICE candidate was gathered and should be sent to the peer.
    LocalIce(proto::signal::IceCandidate),
    /// The peer connection's aggregate connection state changed.
    ConnectionState(RTCPeerConnectionState),
    /// A message arrived on one of the fixed data channels.
    DataChannelMessage {
        label: String,
        data: Bytes,
        is_string: bool,
    },
    /// The remote peer asked for a keyframe (PLI or FIR).
    KeyframeRequested,
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
        let _ = self.events.send(SessionEvent::ConnectionState(state)).await;
    }
}

/// One WebRTC peer session: a `PeerConnection`, its single video track and
/// the four fixed data channels.
pub struct PeerSession {
    pc: Box<dyn PeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    video_sender: Arc<dyn RtpSender>,
    events: mpsc::Sender<SessionEvent>,
    connected: Arc<AtomicBool>,
    fps: u32,
    /// The `control` data channel (cursor shape, app-level ping/pong; see
    /// ARCHITECTURE.md §5), kept aside from the other three so
    /// `send_control` can write to it directly.
    control_channel: Arc<dyn DataChannel>,
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
) -> anyhow::Result<Arc<dyn DataChannel>> {
    let mut control_channel = None;
    for (label, init) in data_channel_specs() {
        let dc = pc.create_data_channel(label, Some(init)).await?;
        if label == "control" {
            control_channel = Some(Arc::clone(&dc));
        }
        spawn_data_channel_reader(label, dc, events.clone());
    }
    control_channel.ok_or_else(|| anyhow::anyhow!("data_channel_specs did not include \"control\""))
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
        let configuration = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers)
            .build();

        let connected = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(Handler {
            events: events.clone(),
            connected: Arc::clone(&connected),
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
        let video_track = Arc::new(TrackLocalStaticSample::new(MediaStreamTrack::new(
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
        ))?);

        let video_sender = pc
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>)
            .await?;

        let control_channel = open_data_channels(pc.as_ref(), events.clone()).await?;

        let session = Arc::new(PeerSession {
            pc,
            video_track,
            video_sender,
            events: events.clone(),
            connected,
            fps: cfg.fps.max(1),
            control_channel,
        });

        Ok(session)
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
    /// will be sent once the channel does open.
    pub async fn send_control(&self, msg: &proto::control::ControlMessage) -> anyhow::Result<()> {
        let ready_state = self
            .control_channel
            .ready_state()
            .await
            .unwrap_or(RTCDataChannelState::Closed);
        if ready_state != RTCDataChannelState::Open {
            tracing::trace!(?ready_state, "control channel not open, dropping message");
            return Ok(());
        }
        let json = serde_json::to_string(msg)?;
        self.control_channel.send_text(&json).await?;
        Ok(())
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
        let session = Arc::clone(self);
        let request_keyframe_on_connect = Arc::clone(&request_keyframe);
        tokio::spawn(async move {
            let request_keyframe = request_keyframe_on_connect;
            let mut negotiated: Option<(u32, u8)> = None;
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
                    // frame to.
                    continue;
                };

                let sample = Sample {
                    data: Bytes::from(frame.data),
                    duration: Duration::from_micros(1_000_000 / u64::from(session.fps)),
                    ..Default::default()
                };
                if let Err(err) = session
                    .video_track
                    .sample_writer(ssrc, pt)
                    .write_sample(&sample)
                    .await
                {
                    tracing::warn!(?err, "failed to write video sample");
                }
            }
        });

        let session = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(event) = session.video_track.poll().await {
                match event {
                    TrackLocalEvent::OnRtcpPacket(packets) => {
                        let mut keyframe_requested = false;
                        for pkt in &packets {
                            if pkt
                                .as_any()
                                .downcast_ref::<PictureLossIndication>()
                                .is_some()
                                || pkt.as_any().downcast_ref::<FullIntraRequest>().is_some()
                            {
                                keyframe_requested = true;
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
) {
    tokio::spawn(async move {
        while let Some(event) = dc.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    tracing::info!(label, "data channel open");
                }
                DataChannelEvent::OnClose => {
                    tracing::info!(label, "data channel closed");
                    break;
                }
                DataChannelEvent::OnMessage(msg) => {
                    tracing::trace!(label, len = msg.data.len(), "data channel message");
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
