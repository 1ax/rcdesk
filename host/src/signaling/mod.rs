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

use std::sync::Arc;
use std::time::Duration;

use webrtc::runtime::Runtime;

#[cfg(test)]
use crate::capture;
use crate::capture::{DisplayInfo, FrameSource};
use crate::clipboard::ClipboardBackend;
use crate::cursor::CursorSource;
use crate::curtain::Curtain;
use crate::encode::EncoderKind;
use crate::input::Injector;
use crate::input_source::InputSourceSwitcher;
use crate::transport::SessionConfig;

mod client;
mod control;
mod display;
mod handlers;
mod session;
mod start;
mod stats;
use handlers::{handle_agent_command, handle_session_event, handle_signal_message};
use session::{ActiveSession, VideoPipeline};
use stats::{send_session_stats, SESSION_STATS_INTERVAL};

pub use client::{ConnectError, Keepalive, PinStore, SignalingClient, CONNECT_TIMEOUT};
pub use session::SessionSlot;

/// See `HostContext::build_clipboard`'s doc comment. Factored out (clippy
/// `type_complexity`): the `Option<Box<dyn Fn() -> ...>>` nesting is one
/// level deeper than `build_injector`/`build_cursor_source`'s bare closures.
pub type BuildClipboard = Box<dyn Fn() -> anyhow::Result<Box<dyn ClipboardBackend>> + Send + Sync>;

/// См. `HostContext::build_source`. Вынесен в псевдоним (clippy
/// `type_complexity`).
pub type BuildSource =
    Arc<dyn Fn(u32, &[u32]) -> anyhow::Result<Box<dyn FrameSource>> + Send + Sync>;

/// Будит дисплей (слайс 4.6a) и возвращает guard, который держит ассерцию
/// до своего `Drop`. Замыкание, а не прямой вызов `platform`, чтобы тесты и
/// синтетика не дёргали IOKit (это включало бы экран машины разработчика).
pub type WakeDisplay = Box<dyn Fn() -> Box<dyn Send> + Send + Sync>;

/// Каталоги для обновления хоста из сеанса (слайс D50d): рабочий (куда
/// качается установщик) и каталог логов установщика.
#[derive(Debug, Clone)]
pub struct UpdateDirs {
    pub work_dir: std::path::PathBuf,
    pub log_dir: std::path::PathBuf,
}

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
    /// (slice 2.4), not just once per session. Второй аргумент -- CGWindowID
    /// окон, которые не захватывать (окна шторки, слайс 4.2c); пустой срез
    /// -- исключений нет. Действует только на macOS, остальные бэкенды его
    /// игнорируют. `Arc`, а не `Box`: `VideoPipeline::start` клонирует
    /// замыкание в `spawn_blocking` (V-1).
    pub build_source: BuildSource,
    /// Шторка хоста (слайс 4.2c), если платформа её умеет; `None` --
    /// `SetCurtain` отвечает отказом, а `HostInfo.curtain_available` ложно.
    /// Реализация приедет в 4.2c2, пока везде `None`.
    pub curtain: Option<Arc<dyn Curtain>>,
    /// Переключатель раскладки хоста (слайс 4.6c); `None` --
    /// `SwitchInputSource` игнорируется, `HostInfo.input_source_available`
    /// ложно.
    pub input_source: Option<Arc<dyn InputSourceSwitcher>>,
    /// Будит спящий дисплей перед выбором дисплея в `start_session` (4.6a).
    pub wake_display: WakeDisplay,
    /// Lists the displays capturable right now, for the initial
    /// `ControlMessage::Displays` announcement and to resolve
    /// `ControlMessage::SelectDisplay` ids (slice 2.4). `main.rs` supplies
    /// either `capture::list_displays` or the synthetic backend's fixed
    /// two-display list.
    pub list_displays: Arc<dyn Fn() -> anyhow::Result<Vec<DisplayInfo>> + Send + Sync>,
    /// V-2: дешёвый источник геометрии дисплеев для фонового опроса
    /// (`spawn_display_poll`, раз в 2 с). Нужны только `id/x/y/width/height`.
    /// На macOS -- CoreGraphics (`platform::displays::geometries`): `list_displays`
    /// там идёт через ScreenCaptureKit (дорого, течёт без autorelease-пула на
    /// потоках blocking-пула). На остальных платформах -- тот же список, что
    /// `list_displays`.
    pub poll_displays: Arc<dyn Fn() -> anyhow::Result<Vec<DisplayInfo>> + Send + Sync>,
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
    /// Whether file transfer is allowed (slice 3.4a); `false` for
    /// `serve --no-files`. The `files` task still runs and answers every
    /// `Start` with `Abort { Disabled }`, and `HostInfo.files_enabled` tells
    /// the client up front.
    pub files_enabled: bool,
    /// Full-mode helper (`serve --machine`, `ServeOptions::data_dir` set):
    /// the process may run as SYSTEM, so per-user folders (Downloads) are
    /// looked up for the console session's user.
    pub machine_mode: bool,
    pub runtime: Arc<dyn Runtime>,
    /// Slice 3.2c: the host's access password, if one is set (`access::AccessStore::load`
    /// returns `Some` -- see `access.rs`'s module doc comment). When present,
    /// `handle_signal_message`'s `PeerJoined` arm asks the joining client to
    /// complete an OPAQUE login (`AuthRequired`/`PakeStart`/`PakeResponse`/
    /// `PakeFinish`) before offering a session, instead of offering right
    /// away. `main.rs`/`app::build_host_context` point this at the same
    /// data directory as `device::DeviceStore` (`agent::paths::data_dir()`).
    pub access_store: crate::access::AccessStore,
    /// Каталоги обновления (слайс D50d); `None` -- хост не умеет обновляться
    /// из сеанса: `UpdateHost` отвечает отказом, а
    /// `HostInfo.update_available` ложно.
    pub update_dirs: Option<UpdateDirs>,
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
        build_source: Arc::new(|id, _exclude| {
            Ok(Box::new(capture::synthetic::for_display(id, 30)?) as Box<dyn FrameSource>)
        }),
        curtain: None,
        input_source: None,
        wake_display: Box::new(|| Box::new(()) as Box<dyn Send>),
        list_displays: Arc::new(|| Ok(capture::synthetic::list_displays())),
        poll_displays: Arc::new(|| Ok(capture::synthetic::list_displays())),
        display: None,
        build_injector: Box::new(|| {
            Ok(Box::new(crate::input::NoopInjector::new()) as Box<dyn Injector>)
        }),
        build_cursor_source: Box::new(|| {
            Box::new(crate::cursor::NoopCursorSource::new()) as Box<dyn CursorSource>
        }),
        build_clipboard: None,
        files_enabled: false,
        machine_mode: false,
        runtime: webrtc::runtime::default_runtime().expect("runtime-tokio feature must be enabled"),
        // Slice 3.2c: a fresh, per-test scratch directory under the system
        // temp dir that's never written to here, mirroring
        // `access::tests::scratch_dir`/`app::tests::scratch_device_store` --
        // no file at this path means `access_store.load()` is `Missing`, i.e.
        // "no password set", so every existing test built on `test_ctx()`
        // keeps behaving exactly as before this slice.
        access_store: crate::access::AccessStore::new(&std::env::temp_dir().join(format!(
            "rcdesk-host-signaling-test-ctx-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))),
        update_dirs: None,
    }
}

#[cfg(test)]
mod tests;
