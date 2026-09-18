//! WebSocket signaling client: registers this host with the signaling
//! server (see `server/src/ws.rs` for the protocol this speaks to) and, for
//! the one active session, wires a `transport::PeerSession` and video
//! pipeline together and forwards SDP/ICE between them and the wire.
//!
//! Reconnecting after a drop is handled one layer up, by `crate::app::run_agent`,
//! which loops `SignalingClient::connect` + `run()` with backoff (slice
//! 2.6a): `run()` itself still just returns an `Err` the moment the
//! connection is lost, same as before.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use proto::control::{ControlMessage, DisplayEntry};
use proto::signal::{Role, SignalMessage};
use webrtc::peer_connection::RTCPeerConnectionState;
use webrtc::runtime::Runtime;

use crate::adapt::{AdaptConfig, Controller, Feedback};
use crate::capture::{self, DisplayInfo, FrameSource};
use crate::clipboard::{self, ClipboardBackend, ClipboardSync};
use crate::cursor::{self, CursorSource, CursorState};
use crate::encode::{build_encoder, EncodedFrame, EncoderConfig, EncoderKind, RateTarget};
use crate::input::{Injector, InputRouter, NoopInjector};
use crate::pipeline::Pipeline;
use crate::transport::{PeerSession, SessionConfig, SessionEvent};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often the cursor watcher thread polls the platform for the current
/// system cursor shape (see `crate::cursor::watch`). 33ms is roughly 30Hz --
/// plenty for a cursor shape, which changes far less often than the pointer
/// moves.
const CURSOR_POLL_INTERVAL: Duration = Duration::from_millis(33);

/// See `HostContext::build_clipboard`'s doc comment. Factored out (clippy
/// `type_complexity`): the `Option<Box<dyn Fn() -> ...>>` nesting is one
/// level deeper than `build_injector`/`build_cursor_source`'s bare closures.
pub type BuildClipboard = Box<dyn Fn() -> anyhow::Result<Box<dyn ClipboardBackend>> + Send + Sync>;

/// What the signaling client needs to build a fresh `PeerSession` and video
/// pipeline for each joining peer.
pub struct HostContext {
    /// Template session config, built once by `app::build_host_context` and
    /// reused for every reconnect. `ice_servers` here holds only the extra
    /// `--stun` servers from the CLI -- the signaling server's own
    /// per-registration credentials (fresh on every `Registered`) are merged
    /// in per-session by `session_ice_servers`, not baked into this template.
    pub session: SessionConfig,
    pub bitrate_kbps: u32,
    /// Whether the bitrate/fps adaptation controller (slice 2.3) runs for
    /// each session. `false` (`serve --no-adapt`) keeps the encoder pinned
    /// to `--bitrate`/`--fps` for the whole session -- see `docs/dev-run.md`.
    pub adapt: bool,
    /// Hard ceiling on encoder QP (0..=51); `None` leaves the encoder's own
    /// default. See `EncoderConfig::max_qp`.
    pub max_qp: Option<u8>,
    /// Which H.264 encoder backend to use. `None` means "auto" -- see
    /// `encode::build_encoder`.
    pub encoder: Option<EncoderKind>,
    /// Builds a fresh frame source for the display `id` (one of
    /// `list_displays`' entries) for a new pipeline. A closure (rather than
    /// a `FrameSource` directly baked in here) because a platform-specific
    /// `scap` source is only constructible behind `cfg(...)`, and this
    /// module stays platform-agnostic; `main.rs` supplies the closure.
    /// Called once per pipeline, i.e. again on every `SelectDisplay`
    /// (slice 2.4), not just once per session.
    pub build_source: Box<dyn Fn(u32) -> anyhow::Result<Box<dyn FrameSource>> + Send + Sync>,
    /// Lists the displays capturable right now, for the initial
    /// `ControlMessage::Displays` announcement and to resolve
    /// `ControlMessage::SelectDisplay` ids (slice 2.4). `main.rs` supplies
    /// either `capture::list_displays` or the synthetic backend's fixed
    /// two-display list.
    pub list_displays: Box<dyn Fn() -> anyhow::Result<Vec<DisplayInfo>> + Send + Sync>,
    /// The display to capture at session start, from CLI `--display`.
    /// `None` means "use `capture::default_display` over `list_displays`'s
    /// result".
    pub display: Option<u32>,
    /// Builds a fresh input injector for a new session, the same way as
    /// `build_source` (and for the same reason: the real, `enigo`-backed
    /// implementation only exists behind `cfg(...)`). `main.rs` supplies the
    /// real, platform-backed builder, or one that always returns `Err` for
    /// `serve --no-input` (see `docs/dev-run.md`). Either way, `Err` here
    /// doesn't fail the session: `start_session` falls back to a
    /// `NoopInjector` and tells the client over `ControlMessage::InputStatus`
    /// (slice 2.5a, debt D26).
    pub build_injector: Box<dyn Fn() -> anyhow::Result<Box<dyn Injector>> + Send + Sync>,
    /// Builds a fresh cursor-shape source for a new session, the same way as
    /// `build_source`/`build_injector` (and for the same reason: the real,
    /// platform-backed implementation only exists behind `cfg(...)`).
    /// `main.rs` supplies either that or `cursor::NoopCursorSource`.
    pub build_cursor_source: Box<dyn Fn() -> Box<dyn CursorSource> + Send + Sync>,
    /// Builds a fresh clipboard backend for a new session, the same pattern
    /// as `build_injector`/`build_cursor_source` (slice 2.5b). `None` means
    /// clipboard sync is disabled for every session (`serve --no-clipboard`);
    /// `main.rs` supplies either that or the real, platform-backed builder.
    /// Unlike `build_injector`, a `Some` closure that returns `Err` (e.g. no
    /// platform clipboard available) doesn't disable input or fail the
    /// session -- it just means this one session runs without clipboard
    /// sync (same lesson as debt D26: a missing optional capability must
    /// never take the session down).
    pub build_clipboard: Option<BuildClipboard>,
    pub runtime: Arc<dyn Runtime>,
}

/// One host agent's current phase, as reported through the `watch` channel
/// `app::run_agent` passes to `SignalingClient::run` (slice 2.6a). Consumed
/// today by `serve`'s PIN-printing task; the reason this lives as a proper
/// type (not just a log line) is a future tray UI (slice 2.6c), which needs
/// to show live status rather than parse logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Attempting to connect/register with the signaling server.
    Connecting,
    /// Registered with the server, `pin` is current, but no session is
    /// active right now.
    Registered { pin: String },
    /// A session's `RTCPeerConnection` has reached `Connected`.
    InSession { pin: String },
    /// The connection was lost (or never established); `error` is the
    /// failure and `retry_in` how long before the next attempt.
    Reconnecting { error: String, retry_in: Duration },
}

/// How often `SignalingClient::run` pings the signaling server, and how long
/// it tolerates silence (no incoming frame of any kind) before deciding the
/// connection is dead -- see `run`'s doc comment for why a plain
/// `read.next()` alone isn't enough. `app::run_agent` owns the value and
/// passes it through on every reconnect; `Default` is 20s/45s.
#[derive(Debug, Clone, Copy)]
pub struct Keepalive {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for Keepalive {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(20),
            timeout: Duration::from_secs(45),
        }
    }
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
    /// connection is lost (an error) -- reconnecting is `app::run_agent`'s
    /// job, one layer up. `ctx` is a shared reference (see that function's
    /// doc comment for why) so it survives every reconnect; `status` is
    /// updated to `InSession`/`Registered` as the one active session's
    /// `RTCPeerConnection` connects/disconnects.
    ///
    /// `keepalive` guards against a *quiet* drop -- laptop sleep, a NAT
    /// mapping expiring, a half-open TCP connection -- none of which fail a
    /// plain `read.next()` on their own, so without this a dead connection
    /// would never surface as an `Err` and `run_agent` would never
    /// reconnect. A background task pings the server every
    /// `keepalive.interval`; the main loop tracks the time of the last
    /// *any* incoming frame (text, ping, pong -- traffic in either direction
    /// proves the socket is alive) and bails out once that's older than
    /// `keepalive.timeout`.
    pub async fn run(
        self,
        ctx: &HostContext,
        status: &watch::Sender<AgentStatus>,
        keepalive: Keepalive,
    ) -> anyhow::Result<()> {
        let SignalingClient {
            host_id,
            pin,
            ice_servers: registered_ice_servers,
            mut read,
            write,
        } = self;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let writer = tokio::spawn(async move {
            let mut write = write;
            let mut ping_ticker = tokio::time::interval(keepalive.interval);
            ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; that's fine here (an extra,
            // harmless ping right after connecting), unlike `adapt`'s ticker
            // which specifically needs to skip it.
            loop {
                tokio::select! {
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        let Ok(json) = serde_json::to_string(&msg) else {
                            continue;
                        };
                        if write.send(Message::text(json)).await.is_err() {
                            break;
                        }
                    }
                    _ = ping_ticker.tick() => {
                        if write.send(Message::Ping(bytes::Bytes::new())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = write.close().await;
        });

        let (event_tx, mut event_rx) = mpsc::channel::<SessionEvent>(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;

        let mut last_incoming = Instant::now();
        let mut timeout_checker = tokio::time::interval(keepalive.interval);
        timeout_checker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut timed_out = false;

        loop {
            tokio::select! {
                incoming = read.next() => {
                    let Some(incoming) = incoming else { break };
                    if incoming.is_ok() {
                        last_incoming = Instant::now();
                    }
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
                        ctx,
                        &registered_ice_servers,
                        &pin,
                        status,
                        &out_tx,
                        event_tx.clone(),
                        &mut active,
                        &mut current_session_id,
                    )
                    .await;
                }
                Some(event) = event_rx.recv() => {
                    handle_session_event(event, ctx, &pin, status, &out_tx, &mut active, &mut current_session_id).await;
                }
                _ = timeout_checker.tick() => {
                    let silence = last_incoming.elapsed();
                    if silence >= keepalive.timeout {
                        tracing::warn!(?silence, "signaling connection timed out, no traffic");
                        timed_out = true;
                        break;
                    }
                }
            }
        }

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
        drop(out_tx);
        // On a dead (half-open) connection the writer's close handshake can
        // block on the socket; don't let that stall the reconnect.
        let writer_abort = writer.abort_handle();
        if tokio::time::timeout(Duration::from_secs(2), writer)
            .await
            .is_err()
        {
            writer_abort.abort();
        }

        if timed_out {
            anyhow::bail!(
                "signaling connection timed out (no traffic for {:?})",
                keepalive.timeout
            );
        }
        anyhow::bail!("signaling connection to server closed (host_id={host_id})")
    }
}

/// The video pipeline (capture + encode) for one displayed screen, plus the
/// task forwarding its encoded frames into the session's video track and
/// (when `HostContext::adapt`) the adaptation controller task. Rebuilt from
/// scratch by `switch_display` on every `ControlMessage::SelectDisplay` --
/// the `PeerConnection`/`PeerSession` are not touched, only this piece (see
/// slice 2.4's plan: switching displays does not recreate the
/// `PeerConnection`).
struct VideoPipeline {
    display: DisplayInfo,
    /// Tells the forward task (spawned detached in `start`: dropping a tokio
    /// `JoinHandle` does not cancel the task) to stop reading frames and hand
    /// the pipeline off to `spawn_blocking` -- see `start`.
    stop_tx: Option<oneshot::Sender<()>>,
    adapt_tx: Option<mpsc::UnboundedSender<Feedback>>,
    adapt_task: Option<JoinHandle<()>>,
}

impl VideoPipeline {
    /// Builds the encoder/pipeline for `display` and starts forwarding its
    /// encoded frames into `video_tx` (the channel `PeerSession::start_video`
    /// reads from). `keyframe_flag` is the *session's* shared flag (set by
    /// `start_session`, not created here): `PeerSession::start_video` is
    /// called once per session, so the flag it was given must keep being the
    /// one this pipeline's encode thread honors even after a display switch.
    async fn start(
        ctx: &HostContext,
        display: DisplayInfo,
        peer: Arc<PeerSession>,
        video_tx: mpsc::Sender<EncodedFrame>,
        keyframe_flag: Arc<AtomicBool>,
    ) -> anyhow::Result<VideoPipeline> {
        let source = (ctx.build_source)(display.id)?;
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
        let display_id = display.id;
        let display_title = display.title.clone();
        tracing::info!(
            display_id,
            title = %display_title,
            width,
            height,
            encoder = encoder_kind.name(),
            "starting video pipeline"
        );
        let handle = Pipeline::start_with_keyframe_flag(
            source,
            encoder,
            RateTarget {
                bitrate_kbps: ctx.bitrate_kbps,
                fps,
            },
            keyframe_flag,
        );
        let rate_control = handle.rate_control();

        // `Some` only when `HostContext::adapt` is set (`serve` without
        // `--no-adapt`, see `docs/dev-run.md`); `adapt_tx` is cloned into the
        // forward task below so it can report every encoded frame, and the
        // original is handed back for `handle_session_event` to feed
        // REMB/loss into (see `ActiveSession::video`/`VideoPipeline::adapt_tx`).
        let (adapt_tx, adapt_rx) = if ctx.adapt {
            let (tx, rx) = mpsc::unbounded_channel::<Feedback>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let frame_feedback_tx = adapt_tx.clone();

        let pipeline_stats = Arc::clone(&handle.stats);
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut handle = handle;
            loop {
                tokio::select! {
                    frame = handle.frames.recv() => {
                        let Some(frame) = frame else { break };
                        if let Some(tx) = &frame_feedback_tx {
                            let _ = tx.send(Feedback::Frame {
                                bytes: frame.data.len(),
                            });
                        }
                        if video_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    _ = &mut stop_rx => {
                        break;
                    }
                }
            }
            // `PipelineHandle::stop` joins the capture/encode threads, and
            // the capture thread may be blocked inside a live source's
            // blocking `get_next_frame()` call -- run it off the tokio
            // worker thread rather than stalling it, and don't wait for it
            // here: this task's job is done once it stops reading frames.
            tokio::task::spawn_blocking(move || handle.stop());
        });

        let adapt_task = adapt_rx.map(|rx| {
            let adapt_peer = Arc::clone(&peer);
            let adapt_cfg = AdaptConfig {
                max_bitrate_kbps: ctx.bitrate_kbps,
                min_bitrate_kbps: crate::adapt::MIN_BITRATE_KBPS,
                max_fps: fps,
                min_fps: crate::adapt::MIN_FPS,
                width,
                height,
            };
            tokio::spawn(run_adapt_task(
                adapt_cfg,
                rx,
                rate_control,
                Arc::clone(&pipeline_stats),
                adapt_peer,
            ))
        });

        Ok(VideoPipeline {
            display,
            stop_tx: Some(stop_tx),
            adapt_tx,
            adapt_task,
        })
    }

    /// Stops the forward task (which in turn stops the pipeline's
    /// capture/encode threads, see `start`'s doc comment) and the adapt
    /// task. The forward task exits on its own once `stop_tx` fires and
    /// hands the pipeline off to `spawn_blocking`.
    fn stop(self) {
        if let Some(tx) = self.stop_tx {
            let _ = tx.send(());
        }
        if let Some(task) = self.adapt_task {
            task.abort();
        }
    }
}

/// A running `PeerSession` plus the task feeding it encoded frames from the
/// video pipeline.
struct ActiveSession {
    peer: Arc<PeerSession>,
    video: VideoPipeline,
    /// Channel `VideoPipeline::forward_task` writes into and
    /// `PeerSession::start_video` reads from; kept here so `switch_display`
    /// can start a replacement pipeline feeding the same channel.
    video_tx: mpsc::Sender<EncodedFrame>,
    /// The session-wide keyframe-request flag handed to `PeerSession::start_video`
    /// once at session start; every `VideoPipeline` (including ones built by
    /// `switch_display`) shares it (see `Pipeline::start_with_keyframe_flag`'s
    /// doc comment).
    keyframe_flag: Arc<AtomicBool>,
    /// The most recently listed set of displays, sent to the client in
    /// `ControlMessage::Displays` (see `displays_message`).
    displays: Vec<DisplayInfo>,
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
    /// Whether `start_session` obtained a real input injector for this
    /// session (slice 2.5a, debt D26). `false` means the session is
    /// view-only -- `build_injector` failed (e.g. missing the macOS
    /// Accessibility permission, or `serve --no-input`) and a `NoopInjector`
    /// was substituted so the session still streams video instead of dying.
    /// Sent to the client as `ControlMessage::InputStatus` once the
    /// `control` channel opens.
    input_available: bool,
    /// Human-readable reason for `input_available == false` (the error from
    /// `build_injector`), or `None` when input is available.
    input_reason: Option<String>,
    /// Clipboard sync state for this session (slice 2.5b), shared between
    /// `clipboard_task`'s watcher thread and
    /// `handle_session_event`'s handling of `InputMessage::ClipboardText` on
    /// the `input` channel. `None` when clipboard sync is disabled
    /// (`HostContext::build_clipboard` is `None`) or unavailable for this
    /// session (the builder returned `Err`) -- the session runs without it
    /// either way, same as a missing input injector (debt D26).
    clipboard: Option<Arc<Mutex<ClipboardSync>>>,
    /// The task that reads clipboard text changes and forwards them over
    /// `control`, mirroring `cursor_task` -- see that field's doc comment
    /// for why the watcher lives inside the task's future rather than being
    /// held separately. `None` alongside `clipboard: None`.
    clipboard_task: Option<tokio::task::JoinHandle<()>>,
}

impl ActiveSession {
    async fn shutdown(self) {
        self.peer.close().await;
        self.video.stop();
        self.cursor_task.abort();
        if let Some(task) = self.clipboard_task {
            task.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_signal_message(
    msg: SignalMessage,
    ctx: &HostContext,
    registered_ice_servers: &[proto::signal::IceServer],
    pin: &str,
    status: &watch::Sender<AgentStatus>,
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
                let _ = status.send(AgentStatus::Registered {
                    pin: pin.to_string(),
                });
            }
            *current_session_id = None;

            match start_session(ctx, registered_ice_servers, event_tx).await {
                Ok(parts) => match parts.peer.create_offer().await {
                    Ok(sdp) => {
                        let _ = out_tx.send(SignalMessage::Offer {
                            session_id: session_id.clone(),
                            sdp,
                        });
                        *current_session_id = Some(session_id);
                        *active = Some(ActiveSession {
                            peer: parts.peer,
                            video: parts.video,
                            video_tx: parts.video_tx,
                            keyframe_flag: parts.keyframe_flag,
                            displays: parts.displays,
                            router: parts.router,
                            cursor_task: parts.cursor_task,
                            input_available: parts.input_available,
                            input_reason: parts.input_reason,
                            clipboard: parts.clipboard,
                            clipboard_task: parts.clipboard_task,
                        });
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to create offer");
                        parts.video.stop();
                        parts.cursor_task.abort();
                        if let Some(task) = parts.clipboard_task {
                            task.abort();
                        }
                        let _ = out_tx.send(SignalMessage::Bye { session_id });
                    }
                },
                Err(err) => {
                    tracing::warn!(?err, "failed to start session pipeline");
                    let _ = out_tx.send(SignalMessage::Bye { session_id });
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
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
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

#[allow(clippy::too_many_arguments)]
async fn handle_session_event(
    event: SessionEvent,
    ctx: &HostContext,
    pin: &str,
    status: &watch::Sender<AgentStatus>,
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
            if state == RTCPeerConnectionState::Connected && active.is_some() {
                let _ = status.send(AgentStatus::InSession {
                    pin: pin.to_string(),
                });
            }
            if matches!(
                state,
                RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Closed
                    | RTCPeerConnectionState::Disconnected
            ) {
                if let Some(old) = active.take() {
                    old.shutdown().await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
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
                        // `ClipboardText` is never routed through
                        // `InputRouter` like every other input message:
                        // `handle_session_event` (this function) processes
                        // one channel event at a time, so applying it here,
                        // synchronously, guarantees the host's clipboard is
                        // updated before the very next event (e.g. a Cmd+V
                        // `Key`) reaches the router (slice 2.5b). Never
                        // logged with its text -- length only (privacy).
                        Ok(proto::input::InputMessage::ClipboardText { text }) => {
                            if label == "pointer" {
                                tracing::warn!(
                                    label,
                                    len = text.len(),
                                    "clipboard_text on the unordered pointer channel, ignoring"
                                );
                            } else if let Some(active) = active.as_ref() {
                                apply_remote_clipboard_text(active.clipboard.as_ref(), &text);
                            }
                        }
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
                        Ok(ControlMessage::SelectDisplay { id }) => {
                            if let Some(active) = active.as_mut() {
                                switch_display(ctx, active, id).await;
                            }
                        }
                        Ok(ControlMessage::ClipboardText { text }) => {
                            // Wrong direction: `ClipboardText` is host ->
                            // client only on `control` (see
                            // `proto::control::ControlMessage::ClipboardText`'s
                            // doc comment; the client -> host direction is
                            // `proto::input::InputMessage::ClipboardText` on
                            // `input`, handled above). Named explicitly
                            // (rather than falling into the catch-all below)
                            // so its text never reaches a log line, even at
                            // `trace` (privacy).
                            tracing::warn!(
                                label,
                                len = text.len(),
                                "clipboard_text on control channel, ignoring (wrong direction)"
                            );
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
            if let Some(active) = active.as_ref() {
                if let Some(adapt_tx) = &active.video.adapt_tx {
                    let _ = adapt_tx.send(Feedback::Remb { bitrate_bps });
                }
            }
        }
        SessionEvent::ReceiverReport {
            fraction_lost, rtt, ..
        } => {
            tracing::debug!(fraction_lost, ?rtt, "receiver report from peer");
            if let Some(active) = active.as_ref() {
                if let Some(adapt_tx) = &active.video.adapt_tx {
                    let _ = adapt_tx.send(Feedback::Loss {
                        fraction: fraction_lost,
                    });
                }
            }
        }
        SessionEvent::DataChannelOpen { label } => match (label.as_str(), active.as_ref()) {
            ("control", Some(active)) => {
                let msg = displays_message(&active.displays, active.video.display.id);
                match active.peer.send_control(&msg).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            "control channel not open yet, dropped initial displays list"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to send initial displays list");
                    }
                }
                let status = ControlMessage::InputStatus {
                    available: active.input_available,
                    reason: active.input_reason.clone(),
                };
                match active.peer.send_control(&status).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            "control channel not open yet, dropped initial input status"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to send initial input status");
                    }
                }
            }
            _ => {
                tracing::trace!(label, "data channel open");
            }
        },
    }
}

/// Applies an incoming `InputMessage::ClipboardText` (client -> host) to the
/// session's clipboard, if clipboard sync is available for it -- see the
/// call site in `handle_session_event` for why this happens synchronously
/// rather than through `InputRouter`. `clipboard` is `None` when
/// `HostContext::build_clipboard` was `None` (`serve --no-clipboard`) or the
/// builder failed for this session; `text` is never logged (privacy).
fn apply_remote_clipboard_text(clipboard: Option<&Arc<Mutex<ClipboardSync>>>, text: &str) {
    match clipboard {
        Some(sync) => {
            let mut sync = sync.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            sync.apply_remote(text);
        }
        None => {
            tracing::debug!(
                len = text.len(),
                "clipboard sync unavailable for this session, ignoring incoming clipboard text"
            );
        }
    }
}

/// Builds the `ControlMessage::Displays` announcement for `displays`/`current`.
fn displays_message(displays: &[DisplayInfo], current: u32) -> ControlMessage {
    ControlMessage::Displays {
        displays: displays
            .iter()
            .map(|d| DisplayEntry {
                id: d.id,
                title: d.title.clone(),
                width: d.width,
                height: d.height,
                primary: d.primary,
            })
            .collect(),
        current,
    }
}

/// Handles `ControlMessage::SelectDisplay { id }`: builds a fresh
/// `VideoPipeline` for the requested display *before* stopping the current
/// one, so a failure (unknown id, or the new pipeline fails to start) leaves
/// the session streaming exactly what it was streaming before -- no black
/// gap, no dropped session. Always re-announces the (possibly refreshed)
/// display list afterwards, since the client is waiting on a reply either
/// way.
async fn switch_display(ctx: &HostContext, active: &mut ActiveSession, id: u32) {
    let list = match (ctx.list_displays)() {
        Ok(list) => list,
        Err(err) => {
            // Still answer: the client resets its picker from `Displays`.
            tracing::warn!(?err, "failed to list displays for switch");
            let msg = displays_message(&active.displays, active.video.display.id);
            let _ = active.peer.send_control(&msg).await;
            return;
        }
    };
    let Some(display) = list.iter().find(|d| d.id == id).cloned() else {
        tracing::warn!(id, "select_display: unknown display id");
        let msg = displays_message(&active.displays, active.video.display.id);
        let _ = active.peer.send_control(&msg).await;
        return;
    };
    if id == active.video.display.id {
        active.displays = list;
        let msg = displays_message(&active.displays, active.video.display.id);
        let _ = active.peer.send_control(&msg).await;
        return;
    }
    match VideoPipeline::start(
        ctx,
        display,
        Arc::clone(&active.peer),
        active.video_tx.clone(),
        Arc::clone(&active.keyframe_flag),
    )
    .await
    {
        Ok(new) => {
            let from = active.video.display.id;
            let old = std::mem::replace(&mut active.video, new);
            old.stop();
            active
                .router
                .set_capture_rect(crate::input::CaptureRect::from(&active.video.display));
            active.displays = list;
            tracing::info!(from, to = active.video.display.id, "switched display");
            let msg = displays_message(&active.displays, active.video.display.id);
            let _ = active.peer.send_control(&msg).await;
        }
        Err(err) => {
            tracing::warn!(
                ?err,
                id,
                "failed to start pipeline for new display, keeping current"
            );
            let msg = displays_message(&active.displays, active.video.display.id);
            let _ = active.peer.send_control(&msg).await;
        }
    }
}

/// What `start_session` hands back to `handle_signal_message` for one
/// joining peer: the `PeerSession` plus every background task/resource that
/// makes up the running session (see `ActiveSession`, which this gets moved
/// into once the offer is sent).
struct SessionParts {
    peer: Arc<PeerSession>,
    video: VideoPipeline,
    video_tx: mpsc::Sender<EncodedFrame>,
    keyframe_flag: Arc<AtomicBool>,
    displays: Vec<DisplayInfo>,
    router: InputRouter,
    cursor_task: tokio::task::JoinHandle<()>,
    /// See `ActiveSession::input_available`.
    input_available: bool,
    /// See `ActiveSession::input_reason`.
    input_reason: Option<String>,
    /// See `ActiveSession::clipboard`.
    clipboard: Option<Arc<Mutex<ClipboardSync>>>,
    /// See `ActiveSession::clipboard_task`.
    clipboard_task: Option<tokio::task::JoinHandle<()>>,
}

/// Combines one registration's ICE credentials (from the signaling server's
/// `Registered` reply) with the extra `--stun` servers baked into
/// `HostContext::session` at startup, in the order the client has always
/// used: server-provided first, then CLI overrides. A free function (not
/// inlined at its one call site) so slice 2.6b -- which swaps `registered`
/// for the joining peer's own credentials from `PeerJoined` instead of the
/// host's registration -- has exactly one place to change.
fn session_ice_servers(
    registered: &[proto::signal::IceServer],
    extra: &[proto::signal::IceServer],
) -> Vec<proto::signal::IceServer> {
    registered
        .iter()
        .cloned()
        .chain(extra.iter().cloned())
        .collect()
}

/// Builds the video pipeline and the `PeerSession` for one joining peer, and
/// starts the task that feeds encoded frames from the pipeline into the
/// session's video track. `registered_ice_servers` are this registration's
/// own ICE credentials from the signaling server's `Registered` reply (see
/// `session_ice_servers`).
async fn start_session(
    ctx: &HostContext,
    registered_ice_servers: &[proto::signal::IceServer],
    events: mpsc::Sender<SessionEvent>,
) -> anyhow::Result<SessionParts> {
    let displays = (ctx.list_displays)()?;
    let start_display = match ctx.display {
        Some(id) => displays
            .iter()
            .find(|d| d.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("display {id} not found"))?,
        None => capture::default_display(&displays)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no capturable display found"))?,
    };

    let keyframe_flag = Arc::new(AtomicBool::new(false));
    let (video_tx, video_rx) = mpsc::channel(4);

    let session_config = SessionConfig {
        ice_servers: session_ice_servers(registered_ice_servers, &ctx.session.ice_servers),
        ..ctx.session.clone()
    };
    let peer = PeerSession::new(session_config, events, Arc::clone(&ctx.runtime)).await?;
    // Called once per session: every `VideoPipeline` (including ones built
    // later by `switch_display`) feeds frames into the same `video_tx`/
    // `video_rx` pair and shares `keyframe_flag` -- see their doc comments.
    peer.start_video(video_rx, Arc::clone(&keyframe_flag));

    let video = VideoPipeline::start(
        ctx,
        start_display,
        Arc::clone(&peer),
        video_tx.clone(),
        Arc::clone(&keyframe_flag),
    )
    .await?;

    // A failed injector (e.g. missing the macOS Accessibility permission, or
    // `serve --no-input`) must not take the whole session down -- fall back
    // to a `NoopInjector` and tell the client it's view-only (slice 2.5a,
    // debt D26) instead of erroring out of `start_session`.
    let (injector, input_available, input_reason): (Box<dyn Injector>, bool, Option<String>) =
        match (ctx.build_injector)() {
            Ok(injector) => (injector, true, None),
            Err(err) => {
                tracing::warn!(error = %err, "input unavailable, session is view-only");
                (
                    Box::new(NoopInjector::new()) as Box<dyn Injector>,
                    false,
                    Some(err.to_string()),
                )
            }
        };
    let router = InputRouter::new(injector);
    router.set_capture_rect(crate::input::CaptureRect::from(&video.display));

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

    // A failed/absent clipboard builder must not take the session down
    // either -- same lesson as the injector fallback above (debt D26): the
    // session just runs without clipboard sync.
    let (clipboard, clipboard_task) = match &ctx.build_clipboard {
        Some(build_clipboard) => match build_clipboard() {
            Ok(backend) => {
                let sync = Arc::new(Mutex::new(ClipboardSync::new(backend)));
                let mut clipboard_watcher =
                    clipboard::watch(Arc::clone(&sync), clipboard::CLIPBOARD_POLL_INTERVAL);
                let clipboard_peer = Arc::clone(&peer);
                let task = tokio::spawn(async move {
                    while let Some(text) = clipboard_watcher.rx.recv().await {
                        let msg = ControlMessage::ClipboardText { text };
                        if let Err(err) = clipboard_peer.send_control(&msg).await {
                            tracing::warn!(?err, "failed to send clipboard control message");
                        }
                    }
                });
                (Some(sync), Some(task))
            }
            Err(err) => {
                tracing::warn!(error = %err, "clipboard sync unavailable, session runs without it");
                (None, None)
            }
        },
        None => (None, None),
    };

    Ok(SessionParts {
        peer,
        video,
        video_tx,
        keyframe_flag,
        displays,
        router,
        cursor_task,
        input_available,
        input_reason,
        clipboard,
        clipboard_task,
    })
}

/// Drives the adaptation controller for one session: applies every
/// `Feedback` as it arrives, ticks the controller once a second, and on
/// every `Decision` updates the encoder's rate (via `rate_control`) and
/// tells the client (`ControlMessage::Quality`, see `proto/src/control.rs`).
/// Runs until its `Feedback` channel closes or `ActiveSession::shutdown`
/// aborts the task.
async fn run_adapt_task(
    cfg: AdaptConfig,
    mut rx: mpsc::UnboundedReceiver<Feedback>,
    rate_control: Arc<crate::pipeline::RateControl>,
    pipeline_stats: Arc<crate::pipeline::PipelineStats>,
    peer: Arc<PeerSession>,
) {
    let now = Instant::now();
    let mut controller = Controller::new(cfg, now);
    // Capture-slot overwrites are read off the pipeline's counter as a delta
    // per tick (see `Feedback::Overrun`), not pushed per event.
    let mut overwritten_seen = pipeline_stats
        .overwritten
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

    loop {
        tokio::select! {
            fb = rx.recv() => {
                match fb {
                    Some(fb) => controller.feedback(fb, Instant::now()),
                    None => break,
                }
            }
            _ = ticker.tick() => {
                let overwritten_now = pipeline_stats
                    .overwritten
                    .load(std::sync::atomic::Ordering::Relaxed);
                let overrun = overwritten_now.saturating_sub(overwritten_seen);
                overwritten_seen = overwritten_now;
                if overrun > 0 {
                    controller.feedback(Feedback::Overrun { frames: overrun }, Instant::now());
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

/// A `HostContext` wired entirely to synthetic/no-op backends (no real
/// capture, injector, cursor or clipboard access), for tests that need a
/// `HostContext` but not the platform underneath it. `pub(crate)` (rather
/// than nested inside `mod tests`) so `app`'s own reconnect test (slice
/// 2.6a) can reuse it instead of duplicating this fixture.
#[cfg(test)]
pub(crate) fn test_ctx() -> HostContext {
    HostContext {
        session: SessionConfig {
            ice_servers: vec![],
            udp_addrs: vec!["127.0.0.1:0".to_string()],
            fps: 30,
        },
        bitrate_kbps: 2000,
        adapt: false,
        max_qp: None,
        encoder: Some(EncoderKind::OpenH264),
        build_source: Box::new(|id| {
            Ok(Box::new(capture::synthetic::for_display(id, 30)?) as Box<dyn FrameSource>)
        }),
        list_displays: Box::new(|| Ok(capture::synthetic::list_displays())),
        display: None,
        build_injector: Box::new(|| {
            Ok(Box::new(crate::input::NoopInjector::new()) as Box<dyn Injector>)
        }),
        build_cursor_source: Box::new(|| {
            Box::new(crate::cursor::NoopCursorSource::new()) as Box<dyn CursorSource>
        }),
        build_clipboard: None,
        runtime: webrtc::runtime::default_runtime().expect("runtime-tokio feature must be enabled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, title: &str, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            title: title.to_string(),
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            primary,
        }
    }

    #[test]
    fn session_ice_servers_puts_registered_first_then_extra() {
        fn server(url: &str) -> proto::signal::IceServer {
            proto::signal::IceServer {
                urls: vec![url.to_string()],
                username: None,
                credential: None,
            }
        }

        let registered = vec![server("stun:server-a"), server("stun:server-b")];
        let extra = vec![server("stun:cli-extra")];

        let combined = session_ice_servers(&registered, &extra);

        assert_eq!(
            combined,
            vec![
                server("stun:server-a"),
                server("stun:server-b"),
                server("stun:cli-extra"),
            ]
        );
    }

    #[test]
    fn displays_message_builds_the_expected_control_message() {
        let displays = vec![display(1, "Built-in", true), display(2, "External", false)];

        let msg = displays_message(&displays, 2);

        assert_eq!(
            msg,
            ControlMessage::Displays {
                displays: vec![
                    DisplayEntry {
                        id: 1,
                        title: "Built-in".to_string(),
                        width: 100,
                        height: 100,
                        primary: true,
                    },
                    DisplayEntry {
                        id: 2,
                        title: "External".to_string(),
                        width: 100,
                        height: 100,
                        primary: false,
                    },
                ],
                current: 2,
            }
        );
    }

    /// Collects frames from `rx` until either `n` arrive or `timeout` elapses,
    /// panicking on timeout (the synthetic source delivers frames regularly,
    /// so a stall means the pipeline broke).
    async fn collect_frames(
        rx: &mut mpsc::Receiver<EncodedFrame>,
        n: usize,
        timeout: Duration,
    ) -> Vec<EncodedFrame> {
        let mut frames = Vec::with_capacity(n);
        tokio::time::timeout(timeout, async {
            while frames.len() < n {
                match rx.recv().await {
                    Some(frame) => frames.push(frame),
                    None => break,
                }
            }
        })
        .await
        .expect("timed out waiting for frames");
        frames
    }

    /// Reads frames from `rx` until a keyframe is seen or `timeout` elapses,
    /// panicking on timeout.
    async fn wait_for_keyframe(
        rx: &mut mpsc::Receiver<EncodedFrame>,
        timeout: Duration,
    ) -> EncodedFrame {
        tokio::time::timeout(timeout, async {
            loop {
                let frame = rx.recv().await.expect("video channel closed");
                if frame.keyframe {
                    return frame;
                }
            }
        })
        .await
        .expect("timed out waiting for a keyframe")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn switching_display_restarts_pipeline_and_keeps_frames_flowing() {
        let ctx = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel(64);
        let peer = PeerSession::new(ctx.session.clone(), events_tx, Arc::clone(&ctx.runtime))
            .await
            .unwrap();

        let (video_tx, mut video_rx) = mpsc::channel(4);
        let keyframe_flag = Arc::new(AtomicBool::new(false));

        let displays = capture::synthetic::list_displays();
        let display1 = displays.iter().find(|d| d.id == 1).unwrap().clone();
        let display2 = displays.iter().find(|d| d.id == 2).unwrap().clone();

        let mut current = VideoPipeline::start(
            &ctx,
            display1,
            Arc::clone(&peer),
            video_tx.clone(),
            Arc::clone(&keyframe_flag),
        )
        .await
        .unwrap();

        let first_batch = collect_frames(&mut video_rx, 5, Duration::from_secs(5)).await;
        assert!(
            first_batch[0].keyframe,
            "first frame of a fresh pipeline must be a keyframe"
        );

        let next = VideoPipeline::start(
            &ctx,
            display2,
            Arc::clone(&peer),
            video_tx.clone(),
            Arc::clone(&keyframe_flag),
        )
        .await
        .unwrap();
        let old = std::mem::replace(&mut current, next);
        old.stop();

        // The new pipeline forces a keyframe on start; frames from the old
        // one may still be interleaved briefly, so look for the first
        // keyframe after the switch rather than assuming the very next
        // frame is it.
        let _keyframe_after_switch = wait_for_keyframe(&mut video_rx, Duration::from_secs(5)).await;

        // Frames keep flowing after the switch.
        let _more = collect_frames(&mut video_rx, 5, Duration::from_secs(5)).await;

        current.stop();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_session_survives_a_failing_injector_and_reports_view_only() {
        let mut ctx = test_ctx();
        ctx.build_injector = Box::new(|| Err(anyhow::anyhow!("no Accessibility permission")));
        let (events_tx, _events_rx) = mpsc::channel(64);

        let parts = start_session(&ctx, &[], events_tx)
            .await
            .expect("a failing injector must not fail session start");

        assert!(!parts.input_available);
        assert_eq!(
            parts.input_reason.as_deref(),
            Some("no Accessibility permission")
        );

        // `parts.video`'s own frame channel is consumed internally by
        // `PeerSession::start_video`, which only forwards frames once DTLS
        // has actually negotiated -- unobservable from a unit test with no
        // real peer. Instead, prove the video path itself (built by
        // `start_session` the same way, from the same `ctx`/display, before
        // `build_injector` is ever called) really produces frames, the same
        // way `switching_display_restarts_pipeline_and_keeps_frames_flowing`
        // does above.
        let (video_tx, mut video_rx) = mpsc::channel(4);
        let keyframe_flag = Arc::new(AtomicBool::new(false));
        let pipeline = VideoPipeline::start(
            &ctx,
            parts.video.display.clone(),
            Arc::clone(&parts.peer),
            video_tx,
            keyframe_flag,
        )
        .await
        .unwrap();
        let frames = collect_frames(&mut video_rx, 5, Duration::from_secs(5)).await;
        assert!(frames[0].keyframe, "first frame must be a keyframe");
        pipeline.stop();

        parts.video.stop();
        parts.cursor_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_session_reports_input_available_with_a_working_injector() {
        let ctx = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel(64);

        let parts = start_session(&ctx, &[], events_tx).await.unwrap();

        assert!(parts.input_available);
        assert_eq!(parts.input_reason, None);

        parts.video.stop();
        parts.cursor_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_joined_sends_bye_when_session_start_fails() {
        let mut ctx = test_ctx();
        ctx.build_source = Box::new(|_id| anyhow::bail!("synthetic source failure"));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
        )
        .await;

        assert!(active.is_none());
        assert_eq!(current_session_id, None);
        assert_eq!(
            out_rx.try_recv().expect("expected a Bye message"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string()
            }
        );
    }

    /// `InputMessage::ClipboardText` arriving on the `input` channel must be
    /// applied straight to the session's clipboard backend, not handed to
    /// `InputRouter` like every other input message (slice 2.5b) -- see
    /// `apply_remote_clipboard_text`'s doc comment for why (ordering with
    /// the next `Key`).
    #[tokio::test(flavor = "multi_thread")]
    async fn clipboard_text_on_input_channel_is_applied_directly_not_routed() {
        let mut ctx = test_ctx();
        let fake_clipboard = crate::clipboard::FakeClipboardBackend::new();
        let fake_for_ctx = fake_clipboard.clone();
        ctx.build_clipboard = Some(Box::new(move || {
            Ok(Box::new(fake_for_ctx.clone()) as Box<dyn ClipboardBackend>)
        }));

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
        )
        .await;
        assert!(active.is_some(), "session must have started successfully");
        let _ = out_rx.try_recv(); // the Offer

        let msg = proto::input::InputMessage::ClipboardText {
            text: "from client".to_string(),
        };
        let data = serde_json::to_vec(&msg).unwrap();
        handle_session_event(
            SessionEvent::DataChannelMessage {
                label: "input".to_string(),
                data: data.into(),
                is_string: true,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &mut active,
            &mut current_session_id,
        )
        .await;

        assert_eq!(fake_clipboard.set_calls(), vec!["from client".to_string()]);

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }
}
