//! Builds a `signaling::HostContext` from CLI-shaped options and drives the
//! reconnecting agent loop on top of it. Factored out of `main.rs` (slice
//! 2.6a) so the host can be built and run without going through the CLI at
//! all -- the basis for a future tray agent (slice 2.6c). `main.rs` stays a
//! thin shell: it parses `clap` args into `ServeOptions`, calls
//! `build_host_context`, and hands the result to `run_agent`.

use std::time::Duration;

use clap::ValueEnum;
use tokio::sync::{mpsc, watch};

use crate::capture::{self, FrameSource};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use crate::clipboard::ClipboardBackend;
use crate::cursor::CursorSource;
use crate::device::DeviceStore;
use crate::encode::EncoderKind;
use crate::input::Injector;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use crate::input::NoopInjector;
use crate::platform;
use crate::signaling::{
    AgentCommand, AgentStatus, BuildClipboard, ConnectError, HostContext, Keepalive,
    SignalingClient,
};
use crate::transport::SessionConfig;

/// Which screen-capture backend to use on Windows. No effect on
/// macOS/other (there is only `scap`/synthetic there) beyond `Gdi` being
/// rejected -- see `build_screen_source`.
#[derive(Clone, Copy, ValueEnum)]
pub enum CaptureBackend {
    /// Windows Graphics Capture (`scap`) if the video driver reports
    /// Direct3D 11 support, GDI (`BitBlt`) otherwise.
    Auto,
    /// Force Windows Graphics Capture (`scap`), even if the driver looks
    /// like it can't do Direct3D 11 -- for comparing against `gdi` on
    /// hardware where `auto` already falls back.
    Wgc,
    /// Force GDI (`BitBlt`): works on any driver/VM, higher CPU cost, no
    /// "yellow border" capture indicator.
    Gdi,
}

/// Which H.264 encoder backend to use.
#[derive(Clone, Copy, ValueEnum)]
pub enum EncoderBackend {
    /// The platform's native encoder (VideoToolbox on macOS; Media
    /// Foundation on Windows, hardware MFT preferred, Microsoft's software
    /// one otherwise), openh264 if that fails -- see
    /// `encode::build_encoder`'s doc comment.
    Auto,
    /// Software encoder, built from source. Works on every platform.
    Openh264,
    /// macOS hardware encoder (VideoToolbox). Selecting it on another OS
    /// fails with a "not available" error.
    Videotoolbox,
    /// Windows Media Foundation H.264 MFT (hardware if present, otherwise
    /// Microsoft's software encoder). Selecting it on another OS fails with
    /// a "not available" error.
    Mediafoundation,
}

impl EncoderBackend {
    pub fn kind(self) -> Option<EncoderKind> {
        match self {
            EncoderBackend::Auto => None,
            EncoderBackend::Openh264 => Some(EncoderKind::OpenH264),
            EncoderBackend::Videotoolbox => Some(EncoderKind::VideoToolbox),
            EncoderBackend::Mediafoundation => Some(EncoderKind::MediaFoundation),
        }
    }
}

/// Resolves `--display` to a concrete display id before it reaches a
/// backend's `new`: an explicit `--display N` passes through unchanged
/// (an id that turns out not to exist is still that backend's error to
/// report), `None` picks `capture::default_display` out of
/// `capture::list_displays()` (propagating a `list_displays` error, e.g. no
/// screen-recording permission, as-is) and logs which display was picked.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn resolve_display(display: Option<u32>) -> anyhow::Result<u32> {
    if let Some(id) = display {
        return Ok(id);
    }
    let displays = capture::list_displays()?;
    let chosen = capture::default_display(&displays)
        .ok_or_else(|| anyhow::anyhow!("no capturable display found"))?;
    tracing::info!(display_id = chosen.id, title = %chosen.title, "capturing display");
    Ok(chosen.id)
}

/// Builds the real screen-capture source. On Windows this is where
/// `--capture auto` decides between Windows Graphics Capture (`scap`) and
/// the GDI fallback (`capture::gdi`) by probing Direct3D 11 support first
/// (`platform::windows::d3d::wgc_supported`) -- see that module and
/// `capture::gdi` for why: `scap` 0.0.8 panics outright on drivers below
/// feature level 11_0 instead of returning an error.
#[cfg(target_os = "windows")]
pub fn build_screen_source(
    display: Option<u32>,
    fps: u32,
    backend: CaptureBackend,
) -> anyhow::Result<Box<dyn FrameSource>> {
    let display = Some(resolve_display(display)?);
    let use_wgc = match backend {
        CaptureBackend::Wgc => true,
        CaptureBackend::Gdi => false,
        CaptureBackend::Auto => crate::platform::windows::d3d::wgc_supported(),
    };
    if use_wgc {
        tracing::info!(backend = "wgc", "screen capture backend");
        Ok(Box::new(capture::scap::ScapSource::new(display, fps)?))
    } else {
        tracing::info!(backend = "gdi", "screen capture backend");
        Ok(Box::new(capture::gdi::GdiSource::new(display, fps)?))
    }
}

#[cfg(target_os = "macos")]
pub fn build_screen_source(
    display: Option<u32>,
    fps: u32,
    backend: CaptureBackend,
) -> anyhow::Result<Box<dyn FrameSource>> {
    match backend {
        CaptureBackend::Gdi => Err(anyhow::anyhow!(
            "--capture gdi is a Windows-only fallback, not supported on macOS"
        )),
        CaptureBackend::Auto | CaptureBackend::Wgc => {
            let display = Some(resolve_display(display)?);
            Ok(Box::new(capture::scap::ScapSource::new(display, fps)?))
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn build_screen_source(
    _display: Option<u32>,
    _fps: u32,
    _backend: CaptureBackend,
) -> anyhow::Result<Box<dyn FrameSource>> {
    Err(capture::CaptureError::Unsupported.into())
}

/// The real, platform-backed injector (see `crate::input::enigo`).
/// Falls back to `NoopInjector` on platforms with no such backend, exactly
/// like `build_screen_source` falls back to an error for capture -- except
/// here a no-op is the correct behavior rather than a failure, since a host
/// with no way to inject input isn't a broken host, just one that can only
/// be watched.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn build_real_injector() -> anyhow::Result<Box<dyn Injector>> {
    Ok(Box::new(crate::input::enigo::EnigoInjector::new()?))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn build_real_injector() -> anyhow::Result<Box<dyn Injector>> {
    Ok(Box::new(NoopInjector::new()))
}

/// The real, platform-backed cursor-shape source (see
/// `crate::cursor::macos`/`::windows`), the same fallback pattern as
/// `build_real_injector`.
#[cfg(target_os = "macos")]
pub fn build_real_cursor_source() -> Box<dyn CursorSource> {
    Box::new(crate::cursor::macos::MacCursorSource::new())
}

#[cfg(target_os = "windows")]
pub fn build_real_cursor_source() -> Box<dyn CursorSource> {
    Box::new(crate::cursor::windows::WinCursorSource::new())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn build_real_cursor_source() -> Box<dyn CursorSource> {
    Box::new(crate::cursor::NoopCursorSource::new())
}

/// The real, platform-backed clipboard backend (slice 2.5b, see
/// `crate::clipboard::macos`/`::windows`). Unlike
/// `build_real_cursor_source`/`build_real_injector` there is no no-op
/// fallback on other platforms: `build_host_context` passes `None` for
/// `HostContext::build_clipboard` there instead, since "no clipboard sync"
/// (not "sync that always fails") is the correct behavior when there's no
/// backend at all.
#[cfg(target_os = "macos")]
pub fn build_real_clipboard_backend() -> anyhow::Result<Box<dyn ClipboardBackend>> {
    Ok(Box::new(crate::clipboard::macos::MacClipboardBackend::new()?) as Box<dyn ClipboardBackend>)
}

#[cfg(target_os = "windows")]
pub fn build_real_clipboard_backend() -> anyhow::Result<Box<dyn ClipboardBackend>> {
    Ok(
        Box::new(crate::clipboard::windows::WinClipboardBackend::new()?)
            as Box<dyn ClipboardBackend>,
    )
}

/// `serve`'s CLI flags, decoupled from `clap` (see `main.rs`'s `Command::Serve`
/// variant, which this mirrors field-for-field). `build_host_context` uses
/// everything except `server`/`name`, which only matter to the signaling
/// connection (`run_agent`'s own parameters) and are kept here purely so
/// `main.rs` has one struct to build instead of two.
pub struct ServeOptions {
    pub server: String,
    pub name: Option<String>,
    pub synthetic: bool,
    pub display: Option<u32>,
    pub fps: u32,
    pub bitrate: u32,
    pub max_qp: Option<u8>,
    pub stun: Vec<String>,
    pub no_input: bool,
    pub no_clipboard: bool,
    pub no_adapt: bool,
    pub capture: CaptureBackend,
    pub encoder: EncoderBackend,
}

/// Builds the long-lived `HostContext` for a `serve` run: everything a
/// session needs to be built from scratch (capture/injector/cursor/clipboard
/// builders, encoder settings), but no signaling connection -- unlike the
/// pre-2.6a `run_serve`, this is built exactly once and lives across every
/// reconnect `run_agent` performs. `session.ice_servers` therefore holds only
/// `opts.stun`, not the signaling server's per-registration credentials:
/// those come back fresh on every `Registered` and are merged in by
/// `signaling::run` (see `signaling::session_ice_servers`) instead of baked
/// in here once.
pub fn build_host_context(opts: &ServeOptions) -> anyhow::Result<HostContext> {
    let synthetic = opts.synthetic;
    let fps = opts.fps;
    let capture = opts.capture;
    let display = opts.display;
    let no_input = opts.no_input;
    let no_clipboard = opts.no_clipboard;

    let runtime = webrtc::runtime::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no webrtc runtime available"))?;

    let build_source: Box<dyn Fn(u32) -> anyhow::Result<Box<dyn FrameSource>> + Send + Sync> =
        if synthetic {
            Box::new(move |id| {
                Ok(Box::new(capture::synthetic::for_display(id, fps)?) as Box<dyn FrameSource>)
            })
        } else {
            Box::new(move |id| build_screen_source(Some(id), fps, capture))
        };

    let list_displays: Box<dyn Fn() -> anyhow::Result<Vec<capture::DisplayInfo>> + Send + Sync> =
        if synthetic {
            Box::new(|| Ok(capture::synthetic::list_displays()))
        } else {
            Box::new(|| Ok(capture::list_displays()?))
        };

    let build_injector: Box<dyn Fn() -> anyhow::Result<Box<dyn Injector>> + Send + Sync> =
        if no_input {
            // `Err`, not a `NoopInjector` directly: `start_session` already
            // falls back to one on any `build_injector` failure and reports
            // the reason to the client over `ControlMessage::InputStatus`
            // (slice 2.5a, debt D26) -- this way `--no-input` gets a clear,
            // specific reason instead of a generic one.
            Box::new(|| Err(anyhow::anyhow!("отключено флагом --no-input")))
        } else {
            Box::new(build_real_injector)
        };

    let build_clipboard: Option<BuildClipboard> = if no_clipboard {
        None
    } else {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            Some(Box::new(build_real_clipboard_backend) as BuildClipboard)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            None
        }
    };

    // Extra STUN/TURN servers from `--stun`; the signaling server's own
    // per-registration credentials are merged in later, per session (see
    // this function's doc comment).
    let ice_servers: Vec<proto::signal::IceServer> = opts
        .stun
        .iter()
        .cloned()
        .map(|url| proto::signal::IceServer {
            urls: vec![url],
            username: None,
            credential: None,
        })
        .collect();

    Ok(HostContext {
        session: SessionConfig {
            ice_servers,
            udp_addrs: vec!["0.0.0.0:0".to_string()],
            fps,
        },
        bitrate_kbps: opts.bitrate,
        adapt: !opts.no_adapt,
        max_qp: opts.max_qp,
        encoder: opts.encoder.kind(),
        build_source,
        list_displays,
        display,
        build_injector,
        build_cursor_source: Box::new(build_real_cursor_source),
        build_clipboard,
        runtime,
    })
}

/// Resolves the host name registered with the signaling server (and shown to
/// clients): an explicit `--name` wins outright; otherwise
/// `platform::computer_name()` (the Mac/PC's own display name); otherwise
/// `$HOSTNAME`; otherwise the fixed fallback `"rcdesk-host"`.
pub fn resolve_host_name(explicit: Option<&str>) -> String {
    if let Some(name) = explicit {
        return name.to_string();
    }
    if let Some(name) = platform::computer_name() {
        return name;
    }
    if let Ok(name) = std::env::var("HOSTNAME") {
        if !name.is_empty() {
            return name;
        }
    }
    "rcdesk-host".to_string()
}

/// Backoff schedule for `run_agent`'s reconnect loop: the delay before the
/// next attempt starts at `initial`, doubles on every consecutive failure up
/// to `max`, and resets to `initial` the moment a registration succeeds.
#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    pub initial: Duration,
    pub max: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

/// Mutable backoff state for one `run_agent` run. Kept as a small struct
/// (rather than a bare `Duration` recomputed inline) so the doubling/cap/
/// reset rules are unit-testable on their own, without going through
/// `run_agent`'s actual `sleep`s.
struct Backoff {
    policy: ReconnectPolicy,
    current: Duration,
}

impl Backoff {
    fn new(policy: ReconnectPolicy) -> Self {
        Self {
            current: policy.initial,
            policy,
        }
    }

    fn current(&self) -> Duration {
        self.current
    }

    /// Doubles the delay, capped at `policy.max`.
    fn advance(&mut self) {
        self.current = (self.current * 2).min(self.policy.max);
    }

    /// Called after a successful registration.
    fn reset(&mut self) {
        self.current = self.policy.initial;
    }
}

/// Drives one host agent's entire lifetime: connect, register, serve
/// sessions through `SignalingClient::run`, and on any failure (a failed
/// connect, a registration the server rejected, or a connection that later
/// drops) wait out `policy`'s backoff and try again -- forever. `status`
/// reports the agent's current phase (see `signaling::AgentStatus`); `serve`
/// uses it only to print `PIN: ...` on every fresh registration, but it's the
/// hook a future tray UI (slice 2.6c) needs to show live state.
///
/// `ctx` is a shared reference rather than owned: it must survive every
/// reconnect (its capture/injector/cursor/clipboard builders and encoder
/// settings never change), and nothing here ever needs to move it into a
/// detached (`'static`) task -- each `SignalingClient::run(ctx, ...)` call is
/// awaited in place, in this same loop, so a plain borrow that outlives the
/// loop (owned by whoever calls `run_agent`) is enough; an `Arc` would only
/// add an indirection with no caller benefit. A caller that does need to
/// `tokio::spawn` this (e.g. this module's own reconnect test) can move an
/// owned `Arc<HostContext>` into the spawned `async move` block and pass
/// `&*that_arc` here -- the borrow then lives inside the same future, not
/// across it.
///
/// Never returns; cancel by dropping or aborting whatever task awaits it.
///
/// `commands` is the tray agent's "End session" input (slice 2.6c, see
/// `AgentCommand`): owned here, across every reconnect, and handed to each
/// `SignalingClient::run` call as a `&mut` borrow so a command that arrives
/// while disconnected isn't lost, just delayed until the next successful
/// registration. `serve` (no tray UI) passes a receiver whose sender it just
/// holds onto for the process lifetime -- nothing ever sends on it, so this
/// branch simply never fires there.
///
/// `device_store` holds this host's persistent device credentials (slice
/// 3.1c): loaded fresh before every `connect` attempt (so a credentials file
/// written by a previous run, or forgotten below, is always picked up), and
/// saved once `connect` reports a freshly issued one
/// (`SignalingClient::issued_device`, `Some` only on this device's very
/// first registration). `ConnectError::UnknownDevice` (the server doesn't
/// recognize the saved credentials, e.g. its database was recreated) makes
/// this loop forget them and retry immediately, skipping the backoff
/// sleep/advance below entirely -- this isn't a lost connection, just a
/// device the server no longer knows, so there's no reason to wait or to
/// disturb the backoff state a real connection failure is tracking.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent(
    ctx: &HostContext,
    server: &str,
    name: &str,
    policy: ReconnectPolicy,
    keepalive: Keepalive,
    device_store: &DeviceStore,
    status: watch::Sender<AgentStatus>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
) -> anyhow::Result<std::convert::Infallible> {
    let mut backoff = Backoff::new(policy);
    loop {
        let _ = status.send(AgentStatus::Connecting);
        let device = device_store.load();
        match SignalingClient::connect(server, name, device).await {
            Ok(client) => {
                let pin = client.pin().to_string();
                tracing::info!(
                    pin = %pin,
                    host_id = client.host_id(),
                    "registered with signaling server"
                );
                if let Some(issued) = client.issued_device() {
                    if let Err(err) = device_store.save(issued) {
                        tracing::warn!(
                            error = %err,
                            path = %device_store.path().display(),
                            "failed to save issued device credentials, may re-register as a new device next time"
                        );
                    }
                }
                let _ = status.send(AgentStatus::Registered { pin: pin.clone() });
                backoff.reset();

                if let Err(err) = client.run(ctx, &status, keepalive, &mut commands).await {
                    let retry_in = backoff.current();
                    tracing::warn!(
                        error = %err,
                        ?retry_in,
                        "signaling connection lost, reconnecting"
                    );
                    let _ = status.send(AgentStatus::Reconnecting {
                        error: err.to_string(),
                        retry_in,
                    });
                }
            }
            Err(ConnectError::UnknownDevice) => {
                tracing::info!("the server does not know this device, registering as a new one");
                match device_store.forget() {
                    Ok(()) => {
                        // Not a lost connection -- retry right away, without
                        // waiting out or disturbing `backoff` (see this
                        // function's doc comment). Safe from spinning only
                        // because the credentials are now gone: the next
                        // attempt registers without any, which the server
                        // never answers with `unknown device`.
                        continue;
                    }
                    Err(err) => {
                        // The stale credentials are still on disk, so an
                        // immediate retry would present them again and land
                        // right back here -- fall through to the backoff
                        // sleep instead of spinning.
                        tracing::warn!(
                            error = %err,
                            path = %device_store.path().display(),
                            "failed to remove stale device credentials file"
                        );
                    }
                }
            }
            Err(ConnectError::Other(err)) => {
                let retry_in = backoff.current();
                if err.to_string().contains("invalid device credentials") {
                    // Unlike `UnknownDevice`, this isn't forgotten
                    // automatically: a rejected secret is a sign of tampering
                    // or corruption, not a routine "this device is new to the
                    // server" case, so it's left for the owner to act on.
                    tracing::warn!(
                        path = %device_store.path().display(),
                        "saved device credentials were rejected as invalid -- delete this file to register as a new device"
                    );
                }
                tracing::warn!(
                    error = %err,
                    ?retry_in,
                    "failed to connect to signaling server, retrying"
                );
                let _ = status.send(AgentStatus::Reconnecting {
                    error: err.to_string(),
                    retry_in,
                });
            }
        }
        tokio::time::sleep(backoff.current()).await;
        backoff.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps_at_max() {
        let policy = ReconnectPolicy {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(200),
        };
        let mut backoff = Backoff::new(policy);
        assert_eq!(backoff.current(), Duration::from_millis(50));

        backoff.advance();
        assert_eq!(backoff.current(), Duration::from_millis(100));

        backoff.advance();
        assert_eq!(backoff.current(), Duration::from_millis(200));

        // Already at the cap: stays put rather than overshooting.
        backoff.advance();
        assert_eq!(backoff.current(), Duration::from_millis(200));
    }

    #[test]
    fn backoff_resets_to_initial_after_reset() {
        let policy = ReconnectPolicy {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(200),
        };
        let mut backoff = Backoff::new(policy);
        backoff.advance();
        backoff.advance();
        assert_eq!(backoff.current(), Duration::from_millis(200));

        backoff.reset();
        assert_eq!(backoff.current(), Duration::from_millis(50));
    }

    use futures_util::{SinkExt, StreamExt};
    use proto::signal::SignalMessage;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::WebSocketStream;

    /// Drains Hello + HostRegister (this fake server doesn't care about
    /// their contents, only that a registration was attempted) and replies
    /// with `Registered { pin, device, .. }`.
    async fn respond_with_registered(
        ws: &mut WebSocketStream<TcpStream>,
        pin: &str,
        device: Option<proto::signal::DeviceCredentials>,
    ) {
        let _ = ws.next().await;
        let _ = ws.next().await;
        let msg = serde_json::to_string(&SignalMessage::Registered {
            host_id: "host-1".to_string(),
            pin: pin.to_string(),
            ice_servers: vec![],
            device,
        })
        .unwrap();
        ws.send(Message::text(msg)).await.unwrap();
    }

    /// A fresh, per-test `DeviceStore` directory under the system temp dir --
    /// same reasoning as `agent::logging`'s/`device`'s own `scratch_dir`
    /// helper: a unique name is enough, `tempfile` is not an approved
    /// dependency.
    fn scratch_device_store(name: &str) -> (crate::device::DeviceStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-host-app-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        (crate::device::DeviceStore::new(&dir), dir)
    }

    async fn next_status(rx: &mut watch::Receiver<AgentStatus>) -> AgentStatus {
        rx.changed().await.expect("status sender dropped");
        rx.borrow().clone()
    }

    /// End-to-end reconnect proof: a fake WS server registers a first
    /// connection with `pin: "111111"` and then drops it; `run_agent` must
    /// notice, back off, reconnect, and see a second `Registered` with
    /// `pin: "222222"` -- all observable through the `watch` channel without
    /// ever touching a real signaling server.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_agent_reconnects_after_the_server_drops_the_connection() {
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        // `watch` keeps only the *latest* value: if this server dropped the
        // first connection right away, run_agent could race through
        // Registered{111111} -> Reconnecting -> Registered{222222} before
        // the test ever reads the channel, and the first registration would
        // be invisible (a real flake: it failed this way on a loaded macOS
        // CI runner, Deploy 35498413799). The test signals through this
        // oneshot once it has actually observed the first registration.
        let (saw_first_tx, saw_first_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            // First connection: register, then drop -- forcing run_agent to
            // reconnect.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            respond_with_registered(&mut ws, "111111", None).await;
            let _ = saw_first_rx.await;
            drop(ws);

            // Second connection: register and then just hang -- run_agent's
            // `SignalingClient::run` sits in its read loop until the test
            // aborts the task.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            respond_with_registered(&mut ws, "222222", None).await;
            std::future::pending::<()>().await;
        });

        let ctx = Arc::new(crate::signaling::test_ctx());
        let (device_store, device_dir) = scratch_device_store("reconnect-drop");
        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
        let policy = ReconnectPolicy {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(200),
        };

        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(async move {
            let ctx = ctx;
            let _ = run_agent(
                &ctx,
                &url,
                "test-host",
                policy,
                Keepalive::default(),
                &device_store,
                status_tx,
                cmd_rx,
            )
            .await;
        });

        // Connecting (initial), then Registered{111111}.
        timeout(Duration::from_secs(10), async {
            loop {
                if next_status(&mut status_rx).await
                    == (AgentStatus::Registered {
                        pin: "111111".to_string(),
                    })
                {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for the first registration");

        // Now let the server drop the first connection.
        let _ = saw_first_tx.send(());

        // The second registration carries a different PIN, which is itself
        // proof that run_agent reconnected. The intermediate `Reconnecting`
        // is deliberately *not* asserted here: `watch` coalesces, so it can
        // legitimately be overwritten before this loop sees it. That status
        // is covered deterministically by
        // `run_agent_reconnects_when_the_connection_goes_silent`, whose
        // backoff (1s) keeps it observable.
        timeout(Duration::from_secs(10), async {
            loop {
                if next_status(&mut status_rx).await
                    == (AgentStatus::Registered {
                        pin: "222222".to_string(),
                    })
                {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for the second registration");

        agent.abort();
        let _ = std::fs::remove_dir_all(&device_dir);
    }

    /// Proves the keepalive timeout, not just the "the socket errored out"
    /// path above: the fake server registers and then never reads or writes
    /// again, so the TCP connection stays fully open (no reset, no close
    /// frame) -- exactly a laptop-sleep/half-open-NAT drop, which a plain
    /// `read.next()` never surfaces as an error on its own. `run_agent` must
    /// still notice (via `Keepalive::timeout`) and start reconnecting.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_agent_reconnects_when_the_connection_goes_silent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            respond_with_registered(&mut ws, "111111", None).await;
            // Never call `.next()`/`.send()` on `ws` again: tungstenite only
            // queues/sends an automatic Pong reply to an incoming Ping while
            // actively being polled, so simply not polling it any more is
            // enough to make this server "not respond to Ping" without
            // needing a raw-socket workaround. `ws` (and the TCP stream
            // inside it) is kept alive by this future's own scope instead of
            // being dropped, so the connection stays fully open.
            std::future::pending::<()>().await;
        });

        let ctx = crate::signaling::test_ctx();
        let (device_store, device_dir) = scratch_device_store("silent");
        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
        let policy = ReconnectPolicy::default();
        let keepalive = Keepalive {
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(300),
        };

        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(async move {
            let _ = run_agent(
                &ctx,
                &url,
                "test-host",
                policy,
                keepalive,
                &device_store,
                status_tx,
                cmd_rx,
            )
            .await;
        });

        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    next_status(&mut status_rx).await,
                    AgentStatus::Reconnecting { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect(
            "expected run_agent to notice the silent connection and start reconnecting within 5s",
        );

        agent.abort();
        let _ = std::fs::remove_dir_all(&device_dir);
    }

    /// The mirror image of the test above: a server that keeps responding
    /// (so tungstenite's automatic Pong replies to our keepalive Pings keep
    /// going out) must *not* be dropped for a "timeout" even though
    /// `Keepalive::timeout` here (300ms) is far shorter than how long this
    /// test actually watches (1s).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_agent_keepalive_does_not_false_positive_on_a_responsive_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            respond_with_registered(&mut ws, "111111", None).await;
            // Keep polling so tungstenite's automatic Pong replies to our
            // Pings actually go out -- the opposite of the silent-server
            // test above, which relies on never polling again.
            while ws.next().await.is_some() {}
        });

        let ctx = crate::signaling::test_ctx();
        let (device_store, device_dir) = scratch_device_store("keepalive-ok");
        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
        let policy = ReconnectPolicy::default();
        let keepalive = Keepalive {
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(300),
        };

        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(async move {
            let _ = run_agent(
                &ctx,
                &url,
                "test-host",
                policy,
                keepalive,
                &device_store,
                status_tx,
                cmd_rx,
            )
            .await;
        });

        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    next_status(&mut status_rx).await,
                    AgentStatus::Registered { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for the first registration");

        let saw_reconnecting = timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    next_status(&mut status_rx).await,
                    AgentStatus::Reconnecting { .. }
                ) {
                    return;
                }
            }
        })
        .await
        .is_ok();
        assert!(
            !saw_reconnecting,
            "a responsive server must not trigger a keepalive timeout"
        );

        agent.abort();
        let _ = std::fs::remove_dir_all(&device_dir);
    }

    /// Slice 3.1c: a first registration whose `Registered` carries freshly
    /// issued device credentials must have them saved to `device_store`'s
    /// file before the caller can observe `AgentStatus::Registered` (see
    /// `run_agent`'s save-then-announce ordering) -- verified here through a
    /// second, independent `DeviceStore` over the same directory, since the
    /// original is moved into the spawned task.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_agent_saves_freshly_issued_device_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        let issued = proto::signal::DeviceCredentials {
            device_id: "dev-abc".to_string(),
            secret: "top-secret".to_string(),
        };
        let issued_for_server = issued.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            respond_with_registered(&mut ws, "111111", Some(issued_for_server)).await;
            std::future::pending::<()>().await;
        });

        let ctx = crate::signaling::test_ctx();
        let (device_store, device_dir) = scratch_device_store("issue-save");
        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
        let policy = ReconnectPolicy::default();

        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(async move {
            let _ = run_agent(
                &ctx,
                &url,
                "test-host",
                policy,
                Keepalive::default(),
                &device_store,
                status_tx,
                cmd_rx,
            )
            .await;
        });

        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    next_status(&mut status_rx).await,
                    AgentStatus::Registered { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for the registration");

        agent.abort();

        let loaded = crate::device::DeviceStore::new(&device_dir).load();
        assert_eq!(loaded, Some(issued));

        let _ = std::fs::remove_dir_all(&device_dir);
    }

    /// Slice 3.1c: when `device_store` already holds saved credentials
    /// before `connect`, they must be sent back to the server in
    /// `HostRegister.device` on the very next registration.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_agent_presents_saved_device_credentials_on_register() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        let saved = proto::signal::DeviceCredentials {
            device_id: "dev-saved".to_string(),
            secret: "saved-secret".to_string(),
        };

        let (seen_tx, seen_rx) =
            tokio::sync::oneshot::channel::<Option<proto::signal::DeviceCredentials>>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // Hello
            let register = ws.next().await.unwrap().unwrap();
            let device = match register {
                Message::Text(text) => match serde_json::from_str::<SignalMessage>(text.as_str())
                    .expect("valid HostRegister json")
                {
                    SignalMessage::HostRegister { device, .. } => device,
                    other => panic!("expected HostRegister, got {other:?}"),
                },
                other => panic!("expected a text message, got {other:?}"),
            };
            let _ = seen_tx.send(device);
            // Hello + HostRegister were already drained by hand above (to
            // inspect the latter), so send `Registered` directly instead of
            // `respond_with_registered`, which expects to drain both itself.
            let msg = serde_json::to_string(&SignalMessage::Registered {
                host_id: "host-1".to_string(),
                pin: "111111".to_string(),
                ice_servers: vec![],
                device: None,
            })
            .unwrap();
            ws.send(Message::text(msg)).await.unwrap();
            std::future::pending::<()>().await;
        });

        let ctx = crate::signaling::test_ctx();
        let (device_store, device_dir) = scratch_device_store("present-saved");
        device_store.save(&saved).unwrap();
        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
        let policy = ReconnectPolicy::default();

        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(async move {
            let _ = run_agent(
                &ctx,
                &url,
                "test-host",
                policy,
                Keepalive::default(),
                &device_store,
                status_tx,
                cmd_rx,
            )
            .await;
        });

        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    next_status(&mut status_rx).await,
                    AgentStatus::Registered { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for the registration");

        let seen = timeout(Duration::from_secs(5), seen_rx)
            .await
            .expect("timed out waiting for the server to see HostRegister")
            .expect("server task dropped the sender");
        assert_eq!(seen, Some(saved));

        agent.abort();
        let _ = std::fs::remove_dir_all(&device_dir);
    }

    /// Slice 3.1c: `Error { message: "unknown device" }` on the first
    /// registration attempt must make `run_agent` forget the stale
    /// credentials and retry right away (no backoff wait), ending up
    /// registered again with whatever new credentials the server issues.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_agent_forgets_credentials_and_reregisters_on_unknown_device() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}/ws");

        let old = proto::signal::DeviceCredentials {
            device_id: "dev-old".to_string(),
            secret: "old-secret".to_string(),
        };
        let new_creds = proto::signal::DeviceCredentials {
            device_id: "dev-new".to_string(),
            secret: "new-secret".to_string(),
        };
        let new_creds_for_server = new_creds.clone();

        tokio::spawn(async move {
            // First connection: the server no longer recognizes the saved
            // (old) device.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await; // Hello
            let _ = ws.next().await; // HostRegister (with the now-stale credentials)
            let err = serde_json::to_string(&SignalMessage::Error {
                message: "unknown device".to_string(),
            })
            .unwrap();
            ws.send(Message::text(err)).await.unwrap();
            drop(ws);

            // Second connection, immediately after (no backoff wait): a
            // fresh registration, issuing new credentials.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            respond_with_registered(&mut ws, "222222", Some(new_creds_for_server)).await;
            std::future::pending::<()>().await;
        });

        let ctx = crate::signaling::test_ctx();
        let (device_store, device_dir) = scratch_device_store("unknown-device");
        device_store.save(&old).unwrap();
        let (status_tx, mut status_rx) = watch::channel(AgentStatus::Connecting);
        // A generous policy: this test relies on the unknown-device retry
        // skipping the backoff wait entirely, but keeps a real (non-zero)
        // one so a regression that *does* wait would show up as a timeout
        // rather than passing by accident.
        let policy = ReconnectPolicy {
            initial: Duration::from_secs(5),
            max: Duration::from_secs(5),
        };

        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agent = tokio::spawn(async move {
            let _ = run_agent(
                &ctx,
                &url,
                "test-host",
                policy,
                Keepalive::default(),
                &device_store,
                status_tx,
                cmd_rx,
            )
            .await;
        });

        timeout(Duration::from_secs(2), async {
            loop {
                if next_status(&mut status_rx).await
                    == (AgentStatus::Registered {
                        pin: "222222".to_string(),
                    })
                {
                    break;
                }
            }
        })
        .await
        .expect(
            "timed out waiting for re-registration after unknown device (backoff not skipped?)",
        );

        agent.abort();

        let loaded = crate::device::DeviceStore::new(&device_dir).load();
        assert_eq!(
            loaded,
            Some(new_creds),
            "device store must hold the newly issued credentials, not the old ones"
        );

        let _ = std::fs::remove_dir_all(&device_dir);
    }
}
