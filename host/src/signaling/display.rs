//! Display handling: the display list announcement, pipeline watchdog and
//! recovery, display geometry polling and switching, and the curtain
//! (privacy screen) toggle.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use proto::control::{ControlMessage, DisplayEntry, QualityPreset};

use crate::adapt::Feedback;
use crate::capture::DisplayInfo;
use crate::transport::{PeerSession, SessionEvent};

use super::{ActiveSession, HostContext, VideoPipeline};

/// Сообщение о уже показанной шторке для вновь открытого канала `control`;
/// `None`, если шторка не показана.
pub(super) fn curtain_announce_message(curtain_windows: &[u32]) -> Option<ControlMessage> {
    (!curtain_windows.is_empty()).then_some(ControlMessage::CurtainStatus {
        on: true,
        reason: None,
    })
}

/// Builds the `ControlMessage::Displays` announcement for `displays`/`current`.
pub(super) fn displays_message(displays: &[DisplayInfo], current: u32) -> ControlMessage {
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

/// V-2: сколько раз за `PIPELINE_RECOVERY_WINDOW` можно перезапустить
/// упавший конвейер, прежде чем сдаться и закончить сеанс.
pub(super) const PIPELINE_RECOVERY_MAX: usize = 3;
/// V-2: окно подсчёта перезапусков для `PIPELINE_RECOVERY_MAX`.
const PIPELINE_RECOVERY_WINDOW: Duration = Duration::from_secs(60);
/// V-2: период опроса геометрии дисплея.
pub(super) const DISPLAY_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// V-2: сколько `encoded` может стоять при растущем `captured`, прежде чем
/// конвейер объявляется зависшим.
const PIPELINE_STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// V-2: минимальный прирост `captured` за время простоя `encoded`.
const PIPELINE_STALL_MIN_CAPTURED: u64 = 10;

/// V-2: скользящий бюджет автоматических перезапусков конвейера.
#[derive(Debug, Default)]
pub(super) struct RecoveryBudget {
    attempts: Vec<Instant>,
}

impl RecoveryBudget {
    /// Пытается потратить одну попытку на момент `now`. `Some(n)` -- номер
    /// попытки в текущем окне (1-based); `None` -- бюджет исчерпан.
    fn try_acquire(&mut self, now: Instant) -> Option<usize> {
        self.attempts
            .retain(|t| now.saturating_duration_since(*t) < PIPELINE_RECOVERY_WINDOW);
        if self.attempts.len() >= PIPELINE_RECOVERY_MAX {
            return None;
        }
        self.attempts.push(now);
        Some(self.attempts.len())
    }
}

/// V-2: чистый детектор зависания конвейера: `captured` растёт, а `encoded`
/// стоит дольше `PIPELINE_STALL_TIMEOUT`. Срабатывает один раз (до
/// перезапуска конвейера, у которого свой детектор).
#[derive(Debug)]
struct StallDetector {
    last_encoded: u64,
    /// Момент, с которого `encoded` не менялся, и `captured` на тот момент.
    stalled_since: Instant,
    captured_at_stall: u64,
    fired: bool,
}

impl StallDetector {
    fn new(now: Instant, captured: u64, encoded: u64) -> Self {
        Self {
            last_encoded: encoded,
            stalled_since: now,
            captured_at_stall: captured,
            fired: false,
        }
    }

    /// Очередной замер счётчиков; `true` -- конвейер завис (однократно).
    fn observe(&mut self, now: Instant, captured: u64, encoded: u64) -> bool {
        if encoded != self.last_encoded {
            self.last_encoded = encoded;
            self.stalled_since = now;
            self.captured_at_stall = captured;
            return false;
        }
        if self.fired {
            return false;
        }
        if now.saturating_duration_since(self.stalled_since) >= PIPELINE_STALL_TIMEOUT
            && captured.saturating_sub(self.captured_at_stall) >= PIPELINE_STALL_MIN_CAPTURED
        {
            self.fired = true;
            return true;
        }
        false
    }
}

/// V-2: изменилась ли геометрия дисплея (размер или положение). `scale` в
/// `DisplayInfo` нет: на Retina его смена видна как смена `width`/`height`
/// захвата, а координаты ввода берутся из `x/y/width/height`.
fn display_geometry_changed(a: &DisplayInfo, b: &DisplayInfo) -> bool {
    (a.x, a.y, a.width, a.height) != (b.x, b.y, b.width, b.height)
}

/// V-2: watchdog конвейера -- раз в 2 с снимает `PipelineStats` и при
/// зависании шлёт `VideoFailed` (путь восстановления). Живёт рядом с
/// forward-задачей и не зависит от `adapt`; `VideoPipeline::stop` его abort-ит.
pub(super) fn spawn_pipeline_watchdog(
    stats: Arc<crate::pipeline::PipelineStats>,
    peer: Arc<PeerSession>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Прогресс кодера -- `encoded + dropped`: при полном выходном канале
        // кадры уходят в `dropped` (кодер жив, затор дальше), это не зависание.
        let load = |s: &crate::pipeline::PipelineStats| {
            (
                s.captured.load(Ordering::Relaxed),
                s.encoded
                    .load(Ordering::Relaxed)
                    .wrapping_add(s.dropped.load(Ordering::Relaxed)),
            )
        };
        let (captured, encoded) = load(&stats);
        let mut detector = StallDetector::new(Instant::now(), captured, encoded);
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + DISPLAY_POLL_INTERVAL,
            DISPLAY_POLL_INTERVAL,
        );
        loop {
            ticker.tick().await;
            let (captured, encoded) = load(&stats);
            if detector.observe(Instant::now(), captured, encoded) {
                tracing::error!(captured, encoded, "video pipeline stalled");
                let _ = peer
                    .events()
                    .send(SessionEvent::VideoFailed { tag: peer.tag() })
                    .await;
            }
        }
    })
}

/// V-2: чистый шаг опроса: если дисплей с `known.id` есть в `list` и его
/// геометрия отличается от `known` -- возвращает его новое состояние. Дисплей
/// пропал или не изменился -- `None`.
fn geometry_poll_step(known: &DisplayInfo, list: &[DisplayInfo]) -> Option<DisplayInfo> {
    list.iter()
        .find(|d| d.id == known.id)
        .filter(|d| display_geometry_changed(known, d))
        .cloned()
}

/// V-2: задача сеанса: раз в `interval` зовёт `poll_displays` в
/// `spawn_blocking` (на macOS -- дешёвый CoreGraphics, не ScreenCaptureKit) и при
/// смене геометрии текущего дисплея шлёт `DisplayGeometryChanged`. Помнит
/// последнюю известную геометрию, чтобы не слать событие повторно. `current_id`
/// обновляет `replace_pipeline` (смена дисплея): тогда база берётся заново из
/// списка без события. Первый тик -- через полный интервал. Ошибка списка или
/// исчезнувший id -- тик пропускается.
pub(super) fn spawn_display_poll(
    events: mpsc::Sender<SessionEvent>,
    tag: u64,
    interval: Duration,
    poll_displays: Arc<dyn Fn() -> anyhow::Result<Vec<DisplayInfo>> + Send + Sync>,
    current_id: Arc<AtomicU32>,
    initial: DisplayInfo,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut known = initial;
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        loop {
            ticker.tick().await;
            let f = Arc::clone(&poll_displays);
            let list = match tokio::task::spawn_blocking(move || f()).await {
                Ok(Ok(list)) => list,
                Ok(Err(err)) => {
                    tracing::debug!(?err, "display poll: list_displays failed");
                    continue;
                }
                Err(err) => {
                    tracing::debug!(?err, "display poll: task failed");
                    continue;
                }
            };
            let id = current_id.load(Ordering::SeqCst);
            if id != known.id {
                // Конвейер переехал на другой дисплей: новая база без события.
                if let Some(d) = list.iter().find(|d| d.id == id) {
                    known = d.clone();
                }
                continue;
            }
            if let Some(fresh) = geometry_poll_step(&known, &list) {
                known = fresh;
                if events
                    .send(SessionEvent::DisplayGeometryChanged { tag })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    })
}

/// V-2: перезапускает конвейер на актуальной геометрии дисплея того же `id`
/// (если его уже нет -- на первом из списка). Общая часть восстановления
/// после сбоя и реакции на смену геометрии. Старый конвейер останавливает
/// `replace_pipeline` (`stop` безопасен для уже вышедшего: `send` в закрытый
/// oneshot игнорируется, `abort` завершённой задачи -- no-op), обновляет
/// `CaptureRect` роутера; здесь -- ещё `active.displays` и `Displays` клиенту.
async fn restart_pipeline_on_fresh_display(
    ctx: &HostContext,
    active: &mut ActiveSession,
) -> anyhow::Result<()> {
    let list = (ctx.list_displays)()?;
    let id = active.video.display.id;
    let display = list
        .iter()
        .find(|d| d.id == id)
        .or_else(|| list.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no displays available"))?;
    replace_pipeline(ctx, active, display).await?;
    active.displays = list;
    let msg = displays_message(&active.displays, active.video.display.id);
    let _ = active.peer.send_control(&msg).await;
    Ok(())
}

/// V-2: реакция на `VideoFailed` -- `true`, если конвейер перезапущен и сеанс
/// продолжается; `false` -- бюджет исчерпан или перезапуск не удался
/// (вызывающий заканчивает сеанс `Bye { VideoFailed }`).
pub(super) async fn recover_pipeline(ctx: &HostContext, active: &mut ActiveSession) -> bool {
    let Some(attempt) = active.pipeline_recoveries.try_acquire(Instant::now()) else {
        tracing::warn!(
            max = PIPELINE_RECOVERY_MAX,
            "video pipeline recovery budget exhausted"
        );
        return false;
    };
    match restart_pipeline_on_fresh_display(ctx, active).await {
        Ok(()) => {
            tracing::warn!(
                attempt,
                width = active.video.display.width,
                height = active.video.display.height,
                "video pipeline failed, restarted"
            );
            true
        }
        Err(err) => {
            tracing::warn!(?err, attempt, "video pipeline restart failed");
            false
        }
    }
}

/// V-2: реакция на `DisplayGeometryChanged` -- пересобирает конвейер, если
/// конвейер ещё стоит на старой геометрии. Бюджет восстановлений не тратит;
/// при ошибке сеанс не заканчивает (старый конвейер продолжает).
pub(super) async fn refresh_display_geometry(ctx: &HostContext, active: &mut ActiveSession) {
    let Ok(list) = (ctx.list_displays)() else {
        return;
    };
    let current = &active.video.display;
    let Some(fresh) = list.iter().find(|d| d.id == current.id) else {
        return;
    };
    if !display_geometry_changed(current, fresh) {
        return;
    }
    tracing::info!(
        display_id = fresh.id,
        width = fresh.width,
        height = fresh.height,
        "display geometry changed, restarting video pipeline"
    );
    if let Err(err) = restart_pipeline_on_fresh_display(ctx, active).await {
        tracing::warn!(
            ?err,
            "display geometry restart failed, keeping current pipeline"
        );
    }
}

/// Минимальный интервал между попытками переключить дисплей (`SelectDisplay`):
/// каждая попытка пересобирает конвейер захвата и кодера, клиент не должен
/// мочь делать это бесконечно часто. Считается по попыткам, успешным и нет.
const DISPLAY_SWITCH_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Handles `ControlMessage::SelectDisplay { id }`: builds a fresh
/// `VideoPipeline` for the requested display *before* stopping the current
/// one, so a failure (unknown id, or the new pipeline fails to start) leaves
/// the session streaming exactly what it was streaming before -- no black
/// gap, no dropped session. Always re-announces the (possibly refreshed)
/// display list afterwards, since the client is waiting on a reply either
/// way.
pub(super) async fn switch_display(ctx: &HostContext, active: &mut ActiveSession, id: u32) {
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
    if let Some(last) = active.last_display_switch {
        if last.elapsed() < DISPLAY_SWITCH_MIN_INTERVAL {
            tracing::warn!(id, "select_display rate-limited, keeping current display");
            let msg = displays_message(&active.displays, active.video.display.id);
            let _ = active.peer.send_control(&msg).await;
            return;
        }
    }
    active.last_display_switch = Some(std::time::Instant::now());
    let from = active.video.display.id;
    match replace_pipeline(ctx, active, display).await {
        Ok(()) => {
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

/// Строит новый `VideoPipeline` для `display` (с исключением окон шторки
/// `active.curtain_windows`), и только потом останавливает прежний: при
/// ошибке сеанс продолжает стримить как стримил. Общая часть `switch_display`
/// и перезапуска конвейера под шторку (слайс 4.2c): ре-аплай пресета
/// качества и обновление rect роутера ввода. `PeerSession` не трогает.
async fn replace_pipeline(
    ctx: &HostContext,
    active: &mut ActiveSession,
    display: DisplayInfo,
) -> anyhow::Result<()> {
    // D24: both forward tasks write into the shared `video_tx` until `old` is
    // stopped. Bump the generation first so the old pipeline goes silent
    // before the new one can emit its first keyframe; otherwise a late old
    // frame could land after that keyframe and break the client's reference
    // chain.
    let previous_generation = active.pipeline_generation.load(Ordering::SeqCst);
    let new_generation = previous_generation + 1;
    active
        .pipeline_generation
        .store(new_generation, Ordering::SeqCst);
    let new = match VideoPipeline::start(
        ctx,
        display,
        Arc::clone(&active.peer),
        active.video_tx.clone(),
        Arc::clone(&active.keyframe_flag),
        new_generation,
        Arc::clone(&active.pipeline_generation),
        &active.curtain_windows,
    )
    .await
    {
        Ok(new) => new,
        Err(err) => {
            // The old pipeline keeps running: let it send again, and ask for
            // a keyframe since it may have skipped frames meanwhile.
            active
                .pipeline_generation
                .store(previous_generation, Ordering::SeqCst);
            active.keyframe_flag.store(true, Ordering::SeqCst);
            return Err(err);
        }
    };
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
    // V-2: опрос геометрии должен следить за новым дисплеем.
    active
        .display_poll_id
        .store(active.video.display.id, Ordering::SeqCst);
    Ok(())
}

/// Обрабатывает `ControlMessage::SetCurtain { on }` (слайс 4.2c) и возвращает
/// `CurtainStatus` для ответа клиенту. Включение: `show()` -> перезапуск
/// конвейера с исключением окон шторки; любая ошибка откатывает шторку
/// (`hide()`) и оставляет прежний рабочий конвейер. Выключение: `hide()`,
/// конвейер не перезапускается -- несуществующие окна в исключении безвредны.
pub(super) async fn set_curtain(
    ctx: &HostContext,
    active: &mut ActiveSession,
    on: bool,
) -> ControlMessage {
    let status = |on: bool, reason: Option<String>| ControlMessage::CurtainStatus { on, reason };
    let Some(curtain) = ctx.curtain.clone() else {
        return status(false, Some("шторка недоступна на этом хосте".to_string()));
    };
    if !on {
        let c = Arc::clone(&curtain);
        if let Err(err) = tokio::task::spawn_blocking(move || c.hide()).await {
            tracing::warn!(?err, "curtain: hide task failed");
        }
        active.curtain_windows.clear();
        tracing::info!("curtain: off");
        return status(false, None);
    }
    if !active.curtain_windows.is_empty() {
        return status(true, None);
    }
    let c = Arc::clone(&curtain);
    let ids = match tokio::task::spawn_blocking(move || c.show()).await {
        Ok(Ok(ids)) => ids,
        Ok(Err(err)) => {
            tracing::warn!(?err, "curtain: show failed");
            return status(false, Some(err.to_string()));
        }
        Err(err) => {
            tracing::warn!(?err, "curtain: show task failed");
            return status(false, Some(err.to_string()));
        }
    };
    // `show()` без окон (пустой список) нечем отличить от «выключена»:
    // считаем это ошибкой и откатываем.
    if ids.is_empty() {
        let c = Arc::clone(&curtain);
        let _ = tokio::task::spawn_blocking(move || c.hide()).await;
        return status(false, Some("шторка не создала ни одного окна".to_string()));
    }
    active.curtain_windows = ids;
    let display = active.video.display.clone();
    match replace_pipeline(ctx, active, display).await {
        Ok(()) => {
            tracing::info!(windows = active.curtain_windows.len(), "curtain: on");
            status(true, None)
        }
        Err(err) => {
            tracing::warn!(?err, "curtain: failed to restart pipeline, hiding curtain");
            active.curtain_windows.clear();
            let c = Arc::clone(&curtain);
            let _ = tokio::task::spawn_blocking(move || c.hide()).await;
            status(false, Some(err.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_budget_allows_max_attempts_per_window() {
        let t0 = Instant::now();
        let mut b = RecoveryBudget::default();
        assert_eq!(b.try_acquire(t0), Some(1));
        assert_eq!(b.try_acquire(t0 + Duration::from_secs(1)), Some(2));
        assert_eq!(b.try_acquire(t0 + Duration::from_secs(2)), Some(3));
        assert_eq!(b.try_acquire(t0 + Duration::from_secs(3)), None);
        // Исчерпанный бюджет не тратит и не продлевает окно.
        assert_eq!(b.try_acquire(t0 + Duration::from_secs(59)), None);
    }

    #[test]
    fn recovery_budget_refills_as_window_slides() {
        let t0 = Instant::now();
        let mut b = RecoveryBudget::default();
        for i in 0..PIPELINE_RECOVERY_MAX as u64 {
            assert!(b.try_acquire(t0 + Duration::from_secs(i)).is_some());
        }
        // Первая попытка вышла из окна -- освободился один слот.
        assert_eq!(
            b.try_acquire(t0 + PIPELINE_RECOVERY_WINDOW),
            Some(PIPELINE_RECOVERY_MAX)
        );
        assert_eq!(b.try_acquire(t0 + PIPELINE_RECOVERY_WINDOW), None);
    }

    #[test]
    fn stall_detector_ignores_static_screen() {
        let t0 = Instant::now();
        let mut d = StallDetector::new(t0, 100, 100);
        for s in (2..=60).step_by(2) {
            assert!(!d.observe(t0 + Duration::from_secs(s), 100, 100));
        }
    }

    #[test]
    fn stall_detector_ignores_healthy_pipeline() {
        let t0 = Instant::now();
        let mut d = StallDetector::new(t0, 0, 0);
        for s in (2..=60u64).step_by(2) {
            assert!(!d.observe(t0 + Duration::from_secs(s), s * 30, s * 30));
        }
    }

    #[test]
    fn stall_detector_fires_once_when_encoder_stops_but_capture_runs() {
        let t0 = Instant::now();
        let mut d = StallDetector::new(t0, 0, 50);
        let mut fired = Vec::new();
        for s in (2..=30u64).step_by(2) {
            if d.observe(t0 + Duration::from_secs(s), s * 30, 50) {
                fired.push(s);
            }
        }
        assert_eq!(fired, vec![10]);
    }

    #[test]
    fn stall_detector_needs_enough_captured_frames() {
        let t0 = Instant::now();
        let mut d = StallDetector::new(t0, 0, 50);
        // Захват почти не растёт (9 кадров за всё время) -- не зависание.
        assert!(!d.observe(t0 + Duration::from_secs(12), 9, 50));
        assert!(d.observe(t0 + Duration::from_secs(14), 10, 50));
    }

    #[test]
    fn stall_detector_resets_when_encoder_moves() {
        let t0 = Instant::now();
        let mut d = StallDetector::new(t0, 0, 50);
        assert!(!d.observe(t0 + Duration::from_secs(8), 240, 50));
        assert!(!d.observe(t0 + Duration::from_secs(10), 300, 51));
        assert!(!d.observe(t0 + Duration::from_secs(18), 540, 51));
        assert!(d.observe(t0 + Duration::from_secs(20), 600, 51));
    }

    #[test]
    fn geometry_poll_step_reports_only_real_changes() {
        let mk = |id, w| DisplayInfo {
            id,
            title: String::new(),
            x: 0,
            y: 0,
            width: w,
            height: 1080,
            primary: true,
        };
        let known = mk(1, 1920);
        assert_eq!(
            geometry_poll_step(&known, &[mk(2, 800), mk(1, 1280)]).map(|d| d.width),
            Some(1280)
        );
        assert!(geometry_poll_step(&known, &[mk(1, 1920), mk(2, 800)]).is_none());
        assert!(geometry_poll_step(&known, &[mk(2, 800)]).is_none());
        assert!(geometry_poll_step(&known, &[]).is_none());
    }

    #[test]
    fn display_geometry_changed_compares_size_and_position() {
        let base = DisplayInfo {
            id: 1,
            title: "A".to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        assert!(!display_geometry_changed(&base, &base.clone()));
        let mut other = base.clone();
        other.title = "B".to_string();
        other.primary = false;
        assert!(!display_geometry_changed(&base, &other));
        for f in [
            |d: &mut DisplayInfo| d.width = 1280,
            |d: &mut DisplayInfo| d.height = 720,
            |d: &mut DisplayInfo| d.x = -1920,
            |d: &mut DisplayInfo| d.y = 10,
        ] {
            let mut moved = base.clone();
            f(&mut moved);
            assert!(display_geometry_changed(&base, &moved));
        }
    }
}
