//! Loopback proof for slice 1.2b: a `transport::PeerSession` (the host side)
//! talks WebRTC to a second, plain webrtc-rs `PeerConnection` standing in
//! for a browser, both in this process on `127.0.0.1`, no STUN/TURN and no
//! signaling server. Exercises the whole video path -- offer/answer, trickle
//! ICE, H.264 over RTP, and a PLI-triggered keyframe -- without a browser.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};

use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    register_default_interceptors, MediaEngine, PeerConnection, PeerConnectionBuilder,
    PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit,
    RTCPeerConnectionIceEvent, RTCSessionDescription, Registry,
};
use webrtc::runtime::default_runtime;

use rcdesk_host::capture::synthetic::SyntheticSource;
use rcdesk_host::capture::FrameSource;
use rcdesk_host::encode::{build_encoder, EncoderConfig, EncoderKind, RateTarget};
use rcdesk_host::pipeline::Pipeline;
use rcdesk_host::transport::{PeerSession, SessionConfig, SessionEvent};

const H264_FMTP_LINE: &str =
    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";

fn browser_video_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
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
        payload_type: 102,
    }
}

fn ice_init_to_proto(c: RTCIceCandidateInit) -> proto::signal::IceCandidate {
    proto::signal::IceCandidate {
        candidate: c.candidate,
        sdp_mid: c.sdp_mid,
        sdp_mline_index: c.sdp_mline_index,
    }
}

fn ice_proto_to_init(c: proto::signal::IceCandidate) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: c.candidate,
        sdp_mid: c.sdp_mid,
        sdp_mline_index: c.sdp_mline_index,
        username_fragment: None,
        url: None,
    }
}

/// Parses the NAL unit type(s) carried by one RTP payload under
/// `packetization-mode=1` (RFC 6184 §5.2): a plain single NAL unit (type
/// 1-23), a STAP-A aggregate (type 24: a sequence of 2-byte-length-prefixed
/// NAL units), or an FU-A fragment (type 28: the fragmented NAL's type is
/// the low 5 bits of the second payload byte).
fn rtp_payload_nal_types(payload: &[u8]) -> Vec<u8> {
    let Some(&first) = payload.first() else {
        return Vec::new();
    };
    let nal_type = first & 0x1F;
    match nal_type {
        24 => {
            let mut types = Vec::new();
            let mut i = 1;
            while i + 2 <= payload.len() {
                let len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
                i += 2;
                let Some(&nal_header) = payload.get(i) else {
                    break;
                };
                types.push(nal_header & 0x1F);
                i += len;
            }
            types
        }
        28 => payload.get(1).map(|b| vec![b & 0x1F]).unwrap_or_default(),
        _ => vec![nal_type],
    }
}

/// Stand-in for a browser: forwards local ICE candidates over `ice_tx` and
/// hands the first remote track to whoever is waiting on `track_tx`.
struct BrowserHandler {
    ice_tx: mpsc::UnboundedSender<RTCIceCandidateInit>,
    track_tx: Mutex<Option<oneshot::Sender<Arc<dyn TrackRemote>>>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for BrowserHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(candidate) = event.candidate.to_json() {
            let _ = self.ice_tx.send(candidate);
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if let Some(tx) = self.track_tx.lock().await.take() {
            let _ = tx.send(track);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_delivers_h264_and_handles_pli() {
    run().await.expect("loopback scenario failed");
}

async fn run() -> anyhow::Result<()> {
    let runtime = default_runtime().expect("runtime-tokio feature must be enabled");

    // ---- Host side: a PeerSession fed by the synthetic capture pipeline. ----
    let (host_events_tx, mut host_events_rx) = mpsc::channel::<SessionEvent>(64);
    let host_session = PeerSession::new(
        SessionConfig {
            ice_servers: vec![],
            udp_addrs: vec!["127.0.0.1:0".to_string()],
            fps: 30,
        },
        host_events_tx,
        runtime.clone(),
    )
    .await?;

    let source: Box<dyn FrameSource> = Box::new(SyntheticSource::new(320, 240, 15));
    let encoder_cfg = EncoderConfig {
        width: 320,
        height: 240,
        fps: 30,
        bitrate_kbps: 2000,
        // 10x fps, matching the signaling layer's default (see
        // signaling::start_session) -- high enough that the loop's own
        // periodic keyframes don't hide whether the PLI-forced one worked.
        keyframe_interval_frames: 300,
        max_qp: None,
    };
    let (encoder, _kind) = build_encoder(Some(EncoderKind::OpenH264), encoder_cfg)?;
    let pipeline_handle = Pipeline::start(
        source,
        encoder,
        RateTarget {
            bitrate_kbps: encoder_cfg.bitrate_kbps,
            fps: encoder_cfg.fps,
        },
    );
    let keyframe_flag = pipeline_handle.keyframe_flag();
    let stats = Arc::clone(&pipeline_handle.stats);

    // `PipelineHandle` can't be partially moved (it implements `Drop`), so
    // it's handed wholesale to a forwarding task that relays frames into a
    // fresh channel `start_video` can take by value -- the same pattern
    // `signaling::start_session` uses.
    let (video_tx, video_rx) = mpsc::channel(4);
    let forward_task = tokio::spawn(async move {
        let mut handle = pipeline_handle;
        while let Some(frame) = handle.frames.recv().await {
            if video_tx.send(frame).await.is_err() {
                break;
            }
        }
        handle.stop();
    });

    host_session.start_video(video_rx, keyframe_flag);

    // ---- "Browser" side: a second, plain webrtc-rs PeerConnection. ----
    let mut browser_media_engine = MediaEngine::default();
    browser_media_engine.register_codec(browser_video_codec(), RtpCodecKind::Video)?;
    let browser_registry =
        register_default_interceptors(Registry::new(), &mut browser_media_engine)?;

    let (browser_ice_tx, mut browser_ice_rx) = mpsc::unbounded_channel::<RTCIceCandidateInit>();
    let (track_tx, track_rx) = oneshot::channel();
    let browser_handler = Arc::new(BrowserHandler {
        ice_tx: browser_ice_tx,
        track_tx: Mutex::new(Some(track_tx)),
    });

    let browser_pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(browser_media_engine)
            .with_interceptor_registry(browser_registry)
            .with_handler(browser_handler)
            .with_runtime(runtime.clone())
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await?,
    );

    // Trickle ICE, both directions.
    {
        let host_session = Arc::clone(&host_session);
        tokio::spawn(async move {
            while let Some(candidate) = browser_ice_rx.recv().await {
                let _ = host_session
                    .add_remote_ice(ice_init_to_proto(candidate))
                    .await;
            }
        });
    }

    let (keyframe_requested_tx, mut keyframe_requested_rx) = mpsc::channel::<()>(4);
    {
        let browser_pc = Arc::clone(&browser_pc);
        tokio::spawn(async move {
            while let Some(event) = host_events_rx.recv().await {
                match event {
                    SessionEvent::LocalIce(candidate) => {
                        let _ = browser_pc
                            .add_ice_candidate(ice_proto_to_init(candidate))
                            .await;
                    }
                    SessionEvent::KeyframeRequested => {
                        let _ = keyframe_requested_tx.try_send(());
                    }
                    _ => {}
                }
            }
        });
    }

    // ---- Offer / answer. ----
    let offer_sdp = host_session.create_offer().await?;
    browser_pc
        .set_remote_description(RTCSessionDescription::offer(offer_sdp)?)
        .await?;
    let answer = browser_pc.create_answer(None).await?;
    let answer_sdp = answer.sdp.clone();
    browser_pc.set_local_description(answer).await?;
    host_session.set_answer(answer_sdp).await?;

    // (a) on_track fires within 10s.
    let track = timeout(Duration::from_secs(10), track_rx)
        .await
        .expect("on_track timed out after 10s")
        .expect("browser handler dropped the track sender");

    // A dedicated reader task continuously drains the track (only one
    // caller should poll a given TrackRemote) and updates shared counters
    // the assertions below poll -- this also keeps (d)'s PLI/RTCP path
    // exercised concurrently with packet collection.
    let packet_count = Arc::new(AtomicUsize::new(0));
    let saw_sps = Arc::new(AtomicBool::new(false));
    // (arrival time, RTP timestamp) of marker-bit packets (the last packet
    // of each encoded frame, RFC 6184 -- see `Header::marker`'s doc
    // comment), collected for the timestamp-pacing check (e) below. Capped
    // so a long-running test doesn't grow this unboundedly.
    let marker_timestamps = Arc::new(Mutex::new(Vec::<(Instant, u32)>::new()));
    const MAX_MARKER_TIMESTAMPS: usize = 60;
    {
        let packet_count = Arc::clone(&packet_count);
        let saw_sps = Arc::clone(&saw_sps);
        let marker_timestamps = Arc::clone(&marker_timestamps);
        let track = Arc::clone(&track);
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(pkt) = event {
                    packet_count.fetch_add(1, Ordering::Relaxed);
                    if rtp_payload_nal_types(&pkt.payload).contains(&7) {
                        saw_sps.store(true, Ordering::Relaxed);
                    }
                    if pkt.header.marker {
                        let mut timestamps = marker_timestamps.lock().await;
                        if timestamps.len() < MAX_MARKER_TIMESTAMPS {
                            timestamps.push((Instant::now(), pkt.header.timestamp));
                        }
                    }
                }
            }
        });
    }

    // (b) >= 30 RTP packets and (c) an SPS (NAL 7) among them, within 10s.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline
        && (packet_count.load(Ordering::Relaxed) < 30 || !saw_sps.load(Ordering::Relaxed))
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        packet_count.load(Ordering::Relaxed) >= 30,
        "expected >= 30 RTP packets within 10s, got {}",
        packet_count.load(Ordering::Relaxed)
    );
    assert!(
        saw_sps.load(Ordering::Relaxed),
        "expected an SPS (NAL 7) among the received RTP payloads"
    );

    // (e) RTP timestamps track each frame's real capture time rather than a
    // fixed 1/fps step (the bug fixed in slice 2.1d, see
    // `docs/host-libs-api-notes.md`'s webrtc section). The session is
    // configured for 30fps but the synthetic source runs at 15fps, so a
    // fixed-step stamp would advance the RTP clock at half real time; the
    // check is therefore "RTP clock span ~= wall-clock span" over >= 20
    // frames -- deliberately not a per-frame gap check, since a source may
    // legitimately deliver frames in bursts (then the RTP clock must follow
    // the burst too). No gap may go backward (wrapping subtraction, since
    // RTP timestamps wrap at u32::MAX).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while marker_timestamps.lock().await.len() < 20 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let timestamps = marker_timestamps.lock().await.clone();
    assert!(
        timestamps.len() >= 20,
        "expected >= 20 marker-bit RTP timestamps within 10s, got {}",
        timestamps.len()
    );
    for w in timestamps.windows(2) {
        let diff = w[1].1.wrapping_sub(w[0].1);
        assert!(
            diff < 90_000 * 10,
            "RTP timestamp went backward (or jumped implausibly far) between \
             consecutive frames: wrapping diff {diff}"
        );
    }
    let (first_at, first_ts) = timestamps[0];
    let (last_at, last_ts) = timestamps[timestamps.len() - 1];
    let wall_span = last_at.duration_since(first_at);
    let rtp_span = Duration::from_secs_f64(f64::from(last_ts.wrapping_sub(first_ts)) / 90_000.0);
    let tolerance = wall_span.mul_f64(0.3) + Duration::from_millis(100);
    let drift = rtp_span.abs_diff(wall_span);
    assert!(
        drift <= tolerance,
        "RTP clock should advance at wall-clock rate: rtp span {rtp_span:?} vs wall span \
         {wall_span:?} over {} frames (drift {drift:?} > tolerance {tolerance:?}); timestamps={timestamps:?}",
        timestamps.len()
    );

    // (d) the receiver sends a PLI; the host must notice within 3s
    // (SessionEvent::KeyframeRequested) and the pipeline's keyframe counter
    // must increase.
    let media_ssrc = *track
        .ssrcs()
        .await
        .first()
        .expect("remote track should expose an SSRC");
    let keyframes_before = stats.keyframes.load(Ordering::Relaxed);
    track
        .write_rtcp(vec![Box::new(PictureLossIndication {
            sender_ssrc: 0,
            media_ssrc,
        })])
        .await?;

    timeout(Duration::from_secs(3), keyframe_requested_rx.recv())
        .await
        .expect("SessionEvent::KeyframeRequested timed out after 3s")
        .expect("keyframe event channel closed");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while stats.keyframes.load(Ordering::Relaxed) <= keyframes_before
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        stats.keyframes.load(Ordering::Relaxed) > keyframes_before,
        "expected the pipeline's keyframe counter to increase after the PLI"
    );

    host_session.close().await;
    browser_pc.close().await?;
    forward_task.abort();

    Ok(())
}
