//! WebSocket signaling client: registers this host with the signaling
//! server (see `server/src/ws.rs` for the protocol this speaks to) and, for
//! the one active session, wires a `transport::PeerSession` and video
//! pipeline together and forwards SDP/ICE between them and the wire.
//!
//! Reconnecting to the signaling server after a drop is out of scope for
//! this slice: `run()` returns an error and the caller (the `serve` CLI
//! command) exits.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use proto::control::ControlMessage;
use proto::signal::{Role, SignalMessage};
use webrtc::peer_connection::RTCPeerConnectionState;
use webrtc::runtime::Runtime;

use crate::capture::FrameSource;
use crate::cursor::{self, CursorSource, CursorState};
use crate::encode::{build_encoder, EncoderConfig, EncoderKind, RateTarget};
use crate::input::{Injector, InputRouter};
use crate::pipeline::Pipeline;
use crate::transport::{PeerSession, SessionConfig, SessionEvent};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often the cursor watcher thread polls the platform for the current
/// system cursor shape (see `crate::cursor::watch`). 33ms is roughly 30Hz --
/// plenty for a cursor shape, which changes far less often than the pointer
/// moves.
const CURSOR_POLL_INTERVAL: Duration = Duration::from_millis(33);

/// What the signaling client needs to build a fresh `PeerSession` and video
/// pipeline for each joining peer.
pub struct HostContext {
    pub session: SessionConfig,
    pub bitrate_kbps: u32,
    /// Hard ceiling on encoder QP (0..=51); `None` leaves the encoder's own
    /// default. See `EncoderConfig::max_qp`.
    pub max_qp: Option<u8>,
    /// Which H.264 encoder backend to use. `None` means "auto" -- see
    /// `encode::build_encoder`.
    pub encoder: Option<EncoderKind>,
    /// Builds a fresh frame source for a new session. A closure (rather than
    /// a `FrameSource` directly baked in here) because a platform-specific
    /// `scap` source is only constructible behind `cfg(...)`, and this
    /// module stays platform-agnostic; `main.rs` supplies the closure.
    pub build_source: Box<dyn Fn() -> anyhow::Result<Box<dyn FrameSource>> + Send + Sync>,
    /// Builds a fresh input injector for a new session, the same way as
    /// `build_source` (and for the same reason: the real, `enigo`-backed
    /// implementation only exists behind `cfg(...)`). `main.rs` supplies
    /// either that or a `NoopInjector`-returning closure, depending on
    /// `serve --no-input` (see `docs/dev-run.md`).
    pub build_injector: Box<dyn Fn() -> anyhow::Result<Box<dyn Injector>> + Send + Sync>,
    /// Builds a fresh cursor-shape source for a new session, the same way as
    /// `build_source`/`build_injector` (and for the same reason: the real,
    /// platform-backed implementation only exists behind `cfg(...)`).
    /// `main.rs` supplies either that or `cursor::NoopCursorSource`.
    pub build_cursor_source: Box<dyn Fn() -> Box<dyn CursorSource> + Send + Sync>,
    pub runtime: Arc<dyn Runtime>,
}

/// A registered signaling connection, holding the PIN the owner reads out to
/// pair a client.
pub struct SignalingClient {
    host_id: String,
    pin: String,
    ice_servers: Vec<proto::signal::IceServer>,
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

        let (host_id, pin, ice_servers) = match next_message(&mut read).await {
            Some(SignalMessage::Registered {
                host_id,
                pin,
                ice_servers,
            }) => (host_id, pin, ice_servers),
            Some(SignalMessage::Error { message }) => {
                anyhow::bail!("signaling server rejected registration: {message}")
            }
            Some(other) => anyhow::bail!("unexpected reply while registering: {other:?}"),
            None => anyhow::bail!("signaling connection closed before registration completed"),
        };

        Ok(Self {
            host_id,
            pin,
            ice_servers,
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

    /// ICE servers the signaling server sent in `Registered`. `main.rs`
    /// merges these with any `--stun` CLI overrides before building the
    /// `SessionConfig` used for every session (see `docs/dev-run.md`).
    pub fn ice_servers(&self) -> &[proto::signal::IceServer] {
        &self.ice_servers
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
    /// Routes `input`/`pointer` data channel messages to the platform
    /// injector. Dropped at the end of `shutdown` (ordinary field drop),
    /// which -- per `InputRouter`'s own `Drop` impl -- releases any
    /// keys/buttons the session left held.
    router: InputRouter,
    /// The task that reads cursor-shape changes and forwards them over
    /// `control` (see `start_session`). It owns the session's
    /// `cursor::CursorWatcher` itself: `CursorWatcher` implements `Drop` to
    /// stop its background polling thread, which makes it impossible to
    /// partially move its `rx` field out of a separately-held watcher (Rust
    /// disallows moving fields out of any type that implements `Drop`) --
    /// so instead the watcher lives inside this task's future, and aborting
    /// the task (below) drops that future, which drops the watcher, which
    /// stops the thread.
    cursor_task: tokio::task::JoinHandle<()>,
}

impl ActiveSession {
    async fn shutdown(self) {
        self.peer.close().await;
        self.forward_task.abort();
        self.cursor_task.abort();
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
                Ok((peer, forward_task, router, cursor_task)) => match peer.create_offer().await {
                    Ok(sdp) => {
                        let _ = out_tx.send(SignalMessage::Offer {
                            session_id: session_id.clone(),
                            sdp,
                        });
                        *current_session_id = Some(session_id);
                        *active = Some(ActiveSession {
                            peer,
                            forward_task,
                            router,
                            cursor_task,
                        });
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to create offer");
                        forward_task.abort();
                        cursor_task.abort();
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
        } => match label.as_str() {
            "input" | "pointer" => {
                if !is_string {
                    tracing::warn!(label, "non-text message on input channel, ignoring");
                } else {
                    match serde_json::from_slice::<proto::input::InputMessage>(&data) {
                        Ok(msg) => {
                            if let Some(active) = active.as_ref() {
                                if active.router.sender().send(msg).is_err() {
                                    tracing::warn!(label, "input router is no longer running");
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(label, ?err, "failed to parse input message");
                        }
                    }
                }
            }
            "control" => {
                if !is_string {
                    tracing::warn!(label, "non-text message on control channel, ignoring");
                } else {
                    match serde_json::from_slice::<ControlMessage>(&data) {
                        Ok(ControlMessage::Ping { ts }) => {
                            if let Some(active) = active.as_ref() {
                                if let Err(err) =
                                    active.peer.send_control(&ControlMessage::Pong { ts }).await
                                {
                                    tracing::warn!(?err, "failed to send pong");
                                }
                            }
                        }
                        Ok(other) => {
                            tracing::trace!(label, ?other, "unexpected control message, ignoring");
                        }
                        Err(err) => {
                            tracing::warn!(label, ?err, "failed to parse control message");
                        }
                    }
                }
            }
            _ => {
                tracing::trace!(label, len = data.len(), is_string, "data channel message");
            }
        },
        SessionEvent::KeyframeRequested => {
            tracing::debug!("keyframe requested by remote peer (PLI/FIR)");
        }
        SessionEvent::Remb { bitrate_bps } => {
            tracing::debug!(bitrate_bps, "remb from peer");
        }
        SessionEvent::ReceiverReport {
            fraction_lost, rtt, ..
        } => {
            tracing::debug!(fraction_lost, ?rtt, "receiver report from peer");
        }
    }
}

/// Builds the video pipeline and the `PeerSession` for one joining peer, and
/// starts the task that feeds encoded frames from the pipeline into the
/// session's video track.
#[allow(clippy::type_complexity)]
async fn start_session(
    ctx: &HostContext,
    events: mpsc::Sender<SessionEvent>,
) -> anyhow::Result<(
    Arc<PeerSession>,
    tokio::task::JoinHandle<()>,
    InputRouter,
    tokio::task::JoinHandle<()>,
)> {
    let source = (ctx.build_source)()?;
    let (width, height) = source.size();
    let fps = ctx.session.fps.max(1);

    let encoder_cfg = EncoderConfig {
        width,
        height,
        fps,
        bitrate_kbps: ctx.bitrate_kbps,
        keyframe_interval_frames: fps * 10,
        max_qp: ctx.max_qp,
    };
    let (encoder, encoder_kind) = build_encoder(ctx.encoder, encoder_cfg)?;
    tracing::info!(encoder = encoder_kind.name(), "starting session pipeline");
    let handle = Pipeline::start(
        source,
        encoder,
        RateTarget {
            bitrate_kbps: ctx.bitrate_kbps,
            fps,
        },
    );
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

    let injector = (ctx.build_injector)()?;
    let router = InputRouter::new(injector);

    // The watcher's background thread is polled by this task, not directly
    // by `ActiveSession` -- see `ActiveSession::cursor_task`'s doc comment
    // for why (`CursorWatcher` implements `Drop`, so its `rx` field can't be
    // moved out to live separately from the rest of the struct). The first
    // state a fresh watcher reads always counts as a "change" (see
    // `cursor::watch`'s doc comment), so this newly connected client gets
    // the host's current cursor shape right away.
    let cursor_source = (ctx.build_cursor_source)();
    let mut cursor_watcher = cursor::watch(cursor_source, CURSOR_POLL_INTERVAL);
    let cursor_peer = Arc::clone(&peer);
    let cursor_task = tokio::spawn(async move {
        while let Some(state) = cursor_watcher.rx.recv().await {
            let msg = cursor_state_to_control(state);
            if let Err(err) = cursor_peer.send_control(&msg).await {
                tracing::warn!(?err, "failed to send cursor control message");
            }
        }
    });

    Ok((peer, forward_task, router, cursor_task))
}

/// Converts a cursor-shape change into the wire message `send_control`
/// sends, base64-encoding the raw RGBA bytes (see
/// `proto::control::ControlMessage::CursorShape`'s doc comment).
fn cursor_state_to_control(state: CursorState) -> ControlMessage {
    match state {
        CursorState::Hidden => ControlMessage::CursorHidden,
        CursorState::Shape(image) => ControlMessage::CursorShape {
            width: image.width,
            height: image.height,
            hotspot_x: image.hotspot_x,
            hotspot_y: image.hotspot_y,
            scale: image.scale,
            rgba: BASE64_STANDARD.encode(image.rgba),
        },
    }
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
