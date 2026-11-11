//! Session startup: building the `PeerSession` and video pipeline for one
//! joining peer (`start_session`), the display wait after wake, ICE server
//! selection, and the cursor and Windows elevation background watchers.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use tokio::sync::mpsc;

use proto::control::ControlMessage;
use proto::signal::SignalMessage;

use crate::capture::{self, DisplayInfo};
use crate::clipboard::{self, ClipboardEvent, ClipboardSync};
use crate::cursor::{self, CursorState};
use crate::encode::EncodedFrame;
use crate::files::FilesCommand;
use crate::input::{Injector, InputRouter, NoopInjector};
#[cfg(target_os = "windows")]
use crate::platform;
use crate::transport::{PeerSession, SessionConfig, SessionEvent};

use super::{HostContext, VideoPipeline};

/// How often the cursor watcher thread polls the platform for the current
/// system cursor shape (see `crate::cursor::watch`). 33ms is roughly 30Hz --
/// plenty for a cursor shape, which changes far less often than the pointer
/// moves.
const CURSOR_POLL_INTERVAL: Duration = Duration::from_millis(33);

/// How often the cursor task retries a cursor message that `send_control`
/// could not deliver because the `control` data channel was not open yet.
const CURSOR_RETRY_INTERVAL: Duration = Duration::from_millis(100);

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

/// What `start_session` hands back to `handle_signal_message` for one
/// joining peer: the `PeerSession` plus every background task/resource that
/// makes up the running session (see `ActiveSession`, which this gets moved
/// into once the offer is sent).
pub(super) struct SessionParts {
    pub(super) peer: Arc<PeerSession>,
    pub(super) video: VideoPipeline,
    pub(super) video_tx: mpsc::Sender<EncodedFrame>,
    pub(super) keyframe_flag: Arc<AtomicBool>,
    /// See `ActiveSession::pipeline_generation`.
    pub(super) pipeline_generation: Arc<AtomicU64>,
    pub(super) displays: Vec<DisplayInfo>,
    pub(super) router: InputRouter,
    pub(super) cursor_task: tokio::task::JoinHandle<()>,
    /// See `ActiveSession::input_available`.
    pub(super) input_available: bool,
    /// See `ActiveSession::input_reason`.
    pub(super) input_reason: Option<String>,
    /// See `ActiveSession::clipboard`.
    pub(super) clipboard: Option<Arc<Mutex<ClipboardSync>>>,
    /// See `ActiveSession::clipboard_task`.
    pub(super) clipboard_task: Option<tokio::task::JoinHandle<()>>,
    /// See `ActiveSession::elevation_task`.
    pub(super) elevation_task: Option<tokio::task::JoinHandle<()>>,
    /// See `ActiveSession::files_task`.
    pub(super) files_task: tokio::task::JoinHandle<()>,
    /// See `ActiveSession::files_enabled`.
    pub(super) files_enabled: bool,
    /// See `ActiveSession::curtain_windows`.
    pub(super) curtain_windows: Vec<u32>,
    /// See `ActiveSession::_display_wake`.
    pub(super) _display_wake: std::sync::Mutex<Box<dyn Send>>,
}

impl SessionParts {
    /// Tears down a session that was started but never became active:
    /// stops the pipeline and background tasks, then closes the peer
    /// (webrtc-rs does not close a `PeerConnection` on `Drop`).
    pub(super) async fn abort(self) {
        self.video.stop();
        self.cursor_task.abort();
        if let Some(task) = self.clipboard_task {
            task.abort();
        }
        if let Some(task) = self.elevation_task {
            task.abort();
        }
        self.files_task.abort();
        self.peer.close().await;
    }
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
pub(super) fn peer_ice_servers<'a>(
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

/// Сколько ждать появления дисплея после пробуждения (4.6a) и с каким шагом.
const DISPLAY_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const DISPLAY_WAIT_STEP: Duration = Duration::from_millis(250);

/// Дисплей хоста недоступен: список пуст или нужного id нет даже после
/// пробуждения и ожидания (слайс 4.6b). Типизирована, чтобы
/// `begin_session` отличил её от прочих сбоев и сообщил клиенту причину
/// (`ByeReason::DisplayUnavailable`).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct DisplayUnavailable(String);

/// `Bye` для сеанса, который не удалось поднять: с причиной, если дело в
/// недоступном дисплее, иначе без неё.
pub(super) fn bye_for_start_error(session_id: String, err: &anyhow::Error) -> SignalMessage {
    let reason = err
        .downcast_ref::<DisplayUnavailable>()
        .map(|_| proto::signal::ByeReason::DisplayUnavailable);
    SignalMessage::Bye { session_id, reason }
}

/// Повторяет `attempt` с шагом `step`, пока он не вернёт `Ok` или не
/// истечёт `timeout`; после таймаута возвращает последнюю ошибку. Первая
/// удачная попытка -- без задержки. Если пришлось ждать, пишет `info`.
async fn wait_for_value<T>(
    mut attempt: impl FnMut() -> anyhow::Result<T>,
    timeout: Duration,
    step: Duration,
) -> anyhow::Result<T> {
    let mut waited = Duration::ZERO;
    loop {
        match attempt() {
            Ok(value) => {
                if !waited.is_zero() {
                    tracing::info!(
                        waited_ms = waited.as_millis() as u64,
                        "display appeared after wake"
                    );
                }
                return Ok(value);
            }
            Err(err) => {
                if waited >= timeout {
                    return Err(err);
                }
                tokio::time::sleep(step).await;
                waited += step;
            }
        }
    }
}

/// Builds the video pipeline and the `PeerSession` for one joining peer, and
/// starts the task that feeds encoded frames from the pipeline into the
/// session's video track. `registered_ice_servers` are this session's own
/// ICE credentials, chosen by `peer_ice_servers` at the call site (see
/// `session_ice_servers`) -- despite the name, since 2.6b this is usually
/// the joining peer's `PeerJoined` credentials, not the host's `Registered`
/// ones.
pub(super) async fn start_session(
    ctx: &HostContext,
    registered_ice_servers: &[proto::signal::IceServer],
    events: mpsc::Sender<SessionEvent>,
) -> anyhow::Result<SessionParts> {
    start_session_with_display_wait(ctx, registered_ice_servers, events, DISPLAY_WAIT_TIMEOUT).await
}

/// `start_session` с явным таймаутом ожидания дисплея (в тестах -- крошечным,
/// в бою -- `DISPLAY_WAIT_TIMEOUT`).
pub(super) async fn start_session_with_display_wait(
    ctx: &HostContext,
    registered_ice_servers: &[proto::signal::IceServer],
    events: mpsc::Sender<SessionEvent>,
    display_wait_timeout: Duration,
) -> anyhow::Result<SessionParts> {
    // Слайс 4.6a: будим дисплей и ждём, пока он появится в списке. Слайс I7:
    // guard НЕ отпускаем после выбора -- macOS гасит разбуженный дисплей через
    // ~10 с, поэтому он продлевает активность весь сеанс (живёт в
    // `SessionParts` -> `ActiveSession`, дропается вместе с сеансом).
    let wake_guard = (ctx.wake_display)();
    let (displays, start_display) = wait_for_value(
        || {
            let displays = (ctx.list_displays)()?;
            let chosen = match ctx.display {
                Some(id) => displays
                    .iter()
                    .find(|d| d.id == id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::Error::new(DisplayUnavailable(format!("display {id} not found")))
                    })?,
                None => capture::default_display(&displays)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::Error::new(DisplayUnavailable(
                            "no capturable display found".to_string(),
                        ))
                    })?,
            };
            Ok((displays, chosen))
        },
        display_wait_timeout,
        DISPLAY_WAIT_STEP,
    )
    .await?;

    let keyframe_flag = Arc::new(AtomicBool::new(false));
    let pipeline_generation = Arc::new(AtomicU64::new(0));
    let (video_tx, video_rx) = mpsc::channel(4);

    let session_config = SessionConfig {
        ice_servers: session_ice_servers(registered_ice_servers, &ctx.session.ice_servers),
        ..ctx.session.clone()
    };
    let peer = PeerSession::new(session_config, events, Arc::clone(&ctx.runtime)).await?;
    // webrtc-rs does not close a `PeerConnection` on `Drop` (the UDP socket and
    // ICE tasks live on), so any failure after `PeerSession::new` closes it
    // explicitly (audit M-2).
    let started: anyhow::Result<SessionParts> = async {
        // Called once per session: every `VideoPipeline` (including ones built
        // later by `switch_display`) feeds frames into the same `video_tx`/
        // `video_rx` pair and shares `keyframe_flag` -- see their doc comments.
        peer.start_video(video_rx, Arc::clone(&keyframe_flag));

        // Шторка могла остаться от прошлого сеанса: исключаем её окна из захвата.
        let curtain_windows = ctx.curtain.as_ref().map(|c| c.shown()).unwrap_or_default();
        let video = VideoPipeline::start(
            ctx,
            start_display,
            Arc::clone(&peer),
            video_tx.clone(),
            Arc::clone(&keyframe_flag),
            0,
            Arc::clone(&pipeline_generation),
            &curtain_windows,
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
        // `cursor::watch`'s doc comment), and the task below keeps retrying it
        // while `send_control` returns `Ok(false)` (the `control` channel is
        // not open yet), so this newly connected client gets the host's
        // current cursor shape right away once the channel opens.
        let cursor_source = (ctx.build_cursor_source)();
        let mut cursor_watcher = cursor::watch(cursor_source, CURSOR_POLL_INTERVAL);
        let cursor_peer = Arc::clone(&peer);
        let cursor_task = tokio::spawn(async move {
            // The last cursor message not yet delivered. `send_control` returns
            // `Ok(false)` while the `control` channel is not open yet (the
            // watcher's first poll happens at session start, before that), and
            // the watcher will not resend a shape it already reported, so we
            // retry the latest pending state until the channel accepts it.
            let mut pending: Option<ControlMessage> = None;
            loop {
                if let Some(msg) = &pending {
                    let result = cursor_peer.send_control(msg).await;
                    if let Err(err) = &result {
                        tracing::warn!(?err, "failed to send cursor control message");
                    }
                    if !cursor_send_keeps_pending(&result) {
                        pending = None;
                    }
                }
                tokio::select! {
                    state = cursor_watcher.rx.recv() => match state {
                        Some(state) => pending = Some(cursor_state_to_control(state)),
                        None => break,
                    },
                    _ = tokio::time::sleep(CURSOR_RETRY_INTERVAL), if pending.is_some() => {}
                }
            }
            // `rx` only closes when the watcher's thread is gone. On an orderly
            // shutdown this task is aborted first (`ActiveSession::shutdown`),
            // so reaching this line means the thread died mid-session (slice A
            // of the 2026-10-13 audit: a panic is logged by the hook in
            // `agent::logging`, but nothing said the session lost its cursor).
            tracing::warn!("cursor watcher thread exited, cursor shape updates stop for this session");
        });

        // The `file` channel gets its own task (slice 3.4a), fed by the
        // transport's dedicated mpsc rather than `handle_session_event`. A
        // disabled or folder-less session still runs the task so the client
        // gets a clean `Abort { Disabled }` instead of silence.
        // The download folder is resolved once, at session start. In machine
        // mode (Windows helper under SYSTEM) it is the Downloads of the user
        // logged on to the console session; if the session began on the logon
        // screen, files stay disabled in it until the client reconnects.
        // The same goes for whose rights file operations use (D61): in machine
        // mode the logged-on user's, never SYSTEM's.
        let (files_dir, files_user) = if ctx.files_enabled {
            match crate::files::receive_dir(ctx.machine_mode)
                .and_then(|dir| crate::files::receive_fs_user(ctx.machine_mode).map(|u| (dir, u)))
            {
                Ok((dir, user)) => (Some(dir), user),
                Err(err) => {
                    tracing::warn!(error = %err, "download folder unavailable, file transfer disabled");
                    (None, crate::files::FsUser::current())
                }
            }
        } else {
            (None, crate::files::FsUser::current())
        };
        let files_enabled = files_dir.is_some();
        // Copied-file offers travel from the clipboard watcher to the files task
        // (slice 3.4c); without a clipboard the sender is simply dropped.
        let (files_cmd_tx, files_cmd_rx) = mpsc::channel::<crate::files::FilesCommand>(8);

        // A failed/absent clipboard builder must not take the session down
        // either -- same lesson as the injector fallback above (debt D26): the
        // session just runs without clipboard sync.
        let (clipboard, clipboard_task) = match &ctx.build_clipboard {
            Some(build_clipboard) => match build_clipboard() {
                Ok(backend) => {
                    let mut clipboard_sync = ClipboardSync::new(backend);
                    clipboard_sync.set_files_enabled(files_enabled);
                    tracing::info!(files_enabled, "clipboard sync started");
                    let sync = Arc::new(Mutex::new(clipboard_sync));
                    let mut clipboard_watcher =
                        clipboard::watch(Arc::clone(&sync), clipboard::CLIPBOARD_POLL_INTERVAL);
                    let clipboard_peer = Arc::clone(&peer);
                    let task = tokio::spawn(async move {
                        while let Some(event) = clipboard_watcher.rx.recv().await {
                            match event {
                                ClipboardEvent::Text(text) => {
                                    let msg = ControlMessage::ClipboardText { text };
                                    if let Err(err) = clipboard_peer.send_control(&msg).await {
                                        tracing::warn!(
                                            ?err,
                                            "failed to send clipboard control message"
                                        );
                                    }
                                }
                                ClipboardEvent::Files(paths) => {
                                    let _ = files_cmd_tx.send(FilesCommand::Offer(paths)).await;
                                }
                                ClipboardEvent::NoFiles => {
                                    let _ = files_cmd_tx.send(FilesCommand::Clear).await;
                                }
                            }
                        }
                        // Same reasoning as `cursor_task` above: only a dead
                        // watcher thread closes `rx` while this task is alive.
                        tracing::warn!(
                            "clipboard watcher thread exited, clipboard sync stops for this session"
                        );
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

        let Some((file_channel, file_rx)) = peer.take_file_channel() else {
            // Everything above is already running: tear it down here, the
            // caller only closes the peer.
            video.stop();
            cursor_task.abort();
            if let Some(task) = clipboard_task {
                task.abort();
            }
            if let Some(task) = elevation_task {
                task.abort();
            }
            anyhow::bail!("file channel already taken");
        };
        let files_task = tokio::spawn(crate::files::run(
            Arc::new(file_channel) as Arc<dyn crate::files::FileChannel>,
            file_rx,
            files_dir,
            files_user,
            files_cmd_rx,
        ));

        Ok(SessionParts {
            peer: Arc::clone(&peer),
            video,
            video_tx,
            keyframe_flag,
            pipeline_generation,
            displays,
            router,
            cursor_task,
            input_available,
            input_reason,
            clipboard,
            clipboard_task,
            elevation_task,
            files_task,
            files_enabled,
            curtain_windows,
            _display_wake: std::sync::Mutex::new(wake_guard),
        })
    }
    .await;
    if started.is_err() {
        peer.close().await;
    }
    started
}

/// Converts a cursor-shape change into the wire message `send_control`
/// sends, base64-encoding the raw RGBA bytes (see
/// `proto::control::ControlMessage::CursorShape`'s doc comment).
/// Whether a cursor message should stay pending after a send attempt:
/// only `Ok(false)` (channel not open yet) is retried; `Ok(true)` is delivered
/// and `Err` means a broken channel, which is not hammered.
fn cursor_send_keeps_pending(result: &anyhow::Result<bool>) -> bool {
    matches!(result, Ok(false))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // `tokio/test-util` в воркспейсе не включён, поэтому вместо paused-времени
    // берём реальные, но крошечные шаги/таймаут.
    const T_STEP: Duration = Duration::from_millis(5);
    const T_TIMEOUT: Duration = Duration::from_millis(60);

    #[test]
    fn cursor_send_keeps_pending_only_when_channel_not_open() {
        assert!(!cursor_send_keeps_pending(&Ok(true)));
        assert!(cursor_send_keeps_pending(&Ok(false)));
        assert!(!cursor_send_keeps_pending(&Err(anyhow::anyhow!("boom"))));
    }

    #[tokio::test]
    async fn wait_for_value_first_attempt_no_sleep() {
        let mut calls = 0;
        let started = Instant::now();
        let v = wait_for_value(
            || {
                calls += 1;
                Ok(7)
            },
            Duration::from_secs(5),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(v, 7);
        assert_eq!(calls, 1);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn wait_for_value_succeeds_on_nth_attempt() {
        let mut calls = 0;
        let v = wait_for_value(
            || {
                calls += 1;
                if calls < 4 {
                    Err(anyhow::anyhow!("not yet"))
                } else {
                    Ok(calls)
                }
            },
            T_TIMEOUT,
            T_STEP,
        )
        .await
        .unwrap();
        assert_eq!(v, 4);
    }

    #[tokio::test]
    async fn wait_for_value_times_out_with_last_error() {
        let mut calls = 0;
        let err = wait_for_value(
            || -> anyhow::Result<()> {
                calls += 1;
                Err(anyhow::anyhow!("attempt {calls}"))
            },
            T_TIMEOUT,
            T_STEP,
        )
        .await
        .unwrap_err();
        assert!(calls > 1);
        assert_eq!(err.to_string(), format!("attempt {calls}"));
    }

    #[tokio::test]
    async fn wait_for_value_retries_after_list_error_then_succeeds() {
        let mut calls = 0;
        let v = wait_for_value(
            || {
                calls += 1;
                match calls {
                    1 => Err(anyhow::anyhow!("list failed")),
                    2 => Err(anyhow::anyhow!("no capturable display found")),
                    _ => Ok("display"),
                }
            },
            T_TIMEOUT,
            T_STEP,
        )
        .await
        .unwrap();
        assert_eq!(v, "display");
        assert_eq!(calls, 3);
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
}
