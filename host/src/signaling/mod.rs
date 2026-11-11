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
mod tests {
    use super::control::begin_update;
    use super::display::{
        curtain_announce_message, displays_message, set_curtain, switch_display,
        PIPELINE_RECOVERY_MAX,
    };
    use super::session::PendingAuth;
    use super::start::{bye_for_start_error, start_session, start_session_with_display_wait};
    use crate::encode::EncodedFrame;
    use crate::transport::{PeerSession, SessionEvent};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use proto::control::{ControlMessage, DisplayEntry, QualityPreset};
    use proto::signal::SignalMessage;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;
    use tokio::sync::{mpsc, watch};
    use webrtc::peer_connection::RTCPeerConnectionState;

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

    /// Слайс D50d: `UpdateHost` при недоступности -- `Failed` с причиной.
    #[tokio::test]
    async fn update_host_without_dirs_is_refused() {
        let ctx = test_ctx();
        match begin_update(&ctx) {
            Err(refusal) => assert_eq!(
                *refusal,
                ControlMessage::UpdateStatus {
                    state: proto::control::UpdateState::Failed,
                    version: None,
                    message: Some("обновление недоступно на этом хосте".to_string()),
                    received: None,
                    total: None,
                }
            ),
            Ok(_) => panic!("обновление не должно стартовать без update_dirs"),
        }
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
        let generation = Arc::new(AtomicU64::new(0));

        let displays = capture::synthetic::list_displays();
        let display1 = displays.iter().find(|d| d.id == 1).unwrap().clone();
        let display2 = displays.iter().find(|d| d.id == 2).unwrap().clone();

        let mut current = VideoPipeline::start(
            &ctx,
            display1,
            Arc::clone(&peer),
            video_tx.clone(),
            Arc::clone(&keyframe_flag),
            0,
            Arc::clone(&generation),
            &[],
        )
        .await
        .unwrap();

        let first_batch = collect_frames(&mut video_rx, 5, Duration::from_secs(5)).await;
        assert!(
            first_batch[0].keyframe,
            "first frame of a fresh pipeline must be a keyframe"
        );

        generation.store(1, Ordering::SeqCst);
        let next = VideoPipeline::start(
            &ctx,
            display2,
            Arc::clone(&peer),
            video_tx.clone(),
            Arc::clone(&keyframe_flag),
            1,
            Arc::clone(&generation),
            &[],
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

    /// D24: once a newer generation is announced, the older pipeline's
    /// forward task must stop writing into the shared channel (frames already
    /// in the channel or in flight at that moment may still land).
    #[tokio::test(flavor = "multi_thread")]
    async fn superseded_pipeline_stops_sending_frames() {
        let ctx = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel(64);
        let peer = PeerSession::new(ctx.session.clone(), events_tx, Arc::clone(&ctx.runtime))
            .await
            .unwrap();
        let (video_tx, mut video_rx) = mpsc::channel(4);
        let keyframe_flag = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let display = capture::synthetic::list_displays().remove(0);
        let pipeline = VideoPipeline::start(
            &ctx,
            display,
            peer,
            video_tx,
            keyframe_flag,
            0,
            Arc::clone(&generation),
            &[],
        )
        .await
        .unwrap();
        collect_frames(&mut video_rx, 3, Duration::from_secs(5)).await;

        generation.store(1, Ordering::SeqCst);
        // Let the channel fill and the forward task block in `send`, then
        // drain what was already queued (plus the one in-flight frame).
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            while video_rx.try_recv().is_ok() {}
        }
        let late = tokio::time::timeout(Duration::from_millis(500), video_rx.recv()).await;
        assert!(late.is_err(), "superseded pipeline still sent a frame");

        // Without a newer generation the pipeline would have kept sending
        // (sanity check that the silence above comes from the gate).
        generation.store(0, Ordering::SeqCst);
        collect_frames(&mut video_rx, 1, Duration::from_secs(5)).await;
        pipeline.stop();
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
            0,
            Arc::new(AtomicU64::new(0)),
            &[],
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

    /// Slice 4.6b: no capturable display -> `start_session` fails with the
    /// typed error and the resulting `Bye` carries `DisplayUnavailable`;
    /// any other failure stays a plain `Bye`. The wait timeout is shrunk via
    /// `start_session_with_display_wait` so the test doesn't sit out 5 s.
    /// Ending a session with a live peer queues the final `SessionStats`
    /// first -- ahead of the host's own `Bye`.
    fn expect_final_stats(out_rx: &mut mpsc::UnboundedReceiver<SignalMessage>) {
        match out_rx.try_recv().expect("expected a final SessionStats") {
            SignalMessage::SessionStats {
                session_id,
                final_report,
                ..
            } => {
                assert_eq!(session_id, "sess-1");
                assert!(final_report);
            }
            other => panic!("expected SessionStats, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_display_list_yields_bye_with_display_unavailable() {
        let mut ctx = test_ctx();
        ctx.list_displays = Arc::new(|| Ok(Vec::new()));
        let (events_tx, _events_rx) = mpsc::channel(64);

        let err = start_session_with_display_wait(&ctx, &[], events_tx, Duration::from_millis(30))
            .await
            .err()
            .expect("no display must fail the start");

        assert_eq!(
            bye_for_start_error("sess-1".to_string(), &err),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: Some(proto::signal::ByeReason::DisplayUnavailable),
            }
        );
        let other = anyhow::anyhow!("synthetic source failure");
        assert_eq!(
            bye_for_start_error("sess-1".to_string(), &other),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: None,
            }
        );
    }

    /// V-2: вычитывает исходящие сообщения и сообщает, был ли среди них `Bye`.
    fn out_tx_has_bye(rx: &mut mpsc::UnboundedReceiver<SignalMessage>) -> bool {
        let mut found = false;
        while let Ok(msg) = rx.try_recv() {
            found |= matches!(msg, SignalMessage::Bye { .. });
        }
        found
    }
    /// D58 + V-2: a capture source that dies mid-session first gets the
    /// pipeline restarted (up to `PIPELINE_RECOVERY_MAX` times); only when the
    /// budget is exhausted does the session end with `Bye { VideoFailed }` and
    /// the agent return to `Registered`.
    #[tokio::test(flavor = "multi_thread")]
    async fn capture_failure_restarts_pipeline_until_budget_is_exhausted() {
        struct DyingSource {
            inner: Box<dyn FrameSource>,
            left: u32,
        }
        impl FrameSource for DyingSource {
            fn next_frame(&mut self) -> Result<capture::RawFrame, capture::CaptureError> {
                if self.left == 0 {
                    return Err(capture::CaptureError::Backend("boom".into()));
                }
                self.left -= 1;
                self.inner.next_frame()
            }
            fn size(&self) -> (u32, u32) {
                self.inner.size()
            }
        }

        let mut ctx = test_ctx();
        ctx.build_source = Arc::new(|id, _exclude| {
            Ok(Box::new(DyingSource {
                inner: Box::new(capture::synthetic::for_display(id, 30)?),
                left: 3,
            }) as Box<dyn FrameSource>)
        });
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, mut event_rx) = mpsc::channel(64);
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
            event_tx.clone(),
            &mut active,
            &mut current_session_id,
            &mut None,
            &mut 0,
            &mut None,
        )
        .await;
        assert!(active.is_some(), "session must have started successfully");
        let tag = active.as_ref().unwrap().session_tag;

        // V-2: каждый новый конвейер снова умирает через 3 кадра. Первые
        // PIPELINE_RECOVERY_MAX сбоев -- перезапуск (сеанс жив), следующий --
        // конец сеанса.
        for attempt in 1..=PIPELINE_RECOVERY_MAX + 1 {
            let failed = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match event_rx.recv().await {
                        Some(SessionEvent::VideoFailed { tag }) => break Some(tag),
                        Some(_) => continue,
                        None => break None,
                    }
                }
            })
            .await
            .expect("VideoFailed must arrive");
            assert_eq!(failed, Some(tag));

            handle_session_event(
                SessionEvent::VideoFailed { tag },
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

            if attempt <= PIPELINE_RECOVERY_MAX {
                assert!(active.is_some(), "attempt {attempt}: session must survive");
                assert_eq!(current_session_id.as_deref(), Some("sess-1"));
                assert!(
                    !out_tx_has_bye(&mut out_rx),
                    "attempt {attempt}: no Bye while recovering"
                );
            }
        }

        assert!(active.is_none());
        assert_eq!(current_session_id, None);
        assert_eq!(
            *status_rx.borrow(),
            AgentStatus::Registered {
                pin: "111111".to_string()
            }
        );
        let mut found = false;
        while let Ok(msg) = out_rx.try_recv() {
            if msg
                == (SignalMessage::Bye {
                    session_id: "sess-1".to_string(),
                    reason: Some(proto::signal::ByeReason::VideoFailed),
                })
            {
                found = true;
            }
        }
        assert!(found, "expected Bye with VideoFailed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_joined_sends_bye_when_session_start_fails() {
        let mut ctx = test_ctx();
        ctx.build_source = Arc::new(|_id, _exclude| anyhow::bail!("synthetic source failure"));
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
                session_id: "sess-1".to_string(),
                reason: None,
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

    /// Fail-closed: an unreadable/corrupt `access.json` must not be treated
    /// as "no password" -- the session is refused with `Bye`.
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_joined_with_unreadable_access_record_sends_bye_and_starts_nothing() {
        let mut ctx = test_ctx();
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-host-signaling-test-ctx-corrupt-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let store = crate::access::AccessStore::new(&dir);
        std::fs::write(store.path(), b"not json at all").expect("write corrupt record");
        ctx.access_store = store;

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
            out_rx.try_recv().expect("expected Bye"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: None,
            }
        );
        assert!(
            out_rx.try_recv().is_err(),
            "neither AuthRequired nor Offer may follow"
        );
        assert!(active.is_none());
        assert!(pending_auth.is_none());
        assert_eq!(current_session_id, None);

        let _ = std::fs::remove_dir_all(&dir);
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

        expect_final_stats(&mut out_rx);
        assert_eq!(
            out_rx.try_recv().expect("expected bye"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: None,
            }
        );
        assert!(active.is_none(), "session must have been ended");
        assert_eq!(current_session_id, None);
    }

    /// Slice 3.2e: an `Answer` whose `auth` tag verifies against the
    /// client's session key and the answer's own fingerprint must *not* be
    /// rejected by the binding check -- the host goes on to try `set_answer`
    /// (which fails here, since this is a synthetic SDP rather than a real
    /// DTLS answer). Slice E3: that failure now ends the session with a
    /// `Bye`, so the `Bye` below comes from `set_answer`, not the binding.
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

        // Slice E3: binding passed, `set_answer` failed on the synthetic SDP
        // -> the session is ended with a Bye.
        expect_final_stats(&mut out_rx);
        assert_eq!(
            out_rx
                .try_recv()
                .expect("expected bye after set_answer failure"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: None,
            }
        );
        assert!(active.is_none(), "session must have been ended");
        assert_eq!(current_session_id, None);
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
                reason: None,
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
                session_id: "sess-1".to_string(),
                reason: None,
            }
        );
    }

    /// Состояние цикла сигналинга для тестов M-11 (вытеснение сеанса).
    struct Slot {
        out_tx: mpsc::UnboundedSender<SignalMessage>,
        out_rx: mpsc::UnboundedReceiver<SignalMessage>,
        event_tx: mpsc::Sender<SessionEvent>,
        _event_rx: mpsc::Receiver<SessionEvent>,
        status_tx: watch::Sender<AgentStatus>,
        active: Option<ActiveSession>,
        current_session_id: Option<String>,
        pending_auth: Option<PendingAuth>,
        auth_failures: u32,
        auth_locked_until: Option<Instant>,
    }

    impl Slot {
        fn new() -> Self {
            let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalMessage>();
            let (event_tx, _event_rx) = mpsc::channel(64);
            let (status_tx, _) = watch::channel(AgentStatus::Connecting);
            Slot {
                out_tx,
                out_rx,
                event_tx,
                _event_rx,
                status_tx,
                active: None,
                current_session_id: None,
                pending_auth: None,
                auth_failures: 0,
                auth_locked_until: None,
            }
        }

        async fn peer_joined(&mut self, ctx: &HostContext, session_id: &str) {
            handle_signal_message(
                SignalMessage::PeerJoined {
                    session_id: session_id.to_string(),
                    ice_servers: vec![],
                },
                ctx,
                &[],
                "111111",
                &self.status_tx,
                &self.out_tx,
                self.event_tx.clone(),
                &mut self.active,
                &mut self.current_session_id,
                &mut self.pending_auth,
                &mut self.auth_failures,
                &mut self.auth_locked_until,
            )
            .await;
        }

        fn drain(&mut self) -> Vec<SignalMessage> {
            let mut all = Vec::new();
            while let Ok(m) = self.out_rx.try_recv() {
                all.push(m);
            }
            all
        }
    }

    fn is_bye_for(msg: &SignalMessage, id: &str) -> bool {
        matches!(msg, SignalMessage::Bye { session_id, .. } if session_id == id)
    }

    /// M-11: при заданном пароле новый `PeerJoined` не трогает работающий
    /// сеанс -- он гасится только после успешного входа.
    #[tokio::test(flavor = "multi_thread")]
    async fn peer_joined_with_password_keeps_active_session_until_login() {
        let mut ctx = test_ctx();
        let mut slot = Slot::new();
        slot.peer_joined(&ctx, "sess-1").await;
        assert!(slot.active.is_some(), "first session must start");
        slot.drain();
        let old_tag = slot.active.as_ref().unwrap().session_tag;

        ctx.access_store =
            test_ctx_with_password("keeps-active", "correct horse battery staple").access_store;
        slot.peer_joined(&ctx, "sess-2").await;

        let msgs = slot.drain();
        assert!(msgs.contains(&SignalMessage::AuthRequired {
            session_id: "sess-2".to_string()
        }));
        assert!(
            !msgs.iter().any(|m| is_bye_for(m, "sess-1")),
            "old session must not get a Bye: {msgs:?}"
        );
        assert_eq!(slot.active.as_ref().unwrap().session_tag, old_tag);
        assert!(!slot.active.as_ref().unwrap().peer.is_closed());
        assert_eq!(slot.current_session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            slot.pending_auth.as_ref().map(|p| p.session_id.as_str()),
            Some("sess-2")
        );

        // `Bye` кандидата снимает только его ожидание.
        handle_signal_message(
            SignalMessage::Bye {
                session_id: "sess-2".to_string(),
                reason: None,
            },
            &ctx,
            &[],
            "111111",
            &slot.status_tx,
            &slot.out_tx,
            slot.event_tx.clone(),
            &mut slot.active,
            &mut slot.current_session_id,
            &mut slot.pending_auth,
            &mut slot.auth_failures,
            &mut slot.auth_locked_until,
        )
        .await;
        assert!(slot.pending_auth.is_none());
        assert!(slot.active.is_some());
        assert_eq!(slot.current_session_id.as_deref(), Some("sess-1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auth_timeout_for_pending_login_leaves_active_session_alone() {
        let mut ctx = test_ctx();
        let mut slot = Slot::new();
        slot.peer_joined(&ctx, "sess-1").await;
        ctx.access_store =
            test_ctx_with_password("timeout-active", "correct horse battery staple").access_store;
        slot.peer_joined(&ctx, "sess-2").await;
        slot.drain();
        let generation = slot.pending_auth.as_ref().expect("armed").generation;

        handle_session_event(
            SessionEvent::AuthTimeout {
                session_id: "sess-2".to_string(),
                generation,
            },
            &ctx,
            "111111",
            &slot.status_tx,
            &slot.out_tx,
            &slot.event_tx,
            &mut slot.active,
            &mut slot.current_session_id,
            &mut slot.pending_auth,
        )
        .await;

        assert!(slot.pending_auth.is_none());
        let msgs = slot.drain();
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(is_bye_for(&msgs[0], "sess-2"));
        assert!(slot.active.is_some());
        assert!(!slot.active.as_ref().unwrap().peer.is_closed());
        assert_eq!(slot.current_session_id.as_deref(), Some("sess-1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_joined_without_password_supersedes_active_session() {
        let ctx = test_ctx();
        let mut slot = Slot::new();
        slot.peer_joined(&ctx, "sess-1").await;
        let old_tag = slot.active.as_ref().expect("first session").session_tag;
        let old_peer = Arc::clone(&slot.active.as_ref().unwrap().peer);
        slot.drain();

        slot.peer_joined(&ctx, "sess-2").await;

        assert!(old_peer.is_closed(), "old peer must be closed");
        let new = slot.active.as_ref().expect("new session");
        assert_ne!(new.session_tag, old_tag);
        assert_eq!(slot.current_session_id.as_deref(), Some("sess-2"));
        assert!(slot.drain().iter().any(
            |m| matches!(m, SignalMessage::Offer { session_id, .. } if session_id == "sess-2")
        ));
    }

    /// M-2: `SessionParts::abort` закрывает `PeerConnection`.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_parts_abort_closes_peer() {
        let ctx = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel(64);
        let parts = start_session(&ctx, &[], events_tx)
            .await
            .expect("session must start");
        let peer = Arc::clone(&parts.peer);
        assert!(!peer.is_closed());

        parts.abort().await;

        assert!(peer.is_closed());
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

        // Applied on the blocking pool (M-7): wait for it to land.
        for _ in 0..200 {
            if !fake_clipboard.set_calls().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

    /// Фейковая шторка (слайс 4.2c): пишет вызовы в общий журнал.
    struct FakeCurtain {
        log: Arc<Mutex<Vec<String>>>,
        ids: Vec<u32>,
        show_error: Option<String>,
        lock_error: Option<String>,
        shown: Mutex<Vec<u32>>,
    }

    impl Curtain for FakeCurtain {
        fn show(&self) -> anyhow::Result<Vec<u32>> {
            self.log.lock().unwrap().push("show".to_string());
            match &self.show_error {
                Some(msg) => Err(anyhow::anyhow!(msg.clone())),
                None => {
                    *self.shown.lock().unwrap() = self.ids.clone();
                    Ok(self.ids.clone())
                }
            }
        }
        fn hide(&self) {
            self.log.lock().unwrap().push("hide".to_string());
            self.shown.lock().unwrap().clear();
        }
        fn lock_screen(&self) -> anyhow::Result<()> {
            self.log.lock().unwrap().push("lock_screen".to_string());
            match &self.lock_error {
                Some(msg) => Err(anyhow::anyhow!(msg.clone())),
                None => Ok(()),
            }
        }
        fn shown(&self) -> Vec<u32> {
            self.shown.lock().unwrap().clone()
        }
    }

    type SourceCalls = Arc<Mutex<Vec<(u32, Vec<u32>)>>>;

    /// `test_ctx()` с фейковой шторкой и шпионом на `build_source`.
    fn curtain_ctx(
        ids: Vec<u32>,
        show_error: Option<&str>,
    ) -> (HostContext, Arc<Mutex<Vec<String>>>, SourceCalls) {
        curtain_ctx_with_lock_error(ids, show_error, None)
    }

    fn curtain_ctx_with_lock_error(
        ids: Vec<u32>,
        show_error: Option<&str>,
        lock_error: Option<&str>,
    ) -> (HostContext, Arc<Mutex<Vec<String>>>, SourceCalls) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let calls: SourceCalls = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = test_ctx();
        ctx.curtain = Some(Arc::new(FakeCurtain {
            log: Arc::clone(&log),
            ids,
            show_error: show_error.map(str::to_string),
            lock_error: lock_error.map(str::to_string),
            shown: Mutex::new(Vec::new()),
        }));
        let spy = Arc::clone(&calls);
        ctx.build_source = Arc::new(move |id, exclude| {
            spy.lock().unwrap().push((id, exclude.to_vec()));
            Ok(Box::new(capture::synthetic::for_display(id, 30)?) as Box<dyn FrameSource>)
        });
        (ctx, log, calls)
    }

    /// Запускает сеанс через `PeerJoined`, как остальные тесты. Возвращает
    /// каналы, чтобы они жили до конца теста.
    async fn start_active_session(
        ctx: &HostContext,
    ) -> (
        ActiveSession,
        mpsc::UnboundedReceiver<SignalMessage>,
        mpsc::Sender<SessionEvent>,
        mpsc::Receiver<SessionEvent>,
    ) {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<SignalMessage>();
        let (event_tx, event_rx) = mpsc::channel(64);
        let mut active: Option<ActiveSession> = None;
        let mut current_session_id: Option<String> = None;
        let (status_tx, _status_rx) = watch::channel(AgentStatus::Connecting);
        handle_signal_message(
            SignalMessage::PeerJoined {
                session_id: "sess-1".to_string(),
                ice_servers: vec![],
            },
            ctx,
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
        (
            active.expect("session must have started"),
            out_rx,
            event_tx,
            event_rx,
        )
    }

    fn journal(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        log.lock().unwrap().clone()
    }

    fn curtain_status(on: bool, reason: Option<&str>) -> ControlMessage {
        ControlMessage::CurtainStatus {
            on,
            reason: reason.map(str::to_string),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_curtain_without_curtain_is_refused() {
        let ctx = test_ctx();
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;

        let reply = set_curtain(&ctx, &mut active, true).await;

        assert_eq!(
            reply,
            curtain_status(false, Some("шторка недоступна на этом хосте"))
        );
        assert!(active.curtain_windows.is_empty());
        active.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_curtain_on_restarts_pipeline_excluding_curtain_windows() {
        let (ctx, log, calls) = curtain_ctx(vec![101, 102], None);
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
        let display_id = active.video.display.id;
        assert_eq!(*calls.lock().unwrap(), vec![(display_id, vec![])]);

        let reply = set_curtain(&ctx, &mut active, true).await;

        assert_eq!(reply, curtain_status(true, None));
        assert_eq!(journal(&log), vec!["show"]);
        assert_eq!(active.curtain_windows, vec![101, 102]);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(display_id, vec![]), (display_id, vec![101, 102])],
            "pipeline must be rebuilt on the same display with the curtain windows excluded"
        );

        // Повторное включение -- просто повтор статуса, без нового show.
        let again = set_curtain(&ctx, &mut active, true).await;
        assert_eq!(again, curtain_status(true, None));
        assert_eq!(journal(&log), vec!["show"]);
        assert_eq!(calls.lock().unwrap().len(), 2);

        active.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_curtain_show_error_reports_reason_and_keeps_pipeline() {
        let (ctx, log, calls) = curtain_ctx(vec![101], Some("нет прав"));
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;

        let reply = set_curtain(&ctx, &mut active, true).await;

        assert_eq!(reply, curtain_status(false, Some("нет прав")));
        assert!(active.curtain_windows.is_empty());
        assert_eq!(calls.lock().unwrap().len(), 1, "no pipeline rebuild");
        assert_eq!(journal(&log), vec!["show"]);

        active.shutdown().await;
        assert_eq!(
            journal(&log),
            vec!["show"],
            "curtain is off: nothing at shutdown"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_curtain_rolls_back_when_pipeline_restart_fails() {
        let (mut ctx, log, _calls) = curtain_ctx(vec![101], None);
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
        ctx.build_source = Arc::new(|_id, _exclude| anyhow::bail!("restart failure"));

        let reply = set_curtain(&ctx, &mut active, true).await;

        assert_eq!(reply, curtain_status(false, Some("restart failure")));
        assert!(active.curtain_windows.is_empty());
        assert_eq!(
            active.pipeline_generation.load(Ordering::SeqCst),
            0,
            "failed restart must hand the generation back to the running pipeline"
        );
        assert_eq!(journal(&log), vec!["show", "hide"]);

        active.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_curtain_off_hides_without_restarting_pipeline() {
        let (ctx, log, calls) = curtain_ctx(vec![101], None);
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
        set_curtain(&ctx, &mut active, true).await;
        assert_eq!(calls.lock().unwrap().len(), 2);

        let reply = set_curtain(&ctx, &mut active, false).await;

        assert_eq!(reply, curtain_status(false, None));
        assert_eq!(journal(&log), vec!["show", "hide"]);
        assert!(active.curtain_windows.is_empty());
        assert_eq!(calls.lock().unwrap().len(), 2, "no pipeline rebuild on off");

        active.shutdown().await;
        assert_eq!(journal(&log), vec!["show", "hide"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_with_curtain_on_locks_screen_before_hiding() {
        let (ctx, log, _calls) = curtain_ctx(vec![101], None);
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
        set_curtain(&ctx, &mut active, true).await;

        active.shutdown().await;

        assert_eq!(journal(&log), vec!["show", "lock_screen", "hide"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_keeps_curtain_when_lock_fails() {
        let (ctx, log, _calls) = curtain_ctx_with_lock_error(vec![101], None, Some("no lock"));
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
        set_curtain(&ctx, &mut active, true).await;

        active.shutdown().await;

        assert_eq!(journal(&log), vec!["show", "lock_screen"]);
        assert!(!ctx.curtain.as_ref().unwrap().shown().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_session_excludes_already_shown_curtain_windows() {
        let (ctx, _log, calls) = curtain_ctx(vec![101, 102], None);
        ctx.curtain.as_ref().unwrap().show().unwrap();

        let (active, _out, _etx, _erx) = start_active_session(&ctx).await;

        assert_eq!(active.curtain_windows, vec![101, 102]);
        let first = calls.lock().unwrap().first().cloned().unwrap();
        assert_eq!(first.1, vec![101, 102]);
        active.shutdown().await;
    }

    /// Перехватить control-сообщения в тестах нельзя без расширения
    /// инфраструктуры `PeerSession`, поэтому проверяем чистую функцию,
    /// из которой `DataChannelOpen` берёт сообщение.
    #[test]
    fn control_open_announces_curtain_on_when_already_shown() {
        assert_eq!(
            curtain_announce_message(&[101, 102]),
            Some(curtain_status(true, None))
        );
        assert_eq!(curtain_announce_message(&[]), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_without_curtain_never_touches_it() {
        let (ctx, log, _calls) = curtain_ctx(vec![101], None);
        let (active, _out, _etx, _erx) = start_active_session(&ctx).await;

        active.shutdown().await;

        assert!(journal(&log).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn switching_display_with_curtain_on_excludes_curtain_windows() {
        let (ctx, _log, calls) = curtain_ctx(vec![101, 102], None);
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
        set_curtain(&ctx, &mut active, true).await;
        let other = capture::synthetic::list_displays()
            .into_iter()
            .find(|d| d.id != active.video.display.id)
            .unwrap()
            .id;

        switch_display(&ctx, &mut active, other).await;

        assert_eq!(active.video.display.id, other);
        assert_eq!(
            calls.lock().unwrap().last().cloned(),
            Some((other, vec![101, 102]))
        );
        // set_curtain + switch_display: two replacements -> generation 2.
        assert_eq!(active.pipeline_generation.load(Ordering::SeqCst), 2);
        active.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn select_display_is_rate_limited_within_a_second() {
        let (ctx, _log, calls) = curtain_ctx(vec![], None);
        let (mut active, _out, _etx, _erx) = start_active_session(&ctx).await;
        let first = active.video.display.id;
        let other = capture::synthetic::list_displays()
            .into_iter()
            .find(|d| d.id != first)
            .unwrap()
            .id;
        let base = calls.lock().unwrap().len();

        switch_display(&ctx, &mut active, other).await;
        assert_eq!(active.video.display.id, other);
        assert_eq!(calls.lock().unwrap().len(), base + 1);

        // Сразу обратно: лимит, конвейер не пересобирается.
        switch_display(&ctx, &mut active, first).await;
        assert_eq!(active.video.display.id, other);
        assert_eq!(calls.lock().unwrap().len(), base + 1);

        active.last_display_switch =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        switch_display(&ctx, &mut active, first).await;
        assert_eq!(active.video.display.id, first);
        assert_eq!(calls.lock().unwrap().len(), base + 2);
        active.shutdown().await;
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
        expect_final_stats(&mut out_rx);
        assert_eq!(
            out_rx.try_recv().expect("expected a Bye message"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: None,
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
                session_id: "sess-1".to_string(),
                reason: None,
            }
        );
        assert!(
            !status_rx.has_changed().unwrap(),
            "status must not change -- it was already Registered"
        );
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
        expect_final_stats(&mut out_rx);
        assert_eq!(
            out_rx.try_recv().expect("expected a Bye message"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: None,
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
        expect_final_stats(&mut out_rx);
        assert_eq!(
            out_rx.try_recv().expect("expected a Bye message"),
            SignalMessage::Bye {
                session_id: "sess-1".to_string(),
                reason: None,
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
