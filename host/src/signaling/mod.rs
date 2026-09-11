//! WebSocket signaling client: registers this host with the signaling
//! server (see `server/src/ws.rs` for the protocol this speaks to) and, for
//! the one active session, wires a `transport::PeerSession` and video
//! pipeline together and forwards SDP/ICE between them and the wire.
//!
//! Reconnecting to the signaling server after a drop is out of scope for
//! this slice: `run()` returns an error and the caller (the `serve` CLI
//! command) exits.

use std::sync::Arc;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use proto::signal::{Role, SignalMessage};
use webrtc::peer_connection::RTCPeerConnectionState;
use webrtc::runtime::Runtime;

use crate::capture::FrameSource;
use crate::encode::openh264::OpenH264Encoder;
use crate::encode::{Encoder, EncoderConfig};
use crate::pipeline::Pipeline;
use crate::transport::{PeerSession, SessionConfig, SessionEvent};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// What the signaling client needs to build a fresh `PeerSession` and video
/// pipeline for each joining peer.
pub struct HostContext {
    pub session: SessionConfig,
    pub bitrate_kbps: u32,
    /// Builds a fresh frame source for a new session. A closure (rather than
    /// a `FrameSource` directly baked in here) because a platform-specific
    /// `scap` source is only constructible behind `cfg(...)`, and this
    /// module stays platform-agnostic; `main.rs` supplies the closure.
    pub build_source: Box<dyn Fn() -> anyhow::Result<Box<dyn FrameSource>> + Send + Sync>,
    pub runtime: Arc<dyn Runtime>,
}

/// A registered signaling connection, holding the PIN the owner reads out to
/// pair a client.
pub struct SignalingClient {
    host_id: String,
    pin: String,
    write: SplitSink<WsStream, Message>,
    read: SplitStream<WsStream>,
}

impl SignalingClient {
    /// Connects to the signaling server, sends `Hello` + `HostRegister`, and
    /// waits for `Registered`.
    pub async fn connect(url: &str, name: &str) -> anyhow::Result<Self> {
        let (ws_stream, _response) = tokio_tungstenite::connect_async(url).await?;
        let (mut write, mut read) = ws_stream.split();

        send(
            &mut write,
            &SignalMessage::Hello {
                role: Role::Host,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .await?;
        send(
            &mut write,
            &SignalMessage::HostRegister {
                name: name.to_string(),
            },
        )
        .await?;

        let (host_id, pin) = match next_message(&mut read).await {
            Some(SignalMessage::Registered { host_id, pin }) => (host_id, pin),
            Some(SignalMessage::Error { message }) => {
                anyhow::bail!("signaling server rejected registration: {message}")
            }
            Some(other) => anyhow::bail!("unexpected reply while registering: {other:?}"),
            None => anyhow::bail!("signaling connection closed before registration completed"),
        };

        Ok(Self {
            host_id,
            pin,
            write,
            read,
        })
    }

    pub fn pin(&self) -> &str {
        &self.pin
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Drives the signaling connection: forwards SDP/ICE between the wire
    /// and a `PeerSession`, one session at a time. Returns once the
    /// connection is lost (an error -- no reconnect in this slice).
    pub async fn run(self, ctx: HostContext) -> anyhow::Result<()> {
        let SignalingClient {
            host_id,
            mut read,
            write,
            ..
        } = self;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let writer = tokio::spawn(async move {
            let mut write = write;
            while let Some(msg) = out_rx.recv().await {
                let Ok(json) = serde_json::to_string(&msg) else {
                    continue;
                };
                if write.send(Message::text(json)).await.is_err() {
                    break;
                }
            }
            let _ = write.close().await;
        });

        let (event_tx, mut event_rx) = mpsc::channel::<SessionEvent>(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(incoming) = incoming else { break };
                    let msg = match incoming {
                        Ok(Message::Text(text)) => match serde_json::from_str::<SignalMessage>(text.as_str()) {
                            Ok(msg) => msg,
                            Err(err) => {
                                tracing::warn!(?err, "invalid signal message from server");
                                continue;
                            }
                        },
                        Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                        Ok(Message::Close(_)) => break,
                        Ok(_) => continue,
                        Err(err) => {
                            tracing::warn!(?err, "signaling websocket read error");
                            break;
                        }
                    };

                    handle_signal_message(
                        msg,
                        &ctx,
                        &out_tx,
                        event_tx.clone(),
                        &mut active,
                        &mut current_session_id,
                    )
                    .await;
                }
                Some(event) = event_rx.recv() => {
                    handle_session_event(event, &out_tx, &mut active, &mut current_session_id).await;
                }
            }
        }

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
        drop(out_tx);
        let _ = writer.await;

        anyhow::bail!("signaling connection to server closed (host_id={host_id})")
    }
}

/// A running `PeerSession` plus the task feeding it encoded frames from the
/// video pipeline.
struct ActiveSession {
    peer: Arc<PeerSession>,
    forward_task: tokio::task::JoinHandle<()>,
}

impl ActiveSession {
    async fn shutdown(self) {
        self.peer.close().await;
        self.forward_task.abort();
    }
}

async fn handle_signal_message(
    msg: SignalMessage,
    ctx: &HostContext,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    event_tx: mpsc::Sender<SessionEvent>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
) {
    match msg {
        SignalMessage::PeerJoined { session_id } => {
            if let Some(old) = active.take() {
                tracing::info!("new PeerJoined while a session was active; closing the old one");
                old.shutdown().await;
            }
            *current_session_id = None;

            match start_session(ctx, event_tx).await {
                Ok((peer, forward_task)) => match peer.create_offer().await {
                    Ok(sdp) => {
                        let _ = out_tx.send(SignalMessage::Offer {
                            session_id: session_id.clone(),
                            sdp,
                        });
                        *current_session_id = Some(session_id);
                        *active = Some(ActiveSession { peer, forward_task });
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to create offer");
                        forward_task.abort();
                    }
                },
                Err(err) => {
                    tracing::warn!(?err, "failed to start session pipeline");
                }
            }
        }
        SignalMessage::Answer { session_id, sdp } => {
            if current_session_id.as_deref() == Some(session_id.as_str()) {
                if let Some(active) = active.as_ref() {
                    if let Err(err) = active.peer.set_answer(sdp).await {
                        tracing::warn!(?err, "failed to apply remote answer");
                    }
                }
            } else {
                tracing::debug!(%session_id, "answer for unknown/stale session, ignoring");
            }
        }
        SignalMessage::Ice {
            session_id,
            candidate,
        } => {
            if current_session_id.as_deref() == Some(session_id.as_str()) {
                if let Some(active) = active.as_ref() {
                    if let Err(err) = active.peer.add_remote_ice(candidate).await {
                        tracing::warn!(?err, "failed to add remote ice candidate");
                    }
                }
            } else {
                tracing::debug!(%session_id, "ice candidate for unknown/stale session, ignoring");
            }
        }
        SignalMessage::Bye { session_id } => {
            if current_session_id.as_deref() == Some(session_id.as_str()) {
                tracing::info!(%session_id, "session ended by peer");
                if let Some(old) = active.take() {
                    old.shutdown().await;
                }
                *current_session_id = None;
            }
        }
        SignalMessage::Error { message } => {
            tracing::warn!(%message, "signaling server reported an error");
        }
        other => {
            tracing::debug!(?other, "unexpected signal message, ignoring");
        }
    }
}

async fn handle_session_event(
    event: SessionEvent,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
) {
    match event {
        SessionEvent::LocalIce(candidate) => {
            if let Some(session_id) = current_session_id.clone() {
                let _ = out_tx.send(SignalMessage::Ice {
                    session_id,
                    candidate,
                });
            }
        }
        SessionEvent::ConnectionState(state) => {
            tracing::info!(?state, "peer connection state changed");
            if matches!(
                state,
                RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
                    | RTCPeerConnectionState::Disconnected
            ) {
                if let Some(old) = active.take() {
                    old.shutdown().await;
                }
                *current_session_id = None;
            }
        }
        SessionEvent::DataChannelMessage {
            label,
            data,
            is_string,
        } => {
            tracing::trace!(label, len = data.len(), is_string, "data channel message");
        }
        SessionEvent::KeyframeRequested => {
            tracing::debug!("keyframe requested by remote peer (PLI/FIR)");
        }
    }
}

/// Builds the video pipeline and the `PeerSession` for one joining peer, and
/// starts the task that feeds encoded frames from the pipeline into the
/// session's video track.
async fn start_session(
    ctx: &HostContext,
    events: mpsc::Sender<SessionEvent>,
) -> anyhow::Result<(Arc<PeerSession>, tokio::task::JoinHandle<()>)> {
    let source = (ctx.build_source)()?;
    let (width, height) = source.size();
    let fps = ctx.session.fps.max(1);

    let encoder_cfg = EncoderConfig {
        width,
        height,
        fps,
        bitrate_kbps: ctx.bitrate_kbps,
        keyframe_interval_frames: fps * 10,
    };
    let encoder: Box<dyn Encoder> = Box::new(OpenH264Encoder::new(encoder_cfg)?);
    let handle = Pipeline::start(source, encoder);
    let keyframe_flag = handle.keyframe_flag();

    let peer = PeerSession::new(ctx.session.clone(), events, Arc::clone(&ctx.runtime)).await?;

    // `PipelineHandle` implements `Drop` (joins its capture/encode threads),
    // so its `frames` receiver can't be moved out directly; instead it's
    // handed wholesale to this forwarding task, which relays frames into a
    // fresh channel that `start_video` takes by value.
    let (video_tx, video_rx) = mpsc::channel(4);
    let forward_task = tokio::spawn(async move {
        let mut handle = handle;
        while let Some(frame) = handle.frames.recv().await {
            if video_tx.send(frame).await.is_err() {
                break;
            }
        }
        handle.stop();
    });

    peer.start_video(video_rx, keyframe_flag);

    Ok((peer, forward_task))
}

async fn send(write: &mut SplitSink<WsStream, Message>, msg: &SignalMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    write.send(Message::text(json)).await?;
    Ok(())
}

async fn next_message(read: &mut SplitStream<WsStream>) -> Option<SignalMessage> {
    loop {
        match read.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str(text.as_str()) {
                Ok(msg) => return Some(msg),
                Err(err) => {
                    tracing::warn!(?err, "invalid json from signaling server");
                    return None;
                }
            },
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => return None,
            Some(Err(err)) => {
                tracing::warn!(?err, "signaling websocket read error");
                return None;
            }
        }
    }
}
