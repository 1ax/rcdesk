//! Per-session state: the active session and its video pipeline, the
//! disconnect grace timer, pending OPAQUE auth and the `SessionSlot` that
//! outlives signaling reconnects, plus the auth pacing constants.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use proto::control::QualityPreset;
use proto::signal::SignalMessage;

use crate::adapt::{AdaptConfig, Feedback};
use crate::capture::DisplayInfo;
use crate::clipboard::ClipboardSync;
use crate::curtain::Curtain;
use crate::encode::{build_encoder, EncodedFrame, EncoderConfig, RateTarget};
use crate::input::InputRouter;
use crate::input_source::InputSourceSwitcher;
use crate::pipeline::Pipeline;
use crate::transport::{PeerSession, SessionEvent};

use super::display::{spawn_pipeline_watchdog, RecoveryBudget};
use super::handlers::{handle_agent_command, handle_session_event};
use super::stats::{run_adapt_task, send_session_stats};
use super::{AgentCommand, AgentStatus, HostContext};

/// Slice 3.5b: how long a `RTCPeerConnectionState::Disconnected` is
/// tolerated before the session is torn down. ICE can flap through
/// `Disconnected` on a brief network hiccup (a Wi-Fi roam, a dropped packet
/// train) and recover to `Connected` on its own -- only `Failed`/`Closed`,
/// or this grace window expiring without a recovery, actually end the
/// session. See `handle_session_event`'s `ConnectionState`/
/// `DisconnectTimeout` arms.
pub(super) const DISCONNECT_GRACE: Duration = Duration::from_secs(15);

/// Slice 3.2c: how long a `PendingAuth` (armed on `AuthRequired`, see
/// `handle_signal_message`'s `PeerJoined` arm) waits for the client to
/// finish its OPAQUE login before the host gives up and sends `Bye`. Same
/// self-timer pattern as `DISCONNECT_GRACE`/`SessionEvent::DisconnectTimeout`.
pub(super) const AUTH_TIMEOUT: Duration = Duration::from_secs(120);

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
pub(super) const MAX_AUTH_ATTEMPTS: u32 = 5;

/// Slice 3.2c: hands out a fresh, process-wide unique generation for a
/// `PendingAuth`'s `AuthTimeout` timer -- same pattern as
/// `transport::next_session_tag`, for the same reason: distinguishing a
/// timeout for a `PendingAuth` that's since been replaced (a new
/// `PeerJoined`) or resolved (a successful `PakeFinish`) from the one
/// actually still pending.
static NEXT_AUTH_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(super) fn next_auth_generation() -> u64 {
    NEXT_AUTH_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Slice 3.2c: the throttling pause armed before the *next* `PakeStart`,
/// given `auth_failures` consecutive failed attempts so far -- only
/// meaningful once `auth_failures > MAX_AUTH_ATTEMPTS` (i.e.
/// `auth_failures >= 6`, the caller's guard): 5s after the 6th attempt,
/// doubling after each further attempt, capped at 60s.
pub(super) fn auth_lockout_duration(auth_failures: u32) -> Duration {
    let exponent = auth_failures.saturating_sub(6).min(63);
    let secs = 5u64.saturating_mul(1u64 << exponent);
    Duration::from_secs(secs.min(60))
}

/// The video pipeline (capture + encode) for one displayed screen, plus the
/// task forwarding its encoded frames into the session's video track and
/// (when `HostContext::adapt`) the adaptation controller task. Rebuilt from
/// scratch by `switch_display` on every `ControlMessage::SelectDisplay` --
/// the `PeerConnection`/`PeerSession` are not touched, only this piece (see
/// slice 2.4's plan: switching displays does not recreate the
/// `PeerConnection`).
pub(super) struct VideoPipeline {
    pub(super) display: DisplayInfo,
    /// Tells the forward task (spawned detached in `start`: dropping a tokio
    /// `JoinHandle` does not cancel the task) to stop reading frames and hand
    /// the pipeline off to `spawn_blocking` -- see `start`.
    stop_tx: Option<oneshot::Sender<()>>,
    pub(super) adapt_tx: Option<mpsc::UnboundedSender<Feedback>>,
    adapt_task: Option<JoinHandle<()>>,
    /// V-2: watchdog зависания конвейера (`StallDetector`); умирает вместе с
    /// конвейером (`stop` его abort-ит), работает и при `adapt == false`.
    watchdog_task: JoinHandle<()>,
    /// Test-only escape hatch onto the pipeline's `RateControl` (slice
    /// 3.5e), so a test can observe `ControlMessage::SetQuality` actually
    /// reaching the adaptation controller (via `run_adapt_task`'s real
    /// ticker) without a real WebRTC peer to read `ControlMessage::Quality`
    /// back off of -- see `switching_quality_preset_reaches_the_adapt_controller`.
    #[cfg(test)]
    pub(super) rate_control: Arc<crate::pipeline::RateControl>,
}

impl VideoPipeline {
    /// Builds the encoder/pipeline for `display` and starts forwarding its
    /// encoded frames into `video_tx` (the channel `PeerSession::start_video`
    /// reads from). `keyframe_flag` is the *session's* shared flag (set by
    /// `start_session`, not created here): `PeerSession::start_video` is
    /// called once per session, so the flag it was given must keep being the
    /// one this pipeline's encode thread honors even after a display switch.
    ///
    /// `generation`/`current_generation` (D24): this pipeline's generation and
    /// the session-wide "newest generation" counter. The forward task drops
    /// frames once `current_generation > generation`, i.e. once a newer
    /// pipeline exists, so late frames of a replaced pipeline can never land
    /// in `video_tx` after the newer pipeline's first keyframe.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn start(
        ctx: &HostContext,
        display: DisplayInfo,
        peer: Arc<PeerSession>,
        video_tx: mpsc::Sender<EncodedFrame>,
        keyframe_flag: Arc<AtomicBool>,
        generation: u64,
        current_generation: Arc<AtomicU64>,
        exclude_windows: &[u32],
    ) -> anyhow::Result<VideoPipeline> {
        let fps = ctx.session.fps.max(1);
        // V-1: сборка захвата и кодера (MFT/WGC на Win10 -- сотни мс, порой
        // секунды) синхронная и не должна занимать tokio-воркер, на котором
        // живёт сигналинг; уносим её в blocking-пул.
        let build_source = Arc::clone(&ctx.build_source);
        let encoder_choice = ctx.encoder;
        let bitrate_kbps = ctx.bitrate_kbps;
        let max_qp = ctx.max_qp;
        let build_display_id = display.id;
        let build_exclude = exclude_windows.to_vec();
        let (source, width, height, encoder, encoder_kind) =
            tokio::task::spawn_blocking(move || {
                let source = build_source(build_display_id, &build_exclude)?;
                let (width, height) = source.size();
                let encoder_cfg = EncoderConfig {
                    width,
                    height,
                    fps,
                    bitrate_kbps,
                    // Periodic IDR is only a safety net; recovery after loss
                    // goes via PLI/FIR. Every 10 s it cost ~250 kbit/s and a
                    // burst of packets for nothing (audit 2026-10-13).
                    keyframe_interval_frames: fps * 60,
                    max_qp,
                };
                let (encoder, encoder_kind) = build_encoder(encoder_choice, encoder_cfg)?;
                anyhow::Ok((source, width, height, encoder, encoder_kind))
            })
            .await
            .map_err(|err| anyhow::Error::new(err).context("video pipeline build task failed"))??;
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
        let watchdog_task = spawn_pipeline_watchdog(Arc::clone(&pipeline_stats), Arc::clone(&peer));
        let failure_peer = Arc::clone(&peer);
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut handle = handle;
            // Frames skipped because a newer pipeline took over (D24);
            // reported once on exit instead of per frame.
            let mut superseded_drops: u64 = 0;
            loop {
                tokio::select! {
                    frame = handle.frames.recv() => {
                        let Some(frame) = frame else {
                            // D58: the encode thread exited (it dropped
                            // `tx`) while we were not asked to stop: the
                            // capture or the encoder failed.
                            tracing::error!(
                                "video pipeline stopped unexpectedly, ending session"
                            );
                            let _ = failure_peer
                                .events()
                                .send(SessionEvent::VideoFailed {
                                    tag: failure_peer.tag(),
                                })
                                .await;
                            break;
                        };
                        if current_generation.load(Ordering::SeqCst) > generation {
                            superseded_drops += 1;
                            continue;
                        }
                        if let Some(tx) = &frame_feedback_tx {
                            let _ = tx.send(Feedback::Frame {
                                bytes: frame.data.len(),
                                captured_at: frame.captured_at,
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
            if superseded_drops > 0 {
                tracing::debug!(
                    generation,
                    superseded_drops,
                    "dropped frames of a superseded video pipeline"
                );
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
            watchdog_task,
            #[cfg(test)]
            rate_control: rate_control_for_test,
        })
    }

    /// Stops the forward task (which in turn stops the pipeline's
    /// capture/encode threads, see `start`'s doc comment) and the adapt
    /// task. The forward task exits on its own once `stop_tx` fires and
    /// hands the pipeline off to `spawn_blocking`.
    pub(super) fn stop(self) {
        if let Some(tx) = self.stop_tx {
            let _ = tx.send(());
        }
        if let Some(task) = self.adapt_task {
            task.abort();
        }
        self.watchdog_task.abort();
    }
}

/// A running `PeerSession` plus the task feeding it encoded frames from the
/// video pipeline.
pub(super) struct ActiveSession {
    pub(super) peer: Arc<PeerSession>,
    pub(super) video: VideoPipeline,
    /// Channel `VideoPipeline::forward_task` writes into and
    /// `PeerSession::start_video` reads from; kept here so `switch_display`
    /// can start a replacement pipeline feeding the same channel.
    pub(super) video_tx: mpsc::Sender<EncodedFrame>,
    /// The session-wide keyframe-request flag handed to `PeerSession::start_video`
    /// once at session start; every `VideoPipeline` (including ones built by
    /// `switch_display`) shares it (see `Pipeline::start_with_keyframe_flag`'s
    /// doc comment).
    pub(super) keyframe_flag: Arc<AtomicBool>,
    /// Newest `VideoPipeline` generation of the session (D24). All pipelines
    /// share `video_tx`, and `replace_pipeline` starts the new one before
    /// stopping the old, so for a moment both forward tasks write into the same
    /// channel. `replace_pipeline` bumps this counter *before* starting the
    /// new pipeline; a forward task drops frames once the counter exceeds its
    /// own generation, so nothing from the old pipeline can follow the new
    /// one's first keyframe. The session's first pipeline is generation 0.
    pub(super) pipeline_generation: Arc<AtomicU64>,
    /// The most recently listed set of displays, sent to the client in
    /// `ControlMessage::Displays` (see `displays_message`).
    pub(super) displays: Vec<DisplayInfo>,
    /// Routes `input`/`pointer` data channel messages to the platform
    /// injector. Dropped at the end of `shutdown` (ordinary field drop),
    /// which -- per `InputRouter`'s own `Drop` impl -- releases any
    /// keys/buttons the session left held.
    pub(super) router: InputRouter,
    /// When the last "input events dropped" warning was logged (throttle).
    pub(super) last_input_drop_warn: Option<Instant>,
    /// Input events dropped (rate limit / full queue) since that warning.
    pub(super) input_dropped: u64,
    /// The task that reads cursor-shape changes and forwards them over
    /// `control` (see `start_session`). It owns the session's
    /// `cursor::CursorWatcher` itself: `CursorWatcher` implements `Drop` to
    /// stop its background polling thread, which makes it impossible to
    /// partially move its `rx` field out of a separately-held watcher (Rust
    /// disallows moving fields out of any type that implements `Drop`) --
    /// so instead the watcher lives inside this task's future, and aborting
    /// the task (below) drops that future, which drops the watcher, which
    /// stops the thread.
    pub(super) cursor_task: tokio::task::JoinHandle<()>,
    /// Whether `start_session` obtained a real input injector for this
    /// session (slice 2.5a, debt D26). `false` means the session is
    /// view-only -- `build_injector` failed (e.g. missing the macOS
    /// Accessibility permission, or `serve --no-input`) and a `NoopInjector`
    /// was substituted so the session still streams video instead of dying.
    /// Sent to the client as `ControlMessage::InputStatus` once the
    /// `control` channel opens.
    pub(super) input_available: bool,
    /// Human-readable reason for `input_available == false` (the error from
    /// `build_injector`), or `None` when input is available.
    pub(super) input_reason: Option<String>,
    /// Clipboard sync state for this session (slice 2.5b), shared between
    /// `clipboard_task`'s watcher thread and
    /// `handle_session_event`'s handling of `InputMessage::ClipboardText` on
    /// the `input` channel. `None` when clipboard sync is disabled
    /// (`HostContext::build_clipboard` is `None`) or unavailable for this
    /// session (the builder returned `Err`) -- the session runs without it
    /// either way, same as a missing input injector (debt D26).
    pub(super) clipboard: Option<Arc<Mutex<ClipboardSync>>>,
    /// The task that reads clipboard text changes and forwards them over
    /// `control`, mirroring `cursor_task` -- see that field's doc comment
    /// for why the watcher lives inside the task's future rather than being
    /// held separately. `None` alongside `clipboard: None`.
    pub(super) clipboard_task: Option<tokio::task::JoinHandle<()>>,
    /// The Windows foreground-window elevation watcher (slice 2.6e), polling
    /// `platform::windows::elevation::foreground_input_blocked` once a
    /// second and forwarding changes as `ControlMessage::InputBlocked`.
    /// `None` on non-Windows platforms, and on Windows too when the session
    /// has no real input injector (`input_available == false`) -- nothing
    /// to warn about when input isn't wired up at all.
    pub(super) elevation_task: Option<tokio::task::JoinHandle<()>>,
    /// The task serving the `file` channel (`crate::files::run`, slice
    /// 3.4a). Aborting it drops any unfinished transfer, which removes the
    /// partial file.
    pub(super) files_task: tokio::task::JoinHandle<()>,
    /// Whether file transfer works for this session (`HostContext::files_enabled`
    /// and a usable download folder); announced in `HostInfo`.
    pub(super) files_enabled: bool,
    /// This session's `PeerSession::tag()` (fix to slice 3.5b). Events that
    /// can end a session (`SessionEvent::ConnectionState`/`DataChannelClosed`)
    /// carry the tag of the `PeerSession` that produced them;
    /// `handle_session_event` compares it against this field and ignores a
    /// mismatch as stale -- see those variants' doc comments in
    /// `transport::SessionEvent` for the scenario this guards against
    /// (an old, already-superseded session's event arriving after a new one
    /// has taken its place, e.g. during automatic client reconnection).
    pub(super) session_tag: u64,
    /// The owner's chosen quality preset (slice 3.5e, `ControlMessage::SetQuality`),
    /// starting at `QualityPreset::Auto` for every new session. Kept here
    /// (rather than only inside the adapt task's `Controller`) so
    /// `switch_display` -- which rebuilds `VideoPipeline` and therefore
    /// starts a fresh `Controller` at `Auto` -- can re-apply it to the new
    /// pipeline's controller instead of silently resetting the owner's
    /// choice on every display switch.
    pub(super) quality_preset: QualityPreset,
    /// Шторка хоста для этого сеанса (слайс 4.2c), копия `HostContext::curtain`:
    /// `shutdown` не получает `ctx`, а блокировка экрана в конце сеанса
    /// нужна именно ему.
    pub(super) curtain: Option<Arc<dyn Curtain>>,
    /// CGWindowID окон включённой шторки (слайс 4.2c); пусто -- шторка
    /// выключена. Передаётся в `build_source` при каждой пересборке
    /// конвейера (смена дисплея не должна выдать шторку в видео).
    pub(super) curtain_windows: Vec<u32>,
    /// Момент последней попытки `SelectDisplay` на другой дисплей (успешной
    /// или нет) для rate-limit `DISPLAY_SWITCH_MIN_INTERVAL`; `None` -- ещё
    /// не переключали.
    pub(super) last_display_switch: Option<std::time::Instant>,
    /// V-2: бюджет автоматических перезапусков конвейера после сбоя
    /// (`VideoFailed`): не более `PIPELINE_RECOVERY_MAX` за
    /// `PIPELINE_RECOVERY_WINDOW`, дальше сеанс заканчивается как раньше.
    pub(super) pipeline_recoveries: RecoveryBudget,
    /// V-2: задача, раз в `DISPLAY_POLL_INTERVAL` шлющая
    /// `SessionEvent::DisplayGeometryChanged`; abort в `shutdown`.
    pub(super) display_poll_task: tokio::task::JoinHandle<()>,
    /// V-2: id дисплея текущего конвейера для `display_poll_task`;
    /// обновляется в `replace_pipeline`.
    pub(super) display_poll_id: Arc<AtomicU32>,
    /// Guard пробуждения дисплея (слайс I7): держит и продлевает «активность
    /// пользователя» весь сеанс; отпускается при дропе `ActiveSession`.
    /// `Mutex` лишь делает поле `Sync` (`ActiveSession` держат через `.await`).
    pub(super) _display_wake: std::sync::Mutex<Box<dyn Send>>,
    /// Раскладка хоста (слайс 4.6c), копия `HostContext::input_source`.
    pub(super) input_source: Option<Arc<dyn InputSourceSwitcher>>,
    /// Фоновый опрос раскладки: шлёт `InputSource` при изменении. `None`
    /// без реализации.
    pub(super) input_source_task: Option<tokio::task::JoinHandle<()>>,
    /// Slice 3.5b: tracks the current `RTCPeerConnectionState::Disconnected`
    /// grace window (if any) for this session -- see `DisconnectGrace`'s
    /// doc comment and `handle_session_event`'s `ConnectionState`/
    /// `DisconnectTimeout` arms, its only caller.
    pub(super) disconnect_grace: DisconnectGrace,
    /// The OPAQUE session key from a successful login (slice 3.2c), when
    /// this session was gated by an access password -- `None` when the
    /// host has no password set. Unused today beyond being kept here;
    /// slice 3.2e will feed it into the data channel encryption.
    #[allow(dead_code)]
    pub(super) session_key: Option<crate::access::SessionKey>,
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
pub(super) struct DisconnectGrace {
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
    pub(super) fn disconnected(&mut self) -> Option<u64> {
        if self.pending.is_some() {
            return None;
        }
        self.epoch += 1;
        self.pending = Some(self.epoch);
        Some(self.epoch)
    }

    /// The connection reported `Connected`: cancels any grace window in
    /// progress.
    pub(super) fn connected(&mut self) {
        self.pending = None;
    }

    /// Whether a `DisconnectTimeout` for `generation` still refers to the
    /// window currently running -- `false` once a `Connected` cancelled it
    /// (or a session ended and a new one started, tracked separately by
    /// `handle_session_event` via `current_session_id`).
    pub(super) fn is_current(&self, generation: u64) -> bool {
        self.pending == Some(generation)
    }
}

impl ActiveSession {
    /// `shutdown`, preceded by the final `SessionStats` (before any `Bye` the
    /// caller sends afterwards). `session_id` is `None` when unknown.
    pub(super) async fn shutdown_reporting(
        self,
        out_tx: &mpsc::UnboundedSender<SignalMessage>,
        session_id: Option<&str>,
    ) {
        if let Some(session_id) = session_id {
            send_session_stats(&self.peer, out_tx, session_id, true).await;
        }
        self.shutdown().await;
    }

    pub(super) async fn shutdown(self) {
        self.peer.close().await;
        self.video.stop();
        self.cursor_task.abort();
        self.display_poll_task.abort();
        if let Some(task) = self.clipboard_task {
            task.abort();
        }
        if let Some(task) = self.elevation_task {
            task.abort();
        }
        if let Some(task) = self.input_source_task {
            task.abort();
        }
        self.files_task.abort();
        // Шторка включена -- сначала блокируем экран, и только потом убираем
        // шторку: рабочий стол не должен открыться без присмотра (слайс 4.2c).
        if let (Some(curtain), false) = (self.curtain, self.curtain_windows.is_empty()) {
            let result = tokio::task::spawn_blocking(move || {
                let locked = curtain.lock_screen();
                if locked.is_ok() {
                    curtain.hide();
                }
                locked
            })
            .await;
            match result {
                Ok(Ok(())) => tracing::info!("curtain: screen locked, curtain hidden"),
                Ok(Err(err)) => {
                    tracing::warn!(?err, "curtain: screen not locked, curtain stays up");
                }
                Err(err) => tracing::warn!(?err, "curtain: teardown task failed"),
            }
        }
    }
}

/// One session waiting on an OPAQUE login (slice 3.2c) before the host will
/// offer it, armed by `handle_signal_message`'s `PeerJoined` arm when
/// `HostContext::access_store` has a password set. Lives in `SessionSlot`
/// separately from `active`/`current_session_id` -- unlike an `ActiveSession`,
/// there is no `PeerSession`/video pipeline yet, nothing to shut down, only
/// state to remember while the client works through `PakeStart`/`PakeFinish`.
pub(super) struct PendingAuth {
    /// The session id from the `PeerJoined` that armed this wait -- matched
    /// against every incoming `PakeStart`/`PakeFinish`/`Bye` so a message
    /// for some other (already superseded) session id is ignored.
    pub(super) session_id: String,
    /// This `PeerJoined`'s `ice_servers`, kept so a successful `PakeFinish`
    /// can start the session with the same credentials `PeerJoined`
    /// carried, exactly as the no-password path would have used right away.
    pub(super) ice_servers: Vec<proto::signal::IceServer>,
    /// The server half of the in-progress OPAQUE login, set by a successful
    /// `PakeStart` and consumed by the next `PakeFinish`. `None` before the
    /// first `PakeStart`, and again right after a `PakeFinish` that failed
    /// (see `access::LoginServer::login_finish`'s doc comment) -- the wait
    /// stays armed, but the client must start over with a fresh `PakeStart`.
    pub(super) login: Option<crate::access::LoginServer>,
    /// This wait's `AuthTimeout` generation (see `next_auth_generation`) --
    /// matched by `handle_session_event`'s `AuthTimeout` arm against a timer
    /// armed for it, same reasoning as `DisconnectGrace`'s `epoch`.
    pub(super) generation: u64,
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
    pub(super) active: Option<ActiveSession>,
    pub(super) current_session_id: Option<String>,
    pub(super) event_tx: mpsc::Sender<SessionEvent>,
    pub(super) event_rx: mpsc::Receiver<SessionEvent>,
    /// Slice 3.2c: the session currently waiting on an OPAQUE login, if
    /// any -- see `PendingAuth`'s doc comment. Unlike `active`, this has no
    /// P2P channel of its own yet, so it does not survive losing this
    /// signaling connection (see `SignalingClient::run`'s exit path).
    pub(super) pending_auth: Option<PendingAuth>,
    /// Slice 3.2c: consecutive failed `PakeStart` attempts against this
    /// host's access password, across every `PendingAuth` -- deliberately
    /// *not* reset by a fresh `PeerJoined` (only a successful `PakeFinish`
    /// resets it): it lives in the slot and survives a signaling reconnect,
    /// so a client can't dodge the throttle below by reconnecting.
    pub(super) auth_failures: u32,
    /// Slice 3.2c: armed once `auth_failures` crosses `MAX_AUTH_ATTEMPTS`,
    /// as a *pace limit* on the next `PakeStart`, not a hard lockout: while
    /// in the future, a `PakeStart` is rejected with
    /// `AuthFailed { retry_after_secs: Some(..) } }` without spending an
    /// OPRF evaluation or touching `auth_failures` further, but once it's in
    /// the past the next attempt runs as normal (and, if it also fails,
    /// re-arms this for a longer pause -- see `auth_lockout_duration`).
    pub(super) auth_locked_until: Option<Instant>,
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
