//! WebSocket signaling client: registers this host with the signaling
//! server (see `server/src/ws.rs` for the protocol this speaks to) and, for
//! the one active session, wires a `transport::PeerSession` and video
//! pipeline together and forwards SDP/ICE between them and the wire.
//!
//! Reconnecting after a drop is handled one layer up, by `crate::app::run_agent`,
//! which loops `SignalingClient::connect` + `run()` with backoff (slice
//! 2.6a): `run()` itself still just returns an `Err` the moment the
//! connection is lost, same as before.
//!
//! The one active session's state lives in `SessionSlot`, owned by
//! `run_agent` across every reconnect (slice 3.5a): a session is
//! peer-to-peer once its `RTCPeerConnection` is up, so losing the signaling
//! WebSocket -- a server restart, a flaky network -- does not end it. `run()`
//! takes the slot as `&mut` and no longer shuts an active session down when
//! it returns; `run_agent` keeps draining the slot's events/commands
//! (`SessionSlot::run_while_offline`) even while there's no connection to
//! reconnect on.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use proto::control::{ControlMessage, DisplayEntry, QualityPreset};
use proto::signal::{DeviceCredentials, Role, SignalMessage};
use webrtc::peer_connection::RTCPeerConnectionState;
use webrtc::runtime::Runtime;

use crate::adapt::{AdaptConfig, Controller, Feedback};
use crate::capture::{self, DisplayInfo, FrameSource};
use crate::clipboard::{self, ClipboardBackend, ClipboardSync};
use crate::cursor::{self, CursorSource, CursorState};
use crate::encode::{build_encoder, EncodedFrame, EncoderConfig, EncoderKind, RateTarget};
use crate::input::{Injector, InputRouter, NoopInjector};
use crate::pipeline::Pipeline;
#[cfg(target_os = "windows")]
use crate::platform;
use crate::transport::{PeerSession, SessionConfig, SessionEvent};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often the cursor watcher thread polls the platform for the current
/// system cursor shape (see `crate::cursor::watch`). 33ms is roughly 30Hz --
/// plenty for a cursor shape, which changes far less often than the pointer
/// moves.
const CURSOR_POLL_INTERVAL: Duration = Duration::from_millis(33);

/// How often the Windows foreground-window elevation watcher polls
/// `platform::windows::elevation::foreground_input_blocked` (slice 2.6e).
/// 1s: this is a warning banner, not a latency-sensitive path, and the
/// watcher only speaks up on an actual state change (see
/// `InputBlockedWatcher::update`).
#[cfg(target_os = "windows")]
const ELEVATION_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `ControlMessage::InputBlocked { blocked: true, .. }`'s `reason`, shown to
/// the client while the foreground window runs elevated relative to this
/// agent (slice 2.6e). `InputBlockedWatcher`, which uses it, is
/// platform-independent code (see its doc comment), so on a non-Windows
/// build the only caller left is the `cfg(test)` unit tests below -- hence
/// `cfg(any(test, target_os = "windows"))` rather than an unconditional
/// `pub`/no-cfg, which would be genuine dead code (and a clippy error) on
/// e.g. a plain macOS release build.
#[cfg(any(test, target_os = "windows"))]
const ELEVATED_INPUT_BLOCKED_REASON: &str =
    "Активное окно запущено с правами администратора: Windows блокирует ввод (UIPI)";

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
    /// Slice 3.2c: the host's access password, if one is set (`access::AccessStore::load`
    /// returns `Some` -- see `access.rs`'s module doc comment). When present,
    /// `handle_signal_message`'s `PeerJoined` arm asks the joining client to
    /// complete an OPAQUE login (`AuthRequired`/`PakeStart`/`PakeResponse`/
    /// `PakeFinish`) before offering a session, instead of offering right
    /// away. `main.rs`/`app::build_host_context` point this at the same
    /// data directory as `device::DeviceStore` (`agent::paths::data_dir()`).
    pub access_store: crate::access::AccessStore,
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

/// Slice 3.5b: how long a `RTCPeerConnectionState::Disconnected` is
/// tolerated before the session is torn down. ICE can flap through
/// `Disconnected` on a brief network hiccup (a Wi-Fi roam, a dropped packet
/// train) and recover to `Connected` on its own -- only `Failed`/`Closed`,
/// or this grace window expiring without a recovery, actually end the
/// session. See `handle_session_event`'s `ConnectionState`/
/// `DisconnectTimeout` arms.
const DISCONNECT_GRACE: Duration = Duration::from_secs(15);

/// Slice 3.2c: how long a `PendingAuth` (armed on `AuthRequired`, see
/// `handle_signal_message`'s `PeerJoined` arm) waits for the client to
/// finish its OPAQUE login before the host gives up and sends `Bye`. Same
/// self-timer pattern as `DISCONNECT_GRACE`/`SessionEvent::DisconnectTimeout`.
const AUTH_TIMEOUT: Duration = Duration::from_secs(120);

/// Slice 3.2c: how many consecutive failed `PakeStart` attempts a client may
/// make before the host starts throttling further attempts -- see
/// `auth_lockout_duration`. This is a pace limit, not a hard lockout: once
/// `auth_failures` crosses this, every later `PakeStart` still runs, just
/// after an ever-growing pause before it (see `handle_signal_message`'s
/// `PakeStart` arm's doc comment for why it must still run -- refusing it
/// outright would mean the owner could never log in again after this many
/// failures). `SessionSlot::auth_failures` counts *starts*, not finishes:
/// each `PakeStart` costs the host one OPRF evaluation regardless of
/// whether the client's password was right, and a wrong password is only
/// ever caught client-side (see `access::LoginServer::login_finish`'s doc
/// comment), so counting starts is what actually paces the attacker's cost.
const MAX_AUTH_ATTEMPTS: u32 = 5;

/// Slice 3.2c: hands out a fresh, process-wide unique generation for a
/// `PendingAuth`'s `AuthTimeout` timer -- same pattern as
/// `transport::next_session_tag`, for the same reason: distinguishing a
/// timeout for a `PendingAuth` that's since been replaced (a new
/// `PeerJoined`) or resolved (a successful `PakeFinish`) from the one
/// actually still pending.
static NEXT_AUTH_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_auth_generation() -> u64 {
    NEXT_AUTH_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Slice 3.2c: the throttling pause armed before the *next* `PakeStart`,
/// given `auth_failures` consecutive failed attempts so far -- only
/// meaningful once `auth_failures > MAX_AUTH_ATTEMPTS` (i.e.
/// `auth_failures >= 6`, the caller's guard): 5s after the 6th attempt,
/// doubling after each further attempt, capped at 60s.
fn auth_lockout_duration(auth_failures: u32) -> Duration {
    let exponent = auth_failures.saturating_sub(6).min(63);
    let secs = 5u64.saturating_mul(1u64 << exponent);
    Duration::from_secs(secs.min(60))
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

/// A command sent into `SignalingClient::run`'s event loop from outside it
/// -- the tray agent's "End session" menu item (slice 2.6c) is the first and
/// only source today. The same shape as `SessionEvent` but for intent
/// flowing the other way (UI -> signaling loop instead of transport ->
/// signaling loop). `app::run_agent` owns the receiving end across every
/// reconnect and hands `SignalingClient::run` a `&mut` to it each time (see
/// that function's doc comment) so a command sent while the host is
/// reconnecting isn't lost, just delayed until the next `run` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCommand {
    /// End the current session right now, if one is active: tells the
    /// server (`SignalMessage::Bye`), tears down local session resources and
    /// reports `AgentStatus::Registered` again. A no-op (debug-logged) when
    /// no session is active.
    EndSession,
}

/// Why `SignalingClient::connect` failed. Distinguishes the one case
/// `app::run_agent` must react to specially -- the server not recognizing a
/// saved device (slice 3.1c) -- from every other failure.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The signaling server doesn't know this device (e.g. its database was
    /// recreated) -- the caller should forget its saved credentials and
    /// register again as a new device.
    #[error("signaling server does not know this device")]
    UnknownDevice,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A registered signaling connection, holding the PIN the owner reads out to
/// pair a client.
pub struct SignalingClient {
    host_id: String,
    pin: String,
    ice_servers: Vec<proto::signal::IceServer>,
    /// Freshly issued device credentials (slice 3.1c), present only when
    /// this `connect` call was this device's very first registration -- see
    /// `issued_device`.
    issued_device: Option<DeviceCredentials>,
    write: SplitSink<WsStream, Message>,
    read: SplitStream<WsStream>,
}

impl SignalingClient {
    /// Connects to the signaling server, sends `Hello` + `HostRegister`
    /// (carrying `device`, if the caller has saved credentials from a
    /// previous registration -- slice 3.1c -- and `session_id`, if the
    /// caller's `SessionSlot` still has a live session from before this
    /// reconnect -- slice 3.2a/D35, see `SessionSlot::current_session_id`),
    /// and waits for `Registered`.
    pub async fn connect(
        url: &str,
        name: &str,
        device: Option<DeviceCredentials>,
        session_id: Option<String>,
    ) -> Result<Self, ConnectError> {
        let (ws_stream, _response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(anyhow::Error::from)?;
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
                device,
                session_id,
            },
        )
        .await?;

        let (host_id, pin, ice_servers, issued_device) = match next_message(&mut read).await {
            Some(SignalMessage::Registered {
                host_id,
                pin,
                ice_servers,
                device,
            }) => (host_id, pin, ice_servers, device),
            Some(SignalMessage::Error { message }) => {
                if message == "unknown device" {
                    return Err(ConnectError::UnknownDevice);
                }
                return Err(ConnectError::Other(anyhow::anyhow!(
                    "signaling server rejected registration: {message}"
                )));
            }
            Some(other) => {
                return Err(ConnectError::Other(anyhow::anyhow!(
                    "unexpected reply while registering: {other:?}"
                )))
            }
            None => {
                return Err(ConnectError::Other(anyhow::anyhow!(
                    "signaling connection closed before registration completed"
                )))
            }
        };

        Ok(Self {
            host_id,
            pin,
            ice_servers,
            issued_device,
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

    /// Freshly issued device credentials (slice 3.1c), as `Registered` sent
    /// them -- `Some` only on this device's very first registration; on a
    /// successful re-registration with already-known credentials it's
    /// `None`, since the caller already has what it needs saved. See
    /// `app::run_agent`, which saves this via `device::DeviceStore::save`.
    pub fn issued_device(&self) -> Option<&DeviceCredentials> {
        self.issued_device.as_ref()
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
    ///
    /// `commands` is the tray agent's "End session" (and any future command,
    /// slice 2.6c) input, owned by `app::run_agent` across every reconnect
    /// -- see `AgentCommand`'s doc comment.
    ///
    /// `slot` is the one active session's state (`SessionSlot`), also owned
    /// by `app::run_agent` across every reconnect (slice 3.5a). Unlike
    /// before 3.5a, `run` does **not** shut an active session down when it
    /// returns -- losing signaling doesn't end a peer-to-peer session, only
    /// the ability to set up a new one until the socket comes back.
    pub async fn run(
        self,
        ctx: &HostContext,
        status: &watch::Sender<AgentStatus>,
        keepalive: Keepalive,
        commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
        slot: &mut SessionSlot,
    ) -> anyhow::Result<()> {
        let SignalingClient {
            host_id,
            pin,
            ice_servers: registered_ice_servers,
            issued_device: _,
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
                        slot.event_tx.clone(),
                        &mut slot.active,
                        &mut slot.current_session_id,
                        &mut slot.pending_auth,
                        &mut slot.auth_failures,
                        &mut slot.auth_locked_until,
                    )
                    .await;
                }
                Some(event) = slot.event_rx.recv() => {
                    handle_session_event(event, ctx, &pin, status, &out_tx, &slot.event_tx, &mut slot.active, &mut slot.current_session_id, &mut slot.pending_auth).await;
                }
                Some(cmd) = commands.recv() => {
                    handle_agent_command(cmd, &pin, status, &out_tx, &mut slot.active, &mut slot.current_session_id, &mut slot.pending_auth).await;
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

        // Deliberately no `slot.active` shutdown here (slice 3.5a): the
        // session is peer-to-peer and outlives this signaling connection --
        // `run_agent` keeps it alive across the reconnect that follows.
        //
        // `slot.pending_auth`, on the other hand, has no P2P channel of its
        // own yet (slice 3.2c) -- the OPAQUE handshake only ever exists
        // relayed through this very socket -- so it cannot survive the
        // reconnect that follows; clear it rather than leave a stale wait
        // for `AuthTimeout` to eventually clean up on its own.
        slot.pending_auth = None;
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
    /// Test-only escape hatch onto the pipeline's `RateControl` (slice
    /// 3.5e), so a test can observe `ControlMessage::SetQuality` actually
    /// reaching the adaptation controller (via `run_adapt_task`'s real
    /// ticker) without a real WebRTC peer to read `ControlMessage::Quality`
    /// back off of -- see `switching_quality_preset_reaches_the_adapt_controller`.
    #[cfg(test)]
    rate_control: Arc<crate::pipeline::RateControl>,
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
        #[cfg(test)]
        let rate_control_for_test = Arc::clone(&rate_control);

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
            #[cfg(test)]
            rate_control: rate_control_for_test,
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

/// Tracks the Windows foreground-window elevation watcher's last-reported
/// state across its 1s polls (slice 2.6e), so the background task (Windows
/// only, see `start_session`) sends `ControlMessage::InputBlocked` exactly
/// on a *change* rather than once per poll. Deliberately platform-independent
/// -- it holds no Windows types and calls no Windows API, only `update` --
/// so it stays unit-testable on any OS (the actual `foreground_input_blocked`
/// check lives in `platform::windows::elevation`, cfg-gated). The type
/// itself is `cfg(any(test, target_os = "windows"))` -- see
/// `ELEVATED_INPUT_BLOCKED_REASON`'s doc comment for why: its only
/// production caller (`spawn_elevation_watcher`) is Windows-only, so
/// without `test` in the `cfg` it would be dead code everywhere else.
#[cfg(any(test, target_os = "windows"))]
#[derive(Default)]
struct InputBlockedWatcher {
    /// `None` before the first poll.
    last: Option<bool>,
}

#[cfg(any(test, target_os = "windows"))]
impl InputBlockedWatcher {
    /// Feeds one freshly observed `blocked` state in. Returns
    /// `Some(ControlMessage::InputBlocked)` when the client needs telling:
    /// on every poll after the first where the state changed, and on the
    /// very first poll only when it's already `true` -- a client that never
    /// receives this message defaults to "not blocked"
    /// (`ControlMessage::InputBlocked`'s doc comment), so a quiet `false`
    /// first poll needs no message.
    fn update(&mut self, blocked: bool) -> Option<ControlMessage> {
        let first_poll = self.last.is_none();
        let changed = self.last != Some(blocked);
        self.last = Some(blocked);

        if first_poll {
            return blocked.then(|| input_blocked_message(true));
        }
        changed.then(|| input_blocked_message(blocked))
    }
}

/// Builds `ControlMessage::InputBlocked`, filling in `ELEVATED_INPUT_BLOCKED_REASON`
/// for `blocked: true` and `None` for `blocked: false`.
#[cfg(any(test, target_os = "windows"))]
fn input_blocked_message(blocked: bool) -> ControlMessage {
    ControlMessage::InputBlocked {
        blocked,
        reason: blocked.then(|| ELEVATED_INPUT_BLOCKED_REASON.to_string()),
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
    /// The Windows foreground-window elevation watcher (slice 2.6e), polling
    /// `platform::windows::elevation::foreground_input_blocked` once a
    /// second and forwarding changes as `ControlMessage::InputBlocked`.
    /// `None` on non-Windows platforms, and on Windows too when the session
    /// has no real input injector (`input_available == false`) -- nothing
    /// to warn about when input isn't wired up at all.
    elevation_task: Option<tokio::task::JoinHandle<()>>,
    /// This session's `PeerSession::tag()` (fix to slice 3.5b). Events that
    /// can end a session (`SessionEvent::ConnectionState`/`DataChannelClosed`)
    /// carry the tag of the `PeerSession` that produced them;
    /// `handle_session_event` compares it against this field and ignores a
    /// mismatch as stale -- see those variants' doc comments in
    /// `transport::SessionEvent` for the scenario this guards against
    /// (an old, already-superseded session's event arriving after a new one
    /// has taken its place, e.g. during automatic client reconnection).
    session_tag: u64,
    /// The owner's chosen quality preset (slice 3.5e, `ControlMessage::SetQuality`),
    /// starting at `QualityPreset::Auto` for every new session. Kept here
    /// (rather than only inside the adapt task's `Controller`) so
    /// `switch_display` -- which rebuilds `VideoPipeline` and therefore
    /// starts a fresh `Controller` at `Auto` -- can re-apply it to the new
    /// pipeline's controller instead of silently resetting the owner's
    /// choice on every display switch.
    quality_preset: QualityPreset,
    /// Slice 3.5b: tracks the current `RTCPeerConnectionState::Disconnected`
    /// grace window (if any) for this session -- see `DisconnectGrace`'s
    /// doc comment and `handle_session_event`'s `ConnectionState`/
    /// `DisconnectTimeout` arms, its only caller.
    disconnect_grace: DisconnectGrace,
    /// The OPAQUE session key from a successful login (slice 3.2c), when
    /// this session was gated by an access password -- `None` when the
    /// host has no password set. Unused today beyond being kept here;
    /// slice 3.2e will feed it into the data channel encryption.
    #[allow(dead_code)]
    session_key: Option<crate::access::SessionKey>,
}

/// Slice 3.5b: pure decision logic for the `RTCPeerConnectionState::Disconnected`
/// grace window (`DISCONNECT_GRACE`) -- whether a `Disconnected` should arm a
/// fresh timeout timer, whether a `Connected` cancels one in progress, and
/// whether a `SessionEvent::DisconnectTimeout` that comes back is still the
/// current window or a stale one from a window a `Connected` already
/// cancelled. Deliberately free of any timer/async/webrtc type -- see
/// `handle_session_event`, the only caller -- so it's unit-testable as plain
/// data (this workspace does not enable tokio's `test-util` feature needed
/// to pause a real clock in a test).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DisconnectGrace {
    /// Monotonically increasing, bumped every time a fresh grace window
    /// starts. Never reset for the session's lifetime.
    epoch: u64,
    /// `Some(epoch)` while a grace window is running; that `epoch` is the
    /// generation `handle_session_event` arms the timer with.
    pending: Option<u64>,
}

impl DisconnectGrace {
    /// The connection reported `Disconnected`. Returns the new window's
    /// generation when this starts a *fresh* window (the caller should arm
    /// a `DISCONNECT_GRACE` timer for it), or `None` when a window was
    /// already running -- repeated `Disconnected` events (if the
    /// `RTCPeerConnection` ever reports it more than once in a row) must
    /// not re-arm the timer or bump the generation out from under it.
    fn disconnected(&mut self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.epoch += 1;
        self.pending = Some(self.epoch);
        Some(self.epoch)
    }

    /// The connection reported `Connected`: cancels any grace window in
    /// progress.
    fn connected(&mut self) {
        self.pending = None;
    }

    /// Whether a `DisconnectTimeout` for `generation` still refers to the
    /// window currently running -- `false` once a `Connected` cancelled it
    /// (or a session ended and a new one started, tracked separately by
    /// `handle_session_event` via `current_session_id`).
    fn is_current(&self, generation: u64) -> bool {
        self.pending == Some(generation)
    }
}

#[cfg(test)]
mod disconnect_grace_tests {
    use super::DisconnectGrace;

    #[test]
    fn first_disconnect_starts_a_window_with_generation_one() {
        let mut grace = DisconnectGrace::default();
        assert_eq!(grace.disconnected(), Some(1));
        assert!(grace.is_current(1));
    }

    #[test]
    fn a_second_disconnect_while_one_is_pending_does_not_rearm() {
        let mut grace = DisconnectGrace::default();
        assert_eq!(grace.disconnected(), Some(1));
        assert_eq!(
            grace.disconnected(),
            None,
            "a window is already running, must not restart it"
        );
        assert!(grace.is_current(1));
    }

    #[test]
    fn connected_cancels_the_pending_window() {
        let mut grace = DisconnectGrace::default();
        grace.disconnected();
        grace.connected();
        assert!(!grace.is_current(1));
    }

    #[test]
    fn a_timeout_for_a_cancelled_window_is_not_current() {
        let mut grace = DisconnectGrace::default();
        let generation = grace.disconnected().unwrap();
        grace.connected();
        // A later disconnect starts a fresh window with a new generation --
        // the stale one from before `connected()` must stay stale even
        // though a window is running again.
        let new_generation = grace.disconnected().unwrap();
        assert_ne!(generation, new_generation);
        assert!(!grace.is_current(generation));
        assert!(grace.is_current(new_generation));
    }

    #[test]
    fn no_window_is_pending_before_any_disconnect() {
        let grace = DisconnectGrace::default();
        assert!(!grace.is_current(0));
        assert!(!grace.is_current(1));
    }
}

impl ActiveSession {
    async fn shutdown(self) {
        self.peer.close().await;
        self.video.stop();
        self.cursor_task.abort();
        if let Some(task) = self.clipboard_task {
            task.abort();
        }
        if let Some(task) = self.elevation_task {
            task.abort();
        }
    }
}

/// One session waiting on an OPAQUE login (slice 3.2c) before the host will
/// offer it, armed by `handle_signal_message`'s `PeerJoined` arm when
/// `HostContext::access_store` has a password set. Lives in `SessionSlot`
/// separately from `active`/`current_session_id` -- unlike an `ActiveSession`,
/// there is no `PeerSession`/video pipeline yet, nothing to shut down, only
/// state to remember while the client works through `PakeStart`/`PakeFinish`.
struct PendingAuth {
    /// The session id from the `PeerJoined` that armed this wait -- matched
    /// against every incoming `PakeStart`/`PakeFinish`/`Bye` so a message
    /// for some other (already superseded) session id is ignored.
    session_id: String,
    /// This `PeerJoined`'s `ice_servers`, kept so a successful `PakeFinish`
    /// can start the session with the same credentials `PeerJoined`
    /// carried, exactly as the no-password path would have used right away.
    ice_servers: Vec<proto::signal::IceServer>,
    /// The server half of the in-progress OPAQUE login, set by a successful
    /// `PakeStart` and consumed by the next `PakeFinish`. `None` before the
    /// first `PakeStart`, and again right after a `PakeFinish` that failed
    /// (see `access::LoginServer::login_finish`'s doc comment) -- the wait
    /// stays armed, but the client must start over with a fresh `PakeStart`.
    login: Option<crate::access::LoginServer>,
    /// This wait's `AuthTimeout` generation (see `next_auth_generation`) --
    /// matched by `handle_session_event`'s `AuthTimeout` arm against a timer
    /// armed for it, same reasoning as `DisconnectGrace`'s `epoch`.
    generation: u64,
}

/// The one active session's state (if any), owned by `app::run_agent`
/// across every signaling reconnect (slice 3.5a). A session is
/// peer-to-peer once its `RTCPeerConnection` is up -- signaling only sets it
/// up -- so losing the WebSocket (server restart, flaky network on either
/// side) must not tear it down; only the ability to negotiate a *new*
/// session is gone until the socket comes back. `SignalingClient::run`
/// takes this as `&mut` instead of owning equivalent fields itself, and no
/// longer shuts an active session down on exit.
pub struct SessionSlot {
    active: Option<ActiveSession>,
    current_session_id: Option<String>,
    event_tx: mpsc::Sender<SessionEvent>,
    event_rx: mpsc::Receiver<SessionEvent>,
    /// Slice 3.2c: the session currently waiting on an OPAQUE login, if
    /// any -- see `PendingAuth`'s doc comment. Unlike `active`, this has no
    /// P2P channel of its own yet, so it does not survive losing this
    /// signaling connection (see `SignalingClient::run`'s exit path).
    pending_auth: Option<PendingAuth>,
    /// Slice 3.2c: consecutive failed `PakeStart` attempts against this
    /// host's access password, across every `PendingAuth` -- deliberately
    /// *not* reset by a fresh `PeerJoined` (only a successful `PakeFinish`
    /// resets it): it lives in the slot and survives a signaling reconnect,
    /// so a client can't dodge the throttle below by reconnecting.
    auth_failures: u32,
    /// Slice 3.2c: armed once `auth_failures` crosses `MAX_AUTH_ATTEMPTS`,
    /// as a *pace limit* on the next `PakeStart`, not a hard lockout: while
    /// in the future, a `PakeStart` is rejected with
    /// `AuthFailed { retry_after_secs: Some(..) } }` without spending an
    /// OPRF evaluation or touching `auth_failures` further, but once it's in
    /// the past the next attempt runs as normal (and, if it also fails,
    /// re-arms this for a longer pause -- see `auth_lockout_duration`).
    auth_locked_until: Option<Instant>,
}

impl SessionSlot {
    pub fn new() -> Self {
        // Same bound as before 3.5a (when this channel was created fresh
        // inside `run`): unchanged, just moved here so it -- and any event
        // already queued on it -- survives a reconnect instead of being
        // dropped and recreated on every `SignalingClient::run` call.
        let (event_tx, event_rx) = mpsc::channel(64);
        Self {
            active: None,
            current_session_id: None,
            event_tx,
            event_rx,
            pending_auth: None,
            auth_failures: 0,
            auth_locked_until: None,
        }
    }

    /// Whether a session is live right now. `app::run_agent` checks this
    /// right after a fresh registration to pick `AgentStatus::InSession`
    /// (the session survived the reconnect) over `AgentStatus::Registered`.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// The id of the session currently in this slot, if any (slice
    /// 3.2a/D35). `app::run_agent` passes this into `SignalingClient::connect`
    /// so a host reconnecting to signaling while its P2P session is still up
    /// reports it in `HostRegister`, letting the server mark the device busy
    /// (or re-attach the session, if it's the same server instance that set
    /// it up) instead of looking freshly idle.
    pub fn current_session_id(&self) -> Option<String> {
        self.current_session_id.clone()
    }

    /// Drains this slot's session events and `AgentCommand`s while `future`
    /// runs, for stretches with no live signaling connection to hand them to
    /// -- `app::run_agent` mid-`connect` attempt, or sleeping out its
    /// backoff. This isn't optional: `event_rx` is bounded (64) and its
    /// senders live on the session's own WebRTC tasks (input arriving over
    /// data channels, connection-state changes, ...) -- left undrained past
    /// one reconnect cycle, they'd block. `AgentCommand::EndSession` still
    /// ends the session locally (nothing else can do it while offline);
    /// there's simply no connection left to send its `Bye` on. Both
    /// handlers get throwaway `status`/`out_tx` stand-ins: the real agent
    /// status must not flip to `Registered`/`InSession` just because a
    /// session event fired while there's nothing registered to report, and
    /// an outgoing `SignalMessage` has nowhere to go anyway.
    pub async fn run_while_offline<F, T>(
        &mut self,
        ctx: &HostContext,
        commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
        future: F,
    ) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let (discard_status, _discard_status_rx) = watch::channel(AgentStatus::Connecting);
        let (discard_out_tx, discard_out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        drop(discard_out_rx);

        tokio::pin!(future);
        loop {
            tokio::select! {
                result = &mut future => return result,
                Some(event) = self.event_rx.recv() => {
                    handle_session_event(
                        event,
                        ctx,
                        "",
                        &discard_status,
                        &discard_out_tx,
                        &self.event_tx,
                        &mut self.active,
                        &mut self.current_session_id,
                        &mut self.pending_auth,
                    )
                    .await;
                }
                Some(cmd) = commands.recv() => {
                    handle_agent_command(
                        cmd,
                        "",
                        &discard_status,
                        &discard_out_tx,
                        &mut self.active,
                        &mut self.current_session_id,
                        &mut self.pending_auth,
                    )
                    .await;
                }
            }
        }
    }
}

impl Default for SessionSlot {
    fn default() -> Self {
        Self::new()
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
    pending_auth: &mut Option<PendingAuth>,
    auth_failures: &mut u32,
    auth_locked_until: &mut Option<Instant>,
) {
    match msg {
        SignalMessage::PeerJoined {
            session_id,
            ice_servers,
        } => {
            if let Some(old) = active.take() {
                tracing::info!("new PeerJoined while a session was active; closing the old one");
                old.shutdown().await;
                let _ = status.send(AgentStatus::Registered {
                    pin: pin.to_string(),
                });
            }
            *current_session_id = None;
            // Slice 3.2c: a new `PeerJoined` supersedes any login the
            // previous joining peer was in the middle of.
            *pending_auth = None;

            if ctx.access_store.load().is_some() {
                let generation = next_auth_generation();
                *pending_auth = Some(PendingAuth {
                    session_id: session_id.clone(),
                    ice_servers,
                    login: None,
                    generation,
                });
                tracing::info!(%session_id, "password required, waiting for client authentication");
                let _ = out_tx.send(SignalMessage::AuthRequired {
                    session_id: session_id.clone(),
                });
                let timer_tx = event_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(AUTH_TIMEOUT).await;
                    let _ = timer_tx
                        .send(SessionEvent::AuthTimeout {
                            session_id,
                            generation,
                        })
                        .await;
                });
            } else {
                begin_session(
                    ctx,
                    session_id,
                    ice_servers,
                    None,
                    registered_ice_servers,
                    out_tx,
                    event_tx,
                    active,
                    current_session_id,
                )
                .await;
            }
        }
        SignalMessage::Answer {
            session_id,
            sdp,
            auth,
        } => {
            if current_session_id.as_deref() != Some(session_id.as_str()) {
                tracing::debug!(%session_id, "answer for unknown/stale session, ignoring");
                return;
            }
            // Slice 3.2e: when this session was gated by a password, the
            // client's answer must carry a valid dtls-binding tag for its
            // own fingerprint before the host applies it -- otherwise a
            // signaling-server-in-the-middle could swap in its own answer
            // undetected. No password (`session_key: None`) -- unchanged
            // behavior from before this slice.
            if let Some(session_key) = active.as_ref().and_then(|s| s.session_key.as_ref()) {
                let ok = crate::dtls_bind::fingerprint_from_sdp(&sdp)
                    .zip(auth.as_deref())
                    .is_some_and(|(fingerprint, tag)| {
                        crate::dtls_bind::verify_auth_tag(
                            session_key.as_bytes(),
                            "answer",
                            &fingerprint,
                            tag,
                        )
                    });
                if !ok {
                    tracing::warn!(%session_id, "answer failed dtls binding check, ending session");
                    if let Some(old) = active.take() {
                        old.shutdown().await;
                        let _ = status.send(AgentStatus::Registered {
                            pin: pin.to_string(),
                        });
                    }
                    *current_session_id = None;
                    let _ = out_tx.send(SignalMessage::Bye { session_id });
                    return;
                }
            }
            if let Some(active) = active.as_ref() {
                if let Err(err) = active.peer.set_answer(sdp).await {
                    tracing::warn!(?err, "failed to apply remote answer");
                }
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
        // Slice 3.2c: the client's half of the OPAQUE login started by this
        // `PeerJoined`'s `AuthRequired`. Never logs `payload` (an encoded
        // OPAQUE protocol message) or any derived key material, only sizes
        // where useful for debugging.
        SignalMessage::PakeStart {
            session_id,
            payload,
        } => {
            let matches_pending = pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id);
            if !matches_pending {
                tracing::debug!(%session_id, "pake_start for no/unknown pending auth, ignoring");
                return;
            }

            if let Some(locked_until) = *auth_locked_until {
                let now = Instant::now();
                if locked_until > now {
                    let retry_after_secs = (locked_until - now).as_secs_f64().ceil() as u32;
                    tracing::debug!(%session_id, retry_after_secs, "pake_start throttled, too soon after a previous failed attempt");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: Some(retry_after_secs.max(1)),
                    });
                    return;
                }
            }

            // Slice 3.2c fix: this is throttling, not a hard lockout -- it
            // paces how often an attempt may be *started*, it does not
            // refuse to run this one. Past `MAX_AUTH_ATTEMPTS`, arm
            // `auth_locked_until` for the *next* `PakeStart` (checked above,
            // before this one is even counted) and fall straight through to
            // running this attempt as usual. A hard refuse-and-return here
            // (the pre-fix behavior) meant the owner could never actually
            // log in again after `MAX_AUTH_ATTEMPTS` failures: every later
            // attempt would hit its own freshly re-armed lockout before
            // ever reaching `login_start`.
            *auth_failures += 1;
            if *auth_failures > MAX_AUTH_ATTEMPTS {
                let lockout = auth_lockout_duration(*auth_failures);
                *auth_locked_until = Some(Instant::now() + lockout);
                tracing::warn!(
                    %session_id,
                    auth_failures = *auth_failures,
                    lockout_secs = lockout.as_secs(),
                    "login attempt {}, next attempt allowed in {} s",
                    *auth_failures,
                    lockout.as_secs()
                );
            }

            // Re-read rather than reuse anything cached from `PeerJoined`:
            // the owner may have cleared the password since (`password
            // clear`), and this must reflect that right away.
            let Some(record) = ctx.access_store.load() else {
                tracing::warn!("password file disappeared mid-auth");
                let _ = out_tx.send(SignalMessage::AuthFailed {
                    session_id,
                    retry_after_secs: None,
                });
                return;
            };

            let request_bytes = match BASE64_URL_SAFE_NO_PAD.decode(&payload) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(?err, "invalid base64 in pake_start payload");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                    return;
                }
            };

            match crate::access::login_start(&record, &request_bytes) {
                Ok((login, response_bytes)) => {
                    if let Some(pending) = pending_auth.as_mut() {
                        pending.login = Some(login);
                    }
                    let _ = out_tx.send(SignalMessage::PakeResponse {
                        session_id,
                        payload: BASE64_URL_SAFE_NO_PAD.encode(response_bytes),
                    });
                }
                Err(err) => {
                    tracing::warn!(?err, "opaque login_start failed");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                }
            }
        }
        // Slice 3.2c: completes the login `PakeStart` above started. Same
        // "never log the payload/key material" rule as `PakeStart`.
        SignalMessage::PakeFinish {
            session_id,
            payload,
        } => {
            let matches_pending = pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id && p.login.is_some());
            if !matches_pending {
                tracing::debug!(%session_id, "pake_finish with no pending login, ignoring");
                return;
            }
            // Taken here (not just borrowed): on both a successful and a
            // failed `login_finish`, the client must start a fresh
            // `PakeStart` to try again -- `opaque_ke::ServerLogin::finish`
            // consumes its state either way.
            let login = pending_auth.as_mut().and_then(|p| p.login.take());
            let Some(login) = login else {
                return;
            };

            let finalization_bytes = match BASE64_URL_SAFE_NO_PAD.decode(&payload) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(?err, "invalid base64 in pake_finish payload");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                    return;
                }
            };

            match login.login_finish(&finalization_bytes) {
                Ok(session_key) => {
                    *auth_failures = 0;
                    *auth_locked_until = None;
                    let pending = pending_auth
                        .take()
                        .expect("checked Some via matches_pending above");
                    tracing::info!(%session_id, "client authenticated");
                    begin_session(
                        ctx,
                        session_id,
                        pending.ice_servers,
                        Some(session_key),
                        registered_ice_servers,
                        out_tx,
                        event_tx,
                        active,
                        current_session_id,
                    )
                    .await;
                }
                Err(err) => {
                    // The wait stays armed (`pending_auth` is still `Some`,
                    // just with `login` now `None`) -- the client can retry
                    // with a fresh `PakeStart`.
                    tracing::warn!(?err, %session_id, "opaque login_finish failed");
                    let _ = out_tx.send(SignalMessage::AuthFailed {
                        session_id,
                        retry_after_secs: None,
                    });
                }
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
            } else if pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id)
            {
                tracing::info!(%session_id, "pending auth ended by peer");
                *pending_auth = None;
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

/// Starts the actual `PeerSession`/video pipeline and offers it to the
/// client (slice 3.2c): factored out of `handle_signal_message`'s
/// `PeerJoined` arm so it can be called either right away (no access
/// password set) or after a successful OPAQUE login (`PakeFinish`)
/// completes. `session_key` is `Some` only in the latter case.
#[allow(clippy::too_many_arguments)]
async fn begin_session(
    ctx: &HostContext,
    session_id: String,
    ice_servers: Vec<proto::signal::IceServer>,
    session_key: Option<crate::access::SessionKey>,
    registered_ice_servers: &[proto::signal::IceServer],
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    event_tx: mpsc::Sender<SessionEvent>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
) {
    let chosen_ice_servers = peer_ice_servers(&ice_servers, registered_ice_servers);
    match start_session(ctx, chosen_ice_servers, event_tx).await {
        Ok(parts) => match parts.peer.create_offer().await {
            Ok(sdp) => {
                // Slice 3.2e: when this session was gated by a password,
                // bind the offer to the OPAQUE session key -- extract this
                // host's own DTLS fingerprint from the offer it just built
                // and tag it. `create_offer` always produces an SDP with a
                // fingerprint (`webrtc-rs` always sets up DTLS), so `None`
                // here would mean something is badly wrong with the SDP;
                // treat it the same as a `create_offer` failure rather than
                // silently offering an unbound session.
                let auth = match session_key.as_ref() {
                    Some(key) => match crate::dtls_bind::fingerprint_from_sdp(&sdp) {
                        Some(fingerprint) => Some(crate::dtls_bind::auth_tag(
                            key.as_bytes(),
                            "offer",
                            &fingerprint,
                        )),
                        None => {
                            tracing::warn!(
                                "offer sdp has no dtls fingerprint, refusing to start session"
                            );
                            parts.video.stop();
                            parts.cursor_task.abort();
                            if let Some(task) = parts.clipboard_task {
                                task.abort();
                            }
                            if let Some(task) = parts.elevation_task {
                                task.abort();
                            }
                            let _ = out_tx.send(SignalMessage::Bye { session_id });
                            return;
                        }
                    },
                    None => None,
                };
                let _ = out_tx.send(SignalMessage::Offer {
                    session_id: session_id.clone(),
                    sdp,
                    auth,
                });
                *current_session_id = Some(session_id);
                let session_tag = parts.peer.tag();
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
                    elevation_task: parts.elevation_task,
                    session_tag,
                    quality_preset: QualityPreset::Auto,
                    disconnect_grace: DisconnectGrace::default(),
                    session_key,
                });
            }
            Err(err) => {
                tracing::warn!(?err, "failed to create offer");
                parts.video.stop();
                parts.cursor_task.abort();
                if let Some(task) = parts.clipboard_task {
                    task.abort();
                }
                if let Some(task) = parts.elevation_task {
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

/// Handles an `AgentCommand` from outside the signaling loop (see that
/// type's doc comment). Mirrors the `SignalMessage::Bye` arm of
/// `handle_signal_message` above (same shutdown + status update), except
/// this end is the one telling the server, not the other way around --
/// hence sending `SignalMessage::Bye` here instead of just reacting to one.
async fn handle_agent_command(
    cmd: AgentCommand,
    pin: &str,
    status: &watch::Sender<AgentStatus>,
    out_tx: &mpsc::UnboundedSender<SignalMessage>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
    pending_auth: &mut Option<PendingAuth>,
) {
    match cmd {
        AgentCommand::EndSession => {
            let Some(old) = active.take() else {
                // Slice 3.2c: no `active` session yet, but the owner may
                // still be waiting on a client's OPAQUE login
                // (`pending_auth`) -- "End session" should give up on that
                // wait too, not just silently no-op, even though there's no
                // `ActiveSession` to shut down and nothing to change
                // `status` to (it was never anything but `Registered` while
                // only a login was pending).
                let Some(pending) = pending_auth.take() else {
                    tracing::debug!(
                        "EndSession command received with no active session or pending auth, ignoring"
                    );
                    return;
                };
                tracing::info!(session_id = %pending.session_id, "ending pending auth (End session command)");
                let _ = out_tx.send(SignalMessage::Bye {
                    session_id: pending.session_id,
                });
                return;
            };
            let session_id = current_session_id.take();
            tracing::info!(?session_id, "ending session (End session command)");
            old.shutdown().await;
            if let Some(session_id) = session_id {
                let _ = out_tx.send(SignalMessage::Bye { session_id });
            }
            let _ = status.send(AgentStatus::Registered {
                pin: pin.to_string(),
            });
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
    event_tx: &mpsc::Sender<SessionEvent>,
    active: &mut Option<ActiveSession>,
    current_session_id: &mut Option<String>,
    pending_auth: &mut Option<PendingAuth>,
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
        SessionEvent::ConnectionState { state, tag } => {
            // Fix to slice 3.5b: during automatic client reconnection (bye
            // -> `connect_device` -> `PeerJoined`, all while the old
            // `PeerConnection` is still tearing down), a state change from
            // the *old*, already superseded session can land here after a
            // new session has already taken `active`'s place -- e.g. its
            // `Closed` arriving right as the new session is starting up.
            // Acting on it would wrongly end the new session, so it's
            // dropped unless it's tagged with the session that's actually
            // current right now.
            if !active.as_ref().is_some_and(|a| a.session_tag == tag) {
                tracing::debug!(
                    ?state,
                    tag,
                    "connection state event from a superseded session, ignoring"
                );
                return;
            }
            tracing::info!(?state, "peer connection state changed");
            if state == RTCPeerConnectionState::Connected {
                // Slice 3.5b: also cancels any disconnect grace window in
                // progress -- the connection recovered on its own.
                if let Some(active) = active.as_mut() {
                    active.disconnect_grace.connected();
                    let _ = status.send(AgentStatus::InSession {
                        pin: pin.to_string(),
                    });
                }
            } else if state == RTCPeerConnectionState::Disconnected {
                // Slice 3.5b: `Disconnected` can be a brief ICE hiccup (a
                // Wi-Fi roam, a dropped packet train) that recovers to
                // `Connected` on its own -- unlike `Failed`/`Closed`, it
                // does not end the session right away. Arm a
                // `DISCONNECT_GRACE` window instead: `Connected` above
                // cancels it if it arrives first; otherwise
                // `DisconnectTimeout` below ends the session the same way
                // `Failed`/`Closed` do. Agent status deliberately stays
                // `InSession` for the whole window -- no flicker on a
                // hiccup that self-heals.
                if let (Some(active), Some(session_id)) =
                    (active.as_mut(), current_session_id.clone())
                {
                    if let Some(generation) = active.disconnect_grace.disconnected() {
                        let timer_tx = event_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(DISCONNECT_GRACE).await;
                            let _ = timer_tx
                                .send(SessionEvent::DisconnectTimeout {
                                    session_id,
                                    generation,
                                })
                                .await;
                        });
                    }
                }
            } else if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                if let Some(old) = active.take() {
                    old.shutdown().await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                // Slice 3.2a/D35: the session just ended on this end
                // (`Failed`/`Closed`), not via a `Bye` from the peer or an
                // `EndSession` command -- tell the server so it doesn't keep
                // the device marked busy for a session that's actually over.
                if let Some(session_id) = current_session_id.take() {
                    let _ = out_tx.send(SignalMessage::Bye { session_id });
                }
            }
        }
        SessionEvent::DisconnectTimeout {
            session_id,
            generation,
        } => {
            let matches_current = current_session_id.as_deref() == Some(session_id.as_str());
            let matches_pending = active
                .as_ref()
                .is_some_and(|a| a.disconnect_grace.is_current(generation));
            if matches_current && matches_pending {
                tracing::info!(%session_id, "disconnect grace window expired, ending session");
                if let Some(old) = active.take() {
                    old.shutdown().await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                // Slice 3.2a/D35: same reasoning as `Failed`/`Closed` above
                // -- the session ended locally, tell the server.
                if let Some(session_id) = current_session_id.take() {
                    let _ = out_tx.send(SignalMessage::Bye { session_id });
                }
            } else {
                tracing::debug!(%session_id, generation, "stale disconnect timeout, ignoring");
            }
        }
        // Slice 3.2c: a `PendingAuth` (armed on `AuthRequired`) waited out
        // `AUTH_TIMEOUT` without the client completing its login.
        SessionEvent::AuthTimeout {
            session_id,
            generation,
        } => {
            let matches_pending = pending_auth
                .as_ref()
                .is_some_and(|p| p.session_id == session_id && p.generation == generation);
            if matches_pending {
                tracing::info!(%session_id, "auth wait timed out, ending pending session");
                *pending_auth = None;
                let _ = out_tx.send(SignalMessage::Bye { session_id });
            } else {
                tracing::debug!(%session_id, generation, "stale auth timeout, ignoring");
            }
        }
        SessionEvent::DataChannelClosed { label, tag } => {
            if label != "control" {
                tracing::debug!(label, "non-control data channel closed");
            } else if active.as_ref().is_some_and(|a| a.session_tag == tag) {
                // Slice 3.5b: the `control` channel closing is a
                // deliberate end of session from the client (closing the
                // tab, `pc.close()`) -- end it immediately instead of
                // waiting for the `Disconnected` grace window. Guarded with
                // `active.take()` (a no-op if already `None`) since our own
                // `ActiveSession::shutdown` closing the peer connection also
                // closes this same channel and can deliver this event again
                // after the slot is already empty.
                if let Some(old) = active.take() {
                    tracing::info!("control data channel closed by remote peer, ending session");
                    old.shutdown().await;
                    let _ = status.send(AgentStatus::Registered {
                        pin: pin.to_string(),
                    });
                }
                // Slice 3.2a/D35: same reasoning as `Failed`/`Closed` above
                // -- the session ended locally, tell the server.
                if let Some(session_id) = current_session_id.take() {
                    let _ = out_tx.send(SignalMessage::Bye { session_id });
                }
            } else {
                // Fix to slice 3.5b: same reasoning as `ConnectionState`'s
                // tag check above -- this is the *old* session's `control`
                // channel finishing its close handshake after a new session
                // has already taken `active`'s place (e.g. during automatic
                // client reconnection), not the current session's.
                tracing::debug!(
                    label,
                    tag,
                    "control data channel closed event from a superseded session, ignoring"
                );
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
                        Ok(ControlMessage::SetQuality { preset }) => {
                            if let Some(active) = active.as_mut() {
                                active.quality_preset = preset;
                                if let Some(adapt_tx) = &active.video.adapt_tx {
                                    let _ = adapt_tx.send(Feedback::Preset { preset });
                                } else {
                                    tracing::debug!(
                                        label,
                                        ?preset,
                                        "set_quality received with adaptation disabled (--no-adapt), ignoring"
                                    );
                                }
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
                let host_info = host_info_message();
                match active.peer.send_control(&host_info).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!("control channel not open yet, dropped host info");
                    }
                    Err(err) => {
                        tracing::warn!(?err, "failed to send host info");
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

/// Builds the `ControlMessage::HostInfo` announcement (slice 3.5f), sent
/// alongside `displays_message` when the `control` channel opens.
fn host_info_message() -> ControlMessage {
    ControlMessage::HostInfo {
        os: crate::platform::host_os(),
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
            // The new pipeline's `Controller` (if any) always starts fresh
            // at `QualityPreset::Auto` -- re-apply the session's own choice
            // (slice 3.5e) so a display switch doesn't silently revert it.
            if active.quality_preset != QualityPreset::Auto {
                if let Some(adapt_tx) = &active.video.adapt_tx {
                    let _ = adapt_tx.send(Feedback::Preset {
                        preset: active.quality_preset,
                    });
                }
            }
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
    /// See `ActiveSession::elevation_task`.
    elevation_task: Option<tokio::task::JoinHandle<()>>,
}

/// Combines one session's ICE credentials with the extra `--stun` servers
/// baked into `HostContext::session` at startup, in the order the client has
/// always used: server-provided first, then CLI overrides.
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

/// Picks which ICE credentials to use for a new session: the joining peer's
/// own, freshly minted credentials from `PeerJoined` when the server sent
/// them, falling back to the host's `Registered` credentials otherwise (an
/// older server that predates 2.6b, whose `PeerJoined` has no `ice_servers`
/// field and deserializes it to an empty vector via `#[serde(default)]`).
/// A host that has been running for a while (2.6a: it reconnects and stays
/// up) would otherwise keep offering TURN creds that expired hours ago.
fn peer_ice_servers<'a>(
    from_peer_joined: &'a [proto::signal::IceServer],
    registered: &'a [proto::signal::IceServer],
) -> &'a [proto::signal::IceServer] {
    if from_peer_joined.is_empty() {
        registered
    } else {
        from_peer_joined
    }
}

/// Spawns the Windows foreground-window elevation watcher for one session
/// (slice 2.6e, see `start_session`'s call site for when this is skipped).
/// Polls `platform::elevation::foreground_input_blocked` every
/// `ELEVATION_POLL_INTERVAL`; `tokio::time::interval`'s first tick fires
/// immediately, which is exactly the "first poll" `InputBlockedWatcher`
/// expects. Aborted from `ActiveSession::shutdown` like `cursor_task`/
/// `clipboard_task`.
#[cfg(target_os = "windows")]
fn spawn_elevation_watcher(peer: Arc<PeerSession>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut watcher = InputBlockedWatcher::default();
        let mut ticker = tokio::time::interval(ELEVATION_POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let blocked = platform::elevation::foreground_input_blocked();
            if let Some(msg) = watcher.update(blocked) {
                tracing::info!(blocked, "foreground window elevation state changed");
                if let Err(err) = peer.send_control(&msg).await {
                    tracing::warn!(?err, "failed to send input_blocked control message");
                }
            }
        }
    })
}

/// Builds the video pipeline and the `PeerSession` for one joining peer, and
/// starts the task that feeds encoded frames from the pipeline into the
/// session's video track. `registered_ice_servers` are this session's own
/// ICE credentials, chosen by `peer_ice_servers` at the call site (see
/// `session_ice_servers`) -- despite the name, since 2.6b this is usually
/// the joining peer's `PeerJoined` credentials, not the host's `Registered`
/// ones.
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

    // Only worth watching when there's a real injector to worry about --
    // a view-only session (`input_available == false`) already tells the
    // client that up front via `InputStatus` and never attaches input at
    // all, so a UIPI warning on top would be noise. Windows only: UIPI
    // (and elevation generally) doesn't exist on macOS.
    #[cfg(target_os = "windows")]
    let elevation_task = input_available.then(|| spawn_elevation_watcher(Arc::clone(&peer)));
    #[cfg(not(target_os = "windows"))]
    let elevation_task: Option<tokio::task::JoinHandle<()>> = None;

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
        elevation_task,
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
        // Slice 3.2c: a fresh, per-test scratch directory under the system
        // temp dir that's never written to here, mirroring
        // `access::tests::scratch_dir`/`app::tests::scratch_device_store` --
        // no file at this path means `access_store.load()` is `None`, i.e.
        // "no password set", so every existing test built on `test_ctx()`
        // keeps behaving exactly as before this slice.
        access_store: crate::access::AccessStore::new(&std::env::temp_dir().join(format!(
            "rcdesk-host-signaling-test-ctx-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))),
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
    fn input_blocked_watcher_stays_quiet_on_a_first_unblocked_poll() {
        let mut watcher = InputBlockedWatcher::default();
        assert_eq!(watcher.update(false), None);
    }

    #[test]
    fn input_blocked_watcher_speaks_up_immediately_on_a_first_blocked_poll() {
        let mut watcher = InputBlockedWatcher::default();
        assert_eq!(
            watcher.update(true),
            Some(ControlMessage::InputBlocked {
                blocked: true,
                reason: Some(ELEVATED_INPUT_BLOCKED_REASON.to_string()),
            })
        );
    }

    #[test]
    fn input_blocked_watcher_stays_quiet_while_state_does_not_change() {
        let mut watcher = InputBlockedWatcher::default();
        assert_eq!(watcher.update(false), None);
        assert_eq!(watcher.update(false), None);

        assert!(watcher.update(true).is_some());
        assert_eq!(watcher.update(true), None);
    }

    #[test]
    fn input_blocked_watcher_reports_every_change_in_either_direction() {
        let mut watcher = InputBlockedWatcher::default();
        assert_eq!(watcher.update(false), None);

        assert_eq!(
            watcher.update(true),
            Some(ControlMessage::InputBlocked {
                blocked: true,
                reason: Some(ELEVATED_INPUT_BLOCKED_REASON.to_string()),
            })
        );
        assert_eq!(
            watcher.update(false),
            Some(ControlMessage::InputBlocked {
                blocked: false,
                reason: None,
            })
        );
        assert_eq!(
            watcher.update(true),
            Some(ControlMessage::InputBlocked {
                blocked: true,
                reason: Some(ELEVATED_INPUT_BLOCKED_REASON.to_string()),
            })
        );
    }

    #[test]
    fn peer_ice_servers_prefers_peer_joined_credentials_when_present() {
        fn server(url: &str) -> proto::signal::IceServer {
            proto::signal::IceServer {
                urls: vec![url.to_string()],
                username: None,
                credential: None,
            }
        }

        let from_peer_joined = vec![server("turn:fresh")];
        let registered = vec![server("turn:stale")];

        assert_eq!(
            peer_ice_servers(&from_peer_joined, &registered),
            &from_peer_joined[..]
        );
    }

    #[test]
    fn peer_ice_servers_falls_back_to_registered_when_peer_joined_is_empty() {
        fn server(url: &str) -> proto::signal::IceServer {
            proto::signal::IceServer {
                urls: vec![url.to_string()],
                username: None,
                credential: None,
            }
        }

        let from_peer_joined: Vec<proto::signal::IceServer> = vec![];
        let registered = vec![server("turn:stale")];

        assert_eq!(
            peer_ice_servers(&from_peer_joined, &registered),
            &registered[..]
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

    /// The `control` channel's opening announcements (slice 3.5f) include
    /// the host's own OS, matching `platform::host_os()` -- what the client
    /// uses to decide whether Cmd should act as Ctrl (`web/src/keyRemap.ts`).
    #[test]
    fn host_info_message_reports_this_hosts_os() {
        assert_eq!(
            host_info_message(),
            ControlMessage::HostInfo {
                os: crate::platform::host_os()
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
        if let Some(task) = parts.elevation_task {
            task.abort();
        }
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
        if let Some(task) = parts.elevation_task {
            task.abort();
        }
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
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
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

    /// A `test_ctx()` with an access password registered under a fresh
    /// scratch directory (slice 3.2c) -- mirrors
    /// `access::tests::scratch_dir`/`app::tests::scratch_device_store`: a
    /// unique name is enough, `tempfile` is not an approved dependency.
    /// Unlike `test_ctx()` itself, this one *does* write a file (the
    /// `access::register`ed record), so every caller needs its own
    /// directory -- hence the `name` parameter.
    fn test_ctx_with_password(name: &str, password: &str) -> HostContext {
        let mut ctx = test_ctx();
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-host-signaling-test-ctx-pw-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let store = crate::access::AccessStore::new(&dir);
        store
            .save(&crate::access::register(password).expect("opaque registration"))
            .expect("save access record");
        ctx.access_store = store;
        ctx
    }

    /// Slice 3.2c: `PeerJoined` on a host with a password set must ask the
    /// client to authenticate (`AuthRequired`) instead of offering a
    /// session right away.
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_joined_with_a_password_asks_for_auth_and_does_not_offer() {
        let ctx = test_ctx_with_password("asks-for-auth", "correct horse battery staple");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;

        assert_eq!(
            out_rx.try_recv().expect("expected auth_required"),
            SignalMessage::AuthRequired {
                session_id: "sess-1".to_string()
            }
        );
        assert!(
            out_rx.try_recv().is_err(),
            "no Offer must be sent while waiting for auth"
        );
        assert!(active.is_none());
        assert!(pending_auth.is_some());
    }

    /// Slice 3.2c: a full, real OPAQUE login (client side via `opaque_ke`
    /// directly, same as `access::tests`) through `AuthRequired`/
    /// `PakeStart`/`PakeResponse`/`PakeFinish` ends with the host offering
    /// a session, exactly as the no-password path would have.
    #[tokio::test(flavor = "multi_thread")]
    async fn full_opaque_login_then_offer() {
        use opaque_ke::{
            ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers,
        };
        use rand_core::OsRng;

        let password = "correct horse battery staple";
        let ctx = test_ctx_with_password("full-login", password);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        assert_eq!(
            out_rx.try_recv().expect("expected auth_required"),
            SignalMessage::AuthRequired {
                session_id: "sess-1".to_string()
            }
        );

        let mut rng = OsRng;
        let client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
                .expect("client login start");
        let request_payload = BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize());

        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: request_payload,
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let response_payload = match out_rx.try_recv().expect("expected pake_response") {
            SignalMessage::PakeResponse { payload, .. } => payload,
            other => panic!("expected pake_response, got {other:?}"),
        };
        let response_bytes = BASE64_URL_SAFE_NO_PAD
            .decode(response_payload)
            .expect("decode pake_response payload");
        let response =
            CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(&response_bytes)
                .expect("deserialize credential response");

        let ksf = crate::access::CustomKsf::default();
        let client_finish = client_start
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                response,
                ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
            )
            .expect("client login finish");
        let finish_payload = BASE64_URL_SAFE_NO_PAD.encode(client_finish.message.serialize());

        handle_signal_message(
            SignalMessage::PakeFinish {
                session_id: "sess-1".to_string(),
                payload: finish_payload,
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;

        match out_rx.try_recv().expect("expected offer") {
            SignalMessage::Offer {
                session_id,
                sdp,
                auth,
            } => {
                assert_eq!(session_id, "sess-1");
                // Slice 3.2e: this session was gated by a password, so the
                // offer must carry a dtls-binding tag over its own
                // fingerprint, verifiable with the *client's* copy of the
                // session key (`client_finish.session_key`) -- the two
                // sides derive the same key from a real OPAQUE login, same
                // as `access::tests::register_then_login_with_the_same_password_yields_equal_session_keys`.
                let tag = auth.expect("offer must carry a dtls-binding tag when a password is set");
                let fingerprint = crate::dtls_bind::fingerprint_from_sdp(&sdp)
                    .expect("real webrtc-rs offer sdp must have a dtls fingerprint");
                assert!(crate::dtls_bind::verify_auth_tag(
                    &client_finish.session_key[..],
                    "offer",
                    &fingerprint,
                    &tag,
                ));
            }
            other => panic!("expected offer, got {other:?}"),
        }
        assert!(active.is_some(), "session must have started successfully");
        assert_eq!(auth_failures, 0);
        assert!(pending_auth.is_none());

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Runs a full real OPAQUE login (same steps as `full_opaque_login_then_offer`)
    /// up through the host's `Offer`, for the two `Answer`-side dtls-binding
    /// tests below (slice 3.2e), which only care about what happens next.
    /// Returns the context (so the session stays valid for `handle_signal_message`
    /// calls with it), the loop state after the `Offer`, and the client's
    /// session key -- the client's own proof it holds the same key the host
    /// does, exactly as a real browser client would after `finishLogin`.
    #[allow(clippy::type_complexity)]
    async fn logged_in_awaiting_answer(
        name: &str,
    ) -> (
        HostContext,
        Option<ActiveSession>,
        Option<String>,
        mpsc::UnboundedSender<SignalMessage>,
        mpsc::UnboundedReceiver<SignalMessage>,
        mpsc::Sender<SessionEvent>,
        watch::Sender<AgentStatus>,
        Vec<u8>,
    ) {
        use opaque_ke::{
            ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers,
        };
        use rand_core::OsRng;

        let password = "correct horse battery staple";
        let ctx = test_ctx_with_password(name, password);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required

        let mut rng = OsRng;
        let client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
                .expect("client login start");
        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let response_payload = match out_rx.try_recv().expect("expected pake_response") {
            SignalMessage::PakeResponse { payload, .. } => payload,
            other => panic!("expected pake_response, got {other:?}"),
        };
        let response = CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(
            &BASE64_URL_SAFE_NO_PAD
                .decode(response_payload)
                .expect("decode pake_response payload"),
        )
        .expect("deserialize credential response");
        let ksf = crate::access::CustomKsf::default();
        let client_finish = client_start
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                response,
                ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
            )
            .expect("client login finish");
        let session_key = client_finish.session_key[..].to_vec();

        handle_signal_message(
            SignalMessage::PakeFinish {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(client_finish.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // the Offer
        assert!(active.is_some(), "session must have started successfully");

        (
            ctx,
            active,
            current_session_id,
            out_tx,
            out_rx,
            event_tx,
            status_tx,
            session_key,
        )
    }

    /// Slice 3.2e: an `Answer` with no `auth` at all, while the session was
    /// gated by a password, must be rejected -- the host ends the session
    /// (`Bye`) instead of applying an unbound answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn answer_without_auth_ends_session_when_a_password_is_set() {
        let (ctx, mut active, mut current_session_id, out_tx, mut out_rx, event_tx, status_tx, _) =
            logged_in_awaiting_answer("answer-no-auth").await;

        handle_signal_message(
            SignalMessage::Answer {
                session_id: "sess-1".to_string(),
                sdp: "v=0\r\na=fingerprint:sha-256 AA:BB:CC\r\n".to_string(),
                auth: None,
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;

        assert_eq!(
            out_rx.try_recv().expect("expected bye"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string()
            }
        );
        assert!(active.is_none(), "session must have been ended");
        assert_eq!(current_session_id, None);
    }

    /// Slice 3.2e: an `Answer` whose `auth` tag verifies against the
    /// client's session key and the answer's own fingerprint must *not* be
    /// rejected -- the host goes on to try `set_answer` (which fails here,
    /// since this is a synthetic SDP rather than a real DTLS answer;
    /// `set_answer`'s failure path is untouched by this slice and only
    /// `tracing::warn!`s, it does not end the session -- see the `Answer`
    /// arm's non-3.2e code below the dtls-binding check).
    #[tokio::test(flavor = "multi_thread")]
    async fn answer_with_a_valid_auth_tag_is_not_rejected() {
        let (
            ctx,
            mut active,
            mut current_session_id,
            out_tx,
            mut out_rx,
            event_tx,
            status_tx,
            session_key,
        ) = logged_in_awaiting_answer("answer-valid-auth").await;

        let sdp = "v=0\r\na=fingerprint:sha-256 AA:BB:CC\r\n".to_string();
        let fingerprint =
            crate::dtls_bind::fingerprint_from_sdp(&sdp).expect("sdp has a fingerprint");
        let tag = crate::dtls_bind::auth_tag(&session_key, "answer", &fingerprint);

        handle_signal_message(
            SignalMessage::Answer {
                session_id: "sess-1".to_string(),
                sdp,
                auth: Some(tag),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;

        assert!(
            out_rx.try_recv().is_err(),
            "a valid auth tag must not trigger a Bye"
        );
        assert!(active.is_some(), "session must not have been ended");
        assert_eq!(current_session_id.as_deref(), Some("sess-1"));

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.2c: a client that gets its password wrong fails locally
    /// (`ClientLogin::finish`, same as `access::tests::login_with_a_wrong_password_fails`)
    /// and never even produces a `PakeFinish` to send -- the host's wait
    /// must survive that (no `Offer`, `pending_auth` still armed), and a
    /// second, correct attempt afterwards must still succeed.
    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_password_client_finish_fails_and_host_stays_pending() {
        use opaque_ke::{
            ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers,
        };
        use rand_core::OsRng;

        let password = "correct horse battery staple";
        let ctx = test_ctx_with_password("wrong-then-right", password);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required

        // First, wrong-password attempt: the host's `PakeStart` handling
        // can't tell it's wrong yet (OPAQUE's OPRF step is password-blind),
        // so it still answers `PakeResponse` -- the mismatch is only ever
        // caught client-side, in `ClientLogin::finish` below.
        let mut rng = OsRng;
        let wrong_client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, b"wrong password")
                .expect("client login start");
        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(wrong_client_start.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let wrong_response_payload = match out_rx.try_recv().expect("expected pake_response") {
            SignalMessage::PakeResponse { payload, .. } => payload,
            other => panic!("expected pake_response, got {other:?}"),
        };
        let wrong_response = CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(
            &BASE64_URL_SAFE_NO_PAD
                .decode(wrong_response_payload)
                .expect("decode pake_response payload"),
        )
        .expect("deserialize credential response");
        let ksf = crate::access::CustomKsf::default();
        let wrong_finish_result = wrong_client_start.state.finish(
            &mut rng,
            b"wrong password",
            wrong_response,
            ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
        );
        assert!(
            wrong_finish_result.is_err(),
            "a wrong password must fail on the client side"
        );

        // No `PakeFinish` was ever sent (there is nothing valid to send),
        // so the host must still be waiting, with no `Offer` sent.
        assert!(out_rx.try_recv().is_err());
        assert!(active.is_none());
        assert!(pending_auth.is_some());

        // A second, correct attempt must still succeed.
        let right_client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
                .expect("client login start");
        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(right_client_start.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let right_response_payload = match out_rx.try_recv().expect("expected pake_response") {
            SignalMessage::PakeResponse { payload, .. } => payload,
            other => panic!("expected pake_response, got {other:?}"),
        };
        let right_response = CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(
            &BASE64_URL_SAFE_NO_PAD
                .decode(right_response_payload)
                .expect("decode pake_response payload"),
        )
        .expect("deserialize credential response");
        let right_client_finish = right_client_start
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                right_response,
                ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
            )
            .expect("client login finish");

        handle_signal_message(
            SignalMessage::PakeFinish {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(right_client_finish.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;

        match out_rx.try_recv().expect("expected offer") {
            SignalMessage::Offer { session_id, .. } => assert_eq!(session_id, "sess-1"),
            other => panic!("expected offer, got {other:?}"),
        }
        assert!(active.is_some(), "session must have started successfully");

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.2c: a `PakeFinish` that isn't a valid OPAQUE finalization
    /// (garbage payload, not this attacker's actual login attempt) is
    /// rejected with `AuthFailed { retry_after_secs: None }`.
    #[tokio::test(flavor = "multi_thread")]
    async fn garbage_finalization_yields_auth_failed() {
        use opaque_ke::ClientLogin;
        use rand_core::OsRng;

        let password = "correct horse battery staple";
        let ctx = test_ctx_with_password("garbage-finalization", password);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required

        let mut rng = OsRng;
        let client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
                .expect("client login start");
        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // pake_response
                                   // `login` is `Some` now -- a `PakeFinish` matches and is attempted,
                                   // but its payload isn't even valid base64.
        handle_signal_message(
            SignalMessage::PakeFinish {
                session_id: "sess-1".to_string(),
                payload: "not valid base64 at all!!".to_string(),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;

        assert_eq!(
            out_rx.try_recv().expect("expected auth_failed"),
            SignalMessage::AuthFailed {
                session_id: "sess-1".to_string(),
                retry_after_secs: None,
            }
        );
        assert!(active.is_none());
        assert!(
            pending_auth.is_some(),
            "the wait stays armed for a fresh PakeStart"
        );
    }

    /// Fix to slice 3.2c: this is throttling, not a hard lockout -- six
    /// consecutive `PakeStart`s (even ones that fail to finish) must all
    /// still run and get a `PakeResponse`; only a *seventh*, sent right
    /// after the sixth crossed `MAX_AUTH_ATTEMPTS`, is throttled. The
    /// pre-fix behavior refused the sixth attempt itself, which meant the
    /// owner could never log in again after five failures -- every later
    /// attempt re-armed its own lockout before ever reaching `login_start`.
    #[tokio::test(flavor = "multi_thread")]
    async fn six_pake_starts_in_a_row_throttle_the_host() {
        use opaque_ke::ClientLogin;
        use rand_core::OsRng;

        let password = "correct horse battery staple";
        let ctx = test_ctx_with_password("six-in-a-row", password);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required

        // A structurally valid `CredentialRequest` every time (a fresh
        // client start per attempt, same password) -- `login_start` must
        // succeed and answer `PakeResponse` for all six; only `PakeFinish`
        // is ever what actually completes a login, so none of these six
        // resolves the wait.
        let mut rng = OsRng;
        for attempt in 1..=6u32 {
            let client_start = ClientLogin::<crate::access::RcdeskCipherSuite>::start(
                &mut rng,
                password.as_bytes(),
            )
            .expect("client login start");
            handle_signal_message(
                SignalMessage::PakeStart {
                    session_id: "sess-1".to_string(),
                    payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
                },
                &ctx,
                &[],
                "111111",
                &status_tx,
                &out_tx,
                event_tx.clone(),
                &mut active,
                &mut current_session_id,
                &mut pending_auth,
                &mut auth_failures,
                &mut auth_locked_until,
            )
            .await;
            match out_rx.try_recv() {
                Ok(SignalMessage::PakeResponse { session_id, .. }) => {
                    assert_eq!(session_id, "sess-1")
                }
                other => panic!("attempt {attempt} expected pake_response, got {other:?}"),
            }
        }
        assert_eq!(auth_failures, 6);
        assert!(
            auth_locked_until.is_some(),
            "the sixth attempt must arm a throttle for the next one"
        );

        // Seventh, right away: throttled -- no `PakeResponse`, no further
        // increment.
        let client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
                .expect("client login start");
        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        assert_eq!(
            out_rx.try_recv().unwrap(),
            SignalMessage::AuthFailed {
                session_id: "sess-1".to_string(),
                retry_after_secs: Some(5),
            }
        );
        assert_eq!(
            auth_failures, 6,
            "a throttled attempt must not increment the counter"
        );
    }

    /// Fix to slice 3.2c: once the throttle armed by a run of failures has
    /// expired, the next attempt runs normally -- and, if it's the owner's
    /// actual password this time, a full login still succeeds and resets
    /// both `auth_failures` and `auth_locked_until`, proving the owner can
    /// always eventually get back in.
    #[tokio::test(flavor = "multi_thread")]
    async fn login_after_expired_throttle_succeeds_and_resets_the_counter() {
        use opaque_ke::{
            ClientLogin, ClientLoginFinishParameters, CredentialResponse, Identifiers,
        };
        use rand_core::OsRng;

        let password = "correct horse battery staple";
        let ctx = test_ctx_with_password("after-expired-throttle", password);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required

        // Simulate six prior failed attempts whose throttle has since
        // expired (set directly in the slot, same as the real timer would
        // leave it once `Instant::now()` passes it).
        auth_failures = 6;
        auth_locked_until = Some(Instant::now() - Duration::from_secs(1));

        let mut rng = OsRng;
        let client_start =
            ClientLogin::<crate::access::RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
                .expect("client login start");
        handle_signal_message(
            SignalMessage::PakeStart {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(client_start.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let response_payload = match out_rx.try_recv().expect("expected pake_response") {
            SignalMessage::PakeResponse { payload, .. } => payload,
            other => panic!("expected pake_response, got {other:?}"),
        };
        assert_eq!(
            auth_failures, 7,
            "the expired-throttle attempt still counts and re-arms a longer throttle"
        );

        let response_bytes = BASE64_URL_SAFE_NO_PAD
            .decode(response_payload)
            .expect("decode pake_response payload");
        let response =
            CredentialResponse::<crate::access::RcdeskCipherSuite>::deserialize(&response_bytes)
                .expect("deserialize credential response");
        let ksf = crate::access::CustomKsf::default();
        let client_finish = client_start
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                response,
                ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
            )
            .expect("client login finish");

        handle_signal_message(
            SignalMessage::PakeFinish {
                session_id: "sess-1".to_string(),
                payload: BASE64_URL_SAFE_NO_PAD.encode(client_finish.message.serialize()),
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;

        match out_rx.try_recv().expect("expected offer") {
            SignalMessage::Offer { session_id, .. } => assert_eq!(session_id, "sess-1"),
            other => panic!("expected offer, got {other:?}"),
        }
        assert!(active.is_some(), "session must have started successfully");
        assert_eq!(
            auth_failures, 0,
            "a successful login resets the failure counter"
        );
        assert!(
            auth_locked_until.is_none(),
            "a successful login clears the throttle too"
        );

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.2c: `Bye` for the session a `PendingAuth` is waiting on
    /// clears the wait (there's no `active` session to shut down).
    #[tokio::test(flavor = "multi_thread")]
    async fn bye_for_a_pending_session_clears_it() {
        let ctx = test_ctx_with_password("bye-clears", "correct horse battery staple");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required
        assert!(pending_auth.is_some());

        handle_signal_message(
            SignalMessage::Bye {
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
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;

        assert!(pending_auth.is_none());
        assert!(active.is_none());
    }

    /// Slice 3.2c: `SessionEvent::AuthTimeout` for the wait currently
    /// pending ends it with a `Bye`; one for a generation that's already
    /// been superseded is ignored.
    #[tokio::test(flavor = "multi_thread")]
    async fn auth_timeout_sends_bye() {
        let ctx = test_ctx_with_password("auth-timeout", "correct horse battery staple");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel::<SessionEvent>(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required
        let generation = pending_auth.as_ref().expect("armed above").generation;

        // A stale generation (not the one currently pending) is ignored.
        handle_session_event(
            SessionEvent::AuthTimeout {
                session_id: "sess-1".to_string(),
                generation: generation + 1,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
        )
        .await;
        assert!(pending_auth.is_some());
        assert!(out_rx.try_recv().is_err());

        // The current generation ends the wait with a `Bye`.
        handle_session_event(
            SessionEvent::AuthTimeout {
                session_id: "sess-1".to_string(),
                generation,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
        )
        .await;
        assert!(pending_auth.is_none());
        assert_eq!(
            out_rx.try_recv().unwrap(),
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
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
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
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert_eq!(fake_clipboard.set_calls(), vec!["from client".to_string()]);

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// `ControlMessage::SetQuality` arriving on the `control` channel must
    /// reach the session's adaptation controller (slice 3.5e) -- proven
    /// through the real `run_adapt_task` and its 1s ticker (`adapt::TICK`),
    /// the same harness as `clipboard_text_on_input_channel_is_applied_directly_not_routed`
    /// (no real WebRTC peer needed). There's no open data channel to read
    /// `ControlMessage::Quality` back off of, so the effect is observed
    /// through `VideoPipeline`'s test-only `rate_control` handle instead
    /// (`run_adapt_task` calls `rate_control.set()` on every `Decision`).
    #[tokio::test(flavor = "multi_thread")]
    async fn set_quality_reaches_the_adapt_controller() {
        let mut ctx = test_ctx();
        ctx.adapt = true;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;
        assert!(active.is_some(), "session must have started successfully");
        let _ = out_rx.try_recv(); // the Offer

        assert_eq!(
            active.as_ref().unwrap().video.rate_control.fps(),
            30,
            "starts pinned to the session's configured fps"
        );

        let msg = ControlMessage::SetQuality {
            preset: QualityPreset::Sharp,
        };
        let data = serde_json::to_vec(&msg).unwrap();
        handle_session_event(
            SessionEvent::DataChannelMessage {
                label: "control".to_string(),
                data: data.into(),
                is_string: true,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert_eq!(
            active.as_ref().unwrap().quality_preset,
            QualityPreset::Sharp,
            "the session remembers the chosen preset"
        );

        // `run_adapt_task` only applies a `Decision` to `RateControl` on its
        // own 1s ticker (see that function's doc comment) -- wait a couple
        // of ticks rather than racing it.
        // `<=`, not `==`: on a slow CI runner the software encoder can't keep
        // up and the controller legitimately steps below the 15 fps ceiling
        // within the same wait (seen on ubuntu-latest: 12) -- what matters
        // here is that the ceiling arrived, not where overload left fps.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let fps = active.as_ref().unwrap().video.rate_control.fps();
        assert!(
            fps <= 15,
            "sharp's fps ceiling must have reached the controller and been applied, got {fps}"
        );

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// `AgentCommand::EndSession` with an active session must tell the
    /// server (`Bye`), tear the session down and report `Registered` again
    /// -- the tray agent's "End session" menu item (slice 2.6c).
    #[tokio::test(flavor = "multi_thread")]
    async fn end_session_command_sends_bye_and_reports_registered() {
        let ctx = test_ctx();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let (status_tx, status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;
        assert!(active.is_some(), "session must have started successfully");
        let _ = out_rx.try_recv(); // the Offer

        handle_agent_command(
            AgentCommand::EndSession,
            "111111",
            &status_tx,
            &out_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
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
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::Registered {
                pin: "111111".to_string()
            }
        );
    }

    /// `AgentCommand::EndSession` with no active session and no pending auth
    /// is a no-op: no `Bye`, no status change.
    #[tokio::test(flavor = "multi_thread")]
    async fn end_session_command_with_no_active_session_is_a_no_op() {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let (status_tx, status_rx) = watch::channel(AgentStatus::Registered {
            pin: "111111".to_string(),
        });

        handle_agent_command(
            AgentCommand::EndSession,
            "111111",
            &status_tx,
            &out_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(out_rx.try_recv().is_err(), "no Bye should have been sent");
        assert!(
            !status_rx.has_changed().unwrap(),
            "status must not change when there was no active session"
        );
    }

    /// Fix to slice 3.2c: `AgentCommand::EndSession` while a client is
    /// still waiting on `AuthRequired` (no `ActiveSession` yet, but a
    /// `PendingAuth` is armed) must give up on that wait too -- not just
    /// silently no-op, as it used to before this fix -- clearing
    /// `pending_auth` and telling the server `Bye` for its session id.
    /// Status is not touched: it was never anything but `Registered` while
    /// only a login was pending.
    #[tokio::test(flavor = "multi_thread")]
    async fn end_session_command_clears_pending_auth_and_sends_bye() {
        let ctx =
            test_ctx_with_password("end-session-clears-pending", "correct horse battery staple");
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let mut pending_auth: Option<PendingAuth> = None;
        let mut auth_failures = 0u32;
        let mut auth_locked_until: Option<Instant> = None;
        let (status_tx, status_rx) = watch::channel(AgentStatus::Registered {
            pin: "111111".to_string(),
        });

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
            &mut auth_failures,
            &mut auth_locked_until,
        )
        .await;
        let _ = out_rx.try_recv(); // auth_required
        assert!(pending_auth.is_some());

        handle_agent_command(
            AgentCommand::EndSession,
            "111111",
            &status_tx,
            &out_tx,
            &mut active,
            &mut current_session_id,
            &mut pending_auth,
        )
        .await;

        assert!(pending_auth.is_none());
        assert_eq!(
            out_rx.try_recv().expect("expected a Bye message"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string()
            }
        );
        assert!(
            !status_rx.has_changed().unwrap(),
            "status must not change -- it was already Registered"
        );
    }

    /// Slice 3.5a: `run` must not shut an active session down when it
    /// returns -- a P2P session outlives the signaling connection that set
    /// it up. Proven end to end against a fake server: register, trigger a
    /// session via `PeerJoined` (real `start_session`/`PeerSession`, same as
    /// `peer_joined_sends_bye_when_session_start_fails` above), wait for the
    /// host's `Offer` as proof it started, then drop the connection. `run`
    /// must return an error, and the slot -- the observable way to check
    /// this without reaching into `ActiveSession`'s private internals --
    /// must still report a live session.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_leaves_the_slot_session_active_when_the_connection_drops() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // Hello
            let _ = ws.next().await; // HostRegister
            let registered = serde_json::to_string(&SignalMessage::Registered {
                host_id: "host-1".to_string(),
                pin: "111111".to_string(),
                ice_servers: vec![],
                device: None,
            })
            .unwrap();
            ws.send(Message::text(registered)).await.unwrap();

            let peer_joined = serde_json::to_string(&SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            })
            .unwrap();
            ws.send(Message::text(peer_joined)).await.unwrap();

            // Wait for the host's Offer (proof the session actually
            // started) before dropping the connection out from under it.
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(SignalMessage::Offer { .. }) =
                            serde_json::from_str::<SignalMessage>(&text)
                        {
                            break;
                        }
                    }
                    other => panic!("connection ended before an Offer arrived: {other:?}"),
                }
            }
            drop(ws);
        });

        let client = SignalingClient::connect(&url, "test-host", None, None)
            .await
            .expect("connect");
        let ctx = test_ctx();
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();
        let mut slot = SessionSlot::new();

        let result = client
            .run(
                &ctx,
                &status_tx,
                Keepalive::default(),
                &mut cmd_rx,
                &mut slot,
            )
            .await;
        assert!(
            result.is_err(),
            "run must return once the server drops the connection"
        );

        assert!(
            slot.is_active(),
            "the P2P session must survive the signaling connection dropping"
        );

        if let Some(active) = slot.active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.2a/D35: `connect` puts the `session_id` argument into
    /// `HostRegister.session_id` -- `Some` when the caller (`app::run_agent`,
    /// via `SessionSlot::current_session_id`) has a live session from before
    /// this reconnect, `None` when it doesn't.
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_puts_the_session_id_argument_into_host_register() {
        async fn fake_server_expecting_session_id(
            listener: tokio::net::TcpListener,
            expected: Option<String>,
        ) {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // Hello
            let host_register = match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    serde_json::from_str::<SignalMessage>(&text).expect("valid HostRegister json")
                }
                other => panic!("expected HostRegister, got {other:?}"),
            };
            match host_register {
                SignalMessage::HostRegister { session_id, .. } => {
                    assert_eq!(session_id, expected);
                }
                other => panic!("expected host_register, got {other:?}"),
            }
            let registered = serde_json::to_string(&SignalMessage::Registered {
                host_id: "host-1".to_string(),
                pin: "111111".to_string(),
                ice_servers: vec![],
                device: None,
            })
            .unwrap();
            ws.send(Message::text(registered)).await.unwrap();
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");
        let server = tokio::spawn(fake_server_expecting_session_id(
            listener,
            Some("s-1".to_string()),
        ));
        let _client = SignalingClient::connect(&url, "test-host", None, Some("s-1".to_string()))
            .await
            .expect("connect");
        server.await.expect("server task");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");
        let server = tokio::spawn(fake_server_expecting_session_id(listener, None));
        let _client = SignalingClient::connect(&url, "test-host", None, None)
            .await
            .expect("connect");
        server.await.expect("server task");
    }

    /// Slice 3.5a: `SessionSlot::run_while_offline` must still drain and act
    /// on session events while there's no live signaling connection --
    /// otherwise input arriving over data channels would stall (the event
    /// channel is bounded) and e.g. the peer connection failing would never
    /// free the slot. Exercised directly on the slot: start a session, feed
    /// it a `ConnectionState(Closed)` event through the slot's own
    /// `event_tx` (the same channel `PeerSession` reports through in
    /// production), and confirm `run_while_offline` frees the slot before
    /// the passed-in future resolves.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_while_offline_still_frees_the_slot_on_connection_state_closed() {
        let ctx = test_ctx();
        let mut slot = SessionSlot::new();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            slot.event_tx.clone(),
            &mut slot.active,
            &mut slot.current_session_id,
            &mut slot.pending_auth,
            &mut slot.auth_failures,
            &mut slot.auth_locked_until,
        )
        .await;
        assert!(slot.is_active(), "session must have started successfully");
        let _ = out_rx.try_recv(); // the Offer

        let tag = slot.active.as_ref().unwrap().session_tag;
        slot.event_tx
            .send(SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Closed,
                tag,
            })
            .await
            .expect("the slot's own event channel must accept a send");

        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCommand>();
        slot.run_while_offline(&ctx, &mut cmd_rx, async {
            // Give the offline select loop a turn to drain the event above
            // before the wrapped future resolves.
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;

        assert!(
            !slot.is_active(),
            "a ConnectionState(Closed) event must free the slot even while offline"
        );
    }

    /// Starts a real session (via `PeerJoined`, same as the other tests in
    /// this module) and returns the pieces `handle_session_event` needs, for
    /// the slice 3.5b `Disconnected`/`DisconnectTimeout`/`DataChannelClosed`
    /// tests below.
    async fn started_session(
        session_id: &str,
    ) -> (
        HostContext,
        Option<ActiveSession>,
        Option<String>,
        mpsc::UnboundedSender<SignalMessage>,
        mpsc::UnboundedReceiver<SignalMessage>,
        mpsc::Sender<SessionEvent>,
        watch::Sender<AgentStatus>,
        watch::Receiver<AgentStatus>,
    ) {
        let ctx = test_ctx();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let (status_tx, status_rx) = watch::channel(AgentStatus::Connecting);

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: session_id.to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;
        assert!(active.is_some(), "session must have started successfully");

        (
            ctx,
            active,
            current_session_id,
            out_tx,
            out_rx,
            event_tx,
            status_tx,
            status_rx,
        )
    }

    /// Slice 3.5b: `ConnectionState(Disconnected)` must not end the session
    /// right away -- only arm the `DISCONNECT_GRACE` window.
    #[tokio::test(flavor = "multi_thread")]
    async fn disconnected_does_not_end_the_session_immediately() {
        let (ctx, mut active, mut current_session_id, out_tx, _out_rx, event_tx, status_tx, _) =
            started_session("sess-1").await;

        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Disconnected,
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(
            active.is_some(),
            "Disconnected alone must not end the session"
        );
        assert_eq!(current_session_id.as_deref(), Some("sess-1"));

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.5b: `Connected` arriving while a disconnect grace window is
    /// running cancels it -- the session survives and status goes back to
    /// `InSession`.
    #[tokio::test(flavor = "multi_thread")]
    async fn connected_within_the_grace_window_cancels_it() {
        let (
            ctx,
            mut active,
            mut current_session_id,
            out_tx,
            _out_rx,
            event_tx,
            status_tx,
            status_rx,
        ) = started_session("sess-1").await;

        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Disconnected,
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;
        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Connected,
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(active.is_some(), "the session must survive");
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::InSession {
                pin: "111111".to_string()
            }
        );

        // The now-cancelled window's `DisconnectTimeout` must be ignored --
        // proven directly in `disconnect_grace_tests`; here it's enough to
        // check `pending_disconnect`'s observable effect via `is_current`.
        assert!(!active.as_ref().unwrap().disconnect_grace.is_current(1));

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.5b: a `DisconnectTimeout` matching the currently pending
    /// grace window ends the session, the same way `Failed`/`Closed` do.
    /// The real `DISCONNECT_GRACE` timer isn't waited out here (this
    /// workspace doesn't enable tokio's `test-util` feature needed to pause
    /// the clock) -- the event is injected directly, exercising exactly the
    /// same handler code the real timer's `send` would reach. Slice
    /// 3.2a/D35: also checks that ending the session this way sends the
    /// server a `Bye`, so it doesn't keep the device marked busy.
    #[tokio::test(flavor = "multi_thread")]
    async fn disconnect_timeout_with_matching_generation_ends_the_session() {
        let (
            ctx,
            mut active,
            mut current_session_id,
            out_tx,
            mut out_rx,
            event_tx,
            status_tx,
            status_rx,
        ) = started_session("sess-1").await;
        // Slice 3.2e: `started_session` uses `test_ctx()`, which has no
        // access password set -- the offer must carry no dtls-binding tag,
        // same behavior as before this slice.
        match out_rx.try_recv().expect("expected the Offer") {
            SignalMessage::Offer { auth, .. } => assert_eq!(auth, None),
            other => panic!("expected offer, got {other:?}"),
        }

        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Disconnected,
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;
        handle_session_event(
            SessionEvent::DisconnectTimeout {
                session_id: "sess-1".to_string(),
                generation: 1,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(
            active.is_none(),
            "the grace window expiring must end the session"
        );
        assert_eq!(current_session_id, None);
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::Registered {
                pin: "111111".to_string()
            }
        );
        assert_eq!(
            out_rx.try_recv().expect("expected a Bye message"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string()
            }
        );
    }

    /// Slice 3.5b: a `DisconnectTimeout` from a window a `Connected` already
    /// cancelled must be ignored -- it's stale.
    #[tokio::test(flavor = "multi_thread")]
    async fn disconnect_timeout_from_a_cancelled_window_is_ignored() {
        let (ctx, mut active, mut current_session_id, out_tx, _out_rx, event_tx, status_tx, _) =
            started_session("sess-1").await;

        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Disconnected,
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;
        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Connected,
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;
        handle_session_event(
            SessionEvent::DisconnectTimeout {
                session_id: "sess-1".to_string(),
                generation: 1,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(
            active.is_some(),
            "a stale DisconnectTimeout must not end the session"
        );

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.5b: a `DisconnectTimeout` naming a session that's no longer
    /// current (a new one has since started) must not touch the new
    /// session.
    #[tokio::test(flavor = "multi_thread")]
    async fn disconnect_timeout_from_an_old_session_does_not_touch_a_new_one() {
        let (ctx, mut active, mut current_session_id, out_tx, mut out_rx, event_tx, status_tx, _) =
            started_session("sess-1").await;

        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Disconnected,
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        // A brand new session replaces the (still disconnected) old one --
        // mirrors `new PeerJoined while a session was active` in
        // `handle_signal_message`.
        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-2".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;
        assert_eq!(current_session_id.as_deref(), Some("sess-2"));
        let _ = out_rx.try_recv(); // sess-1's Offer
        let _ = out_rx.try_recv(); // sess-2's Offer

        // sess-1's stale timeout arrives after sess-2 has already started.
        handle_session_event(
            SessionEvent::DisconnectTimeout {
                session_id: "sess-1".to_string(),
                generation: 1,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(
            active.is_some(),
            "an old session's timeout must not end the new session"
        );
        assert_eq!(current_session_id.as_deref(), Some("sess-2"));

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Fix to slice 3.5b: a `ConnectionState` event tagged with an old,
    /// already-superseded session's tag must not touch the new one -- the
    /// scenario this guards against is automatic client reconnection (bye ->
    /// `connect_device` -> `PeerJoined`, all while the old `PeerConnection`
    /// is still tearing down): its `Closed` can land on the shared event
    /// channel after a new session has already taken `active`'s place.
    #[tokio::test(flavor = "multi_thread")]
    async fn connection_state_from_an_old_session_does_not_touch_a_new_one() {
        let (
            ctx,
            mut active,
            mut current_session_id,
            out_tx,
            mut out_rx,
            event_tx,
            status_tx,
            status_rx,
        ) = started_session("sess-1").await;
        let old_tag = active.as_ref().unwrap().session_tag;

        // sess-1 is superseded by a brand new sess-2, same as the
        // `DisconnectTimeout` test above.
        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-2".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;
        assert_eq!(current_session_id.as_deref(), Some("sess-2"));
        let new_tag = active.as_ref().unwrap().session_tag;
        assert_ne!(old_tag, new_tag, "each PeerSession must get a distinct tag");
        let _ = out_rx.try_recv(); // sess-1's Offer
        let _ = out_rx.try_recv(); // sess-2's Offer
        let mut status_rx = status_rx;
        status_rx.borrow_and_update(); // mark the supersede's own status send as seen

        // sess-1's `PeerConnection`, still tearing down, reports `Closed`
        // after sess-2 has already started.
        handle_session_event(
            SessionEvent::ConnectionState {
                state: RTCPeerConnectionState::Closed,
                tag: old_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(
            active.is_some(),
            "an old session's Closed event must not end the new session"
        );
        assert_eq!(current_session_id.as_deref(), Some("sess-2"));
        assert!(
            !status_rx.has_changed().unwrap(),
            "a stale Closed event must not report any new status"
        );

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Fix to slice 3.5b: same as
    /// `connection_state_from_an_old_session_does_not_touch_a_new_one`, for
    /// the `control` data channel closing -- the concrete bug this fixes
    /// (client reconnects: bye -> new session starts -> the old session's
    /// `control` channel finishes closing and its event arrives after).
    #[tokio::test(flavor = "multi_thread")]
    async fn data_channel_closed_from_an_old_session_does_not_touch_a_new_one() {
        let (ctx, mut active, mut current_session_id, out_tx, mut out_rx, event_tx, status_tx, _) =
            started_session("sess-1").await;
        let old_tag = active.as_ref().unwrap().session_tag;

        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-2".to_string(),
                ice_servers: vec![],
            },
            &ctx,
            &[],
            "111111",
            &status_tx,
            &out_tx,
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;
        assert_eq!(current_session_id.as_deref(), Some("sess-2"));
        let _ = out_rx.try_recv(); // sess-1's Offer
        let _ = out_rx.try_recv(); // sess-2's Offer

        // sess-1's `control` channel finishes its close handshake after
        // sess-2 has already started.
        handle_session_event(
            SessionEvent::DataChannelClosed {
                label: "control".to_string(),
                tag: old_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(
            active.is_some(),
            "an old session's control channel closing must not end the new session"
        );
        assert_eq!(current_session_id.as_deref(), Some("sess-2"));

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }

    /// Slice 3.5b: the `control` data channel closing ends the session
    /// immediately, without waiting for a `Disconnected` grace window.
    /// Slice 3.2a/D35: also checks that ending the session this way sends
    /// the server a `Bye`, so it doesn't keep the device marked busy -- and
    /// that the second, superseded `DataChannelClosed` (see below) does not
    /// send a second one.
    #[tokio::test(flavor = "multi_thread")]
    async fn control_data_channel_closed_ends_the_session_immediately() {
        let (
            ctx,
            mut active,
            mut current_session_id,
            out_tx,
            mut out_rx,
            event_tx,
            status_tx,
            status_rx,
        ) = started_session("sess-1").await;
        let _ = out_rx.try_recv(); // the Offer
        let tag = active.as_ref().unwrap().session_tag;

        handle_session_event(
            SessionEvent::DataChannelClosed {
                label: "control".to_string(),
                tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(active.is_none());
        assert_eq!(current_session_id, None);
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::Registered {
                pin: "111111".to_string()
            }
        );
        assert_eq!(
            out_rx.try_recv().expect("expected a Bye message"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string()
            }
        );

        // Our own `ActiveSession::shutdown` (called above) closing the peer
        // connection can deliver a second `DataChannelClosed` for the same
        // channel once the slot is already empty -- must be a quiet no-op,
        // not a double status flip, a panic, or a second `Bye`. Same `tag`
        // as before: from the *now-gone* session's own control channel.
        handle_session_event(
            SessionEvent::DataChannelClosed {
                label: "control".to_string(),
                tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;
        assert!(active.is_none());
        assert!(out_rx.try_recv().is_err(), "no second Bye should be sent");
    }

    /// A non-`control` data channel closing (e.g. `input`) must not end the
    /// session -- only `control` is treated as a deliberate end of session.
    #[tokio::test(flavor = "multi_thread")]
    async fn non_control_data_channel_closed_does_not_end_the_session() {
        let (ctx, mut active, mut current_session_id, out_tx, _out_rx, event_tx, status_tx, _) =
            started_session("sess-1").await;

        handle_session_event(
            SessionEvent::DataChannelClosed {
                label: "input".to_string(),
                tag: active.as_ref().unwrap().session_tag,
            },
            &ctx,
            "111111",
            &status_tx,
            &out_tx,
            &event_tx,
            &mut active,
            &mut current_session_id,
            &mut None,
        )
        .await;

        assert!(active.is_some());
        assert_eq!(current_session_id.as_deref(), Some("sess-1"));

        if let Some(active) = active.take() {
            active.shutdown().await;
        }
    }
}
