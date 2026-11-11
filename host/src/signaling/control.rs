//! Control-channel helpers: the `HostInfo` announcement, in-session host
//! update (`UpdateHost`) and the keyboard-layout (input source) switching and
//! polling.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use proto::control::ControlMessage;

use crate::input_source::InputSourceSwitcher;
use crate::transport::PeerSession;

use super::HostContext;

/// Builds the `ControlMessage::HostInfo` announcement (slice 3.5f), sent
/// alongside `displays_message` when the `control` channel opens.
pub(super) fn host_info_message(
    files_enabled: bool,
    curtain_available: bool,
    input_source_available: bool,
    update_available: bool,
    ctrl_alt_del_available: bool,
) -> ControlMessage {
    ControlMessage::HostInfo {
        os: crate::platform::host_os(),
        files_enabled,
        curtain_available,
        input_source_available,
        version: Some(crate::build_info::VERSION.to_string()),
        update_available,
        ctrl_alt_del_available,
    }
}

/// Хост умеет обновиться из сеанса (слайс D50d): известны каталоги и
/// платформа поддерживает установщик.
pub(super) fn update_available(ctx: &HostContext) -> bool {
    ctx.update_dirs.is_some() && !crate::platform::update::ASSET_NAME.is_empty()
}

/// Максимальная длина `message` в `UpdateStatus::Failed`.
const UPDATE_MESSAGE_MAX_CHARS: usize = 300;

/// Обрезает сообщение об ошибке до `UPDATE_MESSAGE_MAX_CHARS` по границе char.
fn truncate_update_message(text: &str) -> String {
    match text.char_indices().nth(UPDATE_MESSAGE_MAX_CHARS) {
        Some((idx, _)) => text[..idx].to_string(),
        None => text.to_string(),
    }
}

fn update_failed(message: String) -> ControlMessage {
    ControlMessage::UpdateStatus {
        state: proto::control::UpdateState::Failed,
        version: None,
        message: Some(message),
        received: None,
        total: None,
    }
}

/// Запускает обновление по `UpdateHost` (слайс D50d). `Err` -- готовое
/// сообщение об отказе (обновление недоступно). `Ok` -- приёмник статусов:
/// задача обновления живёт сама по себе (не abort), закрытие приёмника
/// (сеанс кончился) статусы просто теряет. `InstallerLaunched` ничего сверх
/// `Installing` не шлёт: сеанс сейчас оборвётся.
pub(super) fn begin_update(
    ctx: &HostContext,
) -> Result<mpsc::UnboundedReceiver<ControlMessage>, Box<ControlMessage>> {
    use crate::updater::{UpdateOutcome, UpdateProgress};
    use proto::control::UpdateState;

    let dirs = match (&ctx.update_dirs, update_available(ctx)) {
        (Some(dirs), true) => dirs.clone(),
        _ => {
            return Err(Box::new(update_failed(
                "обновление недоступно на этом хосте".to_string(),
            )))
        }
    };
    let (tx, rx) = mpsc::unbounded_channel::<ControlMessage>();
    let current = crate::update_check::parse_version(crate::build_info::VERSION)
        .unwrap_or(crate::update_check::Version(0, 0, 0));
    tokio::spawn(async move {
        let progress_tx = tx.clone();
        let result = crate::updater::run_update(&dirs.work_dir, &dirs.log_dir, current, move |p| {
            let (state, version, received, total) = match p {
                UpdateProgress::Checking => (UpdateState::Checking, None, None, None),
                UpdateProgress::Downloading {
                    version,
                    received,
                    total,
                } => (
                    UpdateState::Downloading,
                    Some(version.to_string()),
                    Some(received),
                    total,
                ),
                UpdateProgress::Installing(v) => {
                    (UpdateState::Installing, Some(v.to_string()), None, None)
                }
            };
            let _ = progress_tx.send(ControlMessage::UpdateStatus {
                state,
                version,
                message: None,
                received,
                total,
            });
        })
        .await;
        match result {
            Ok(UpdateOutcome::UpToDate(v)) => {
                let _ = tx.send(ControlMessage::UpdateStatus {
                    state: UpdateState::UpToDate,
                    version: Some(v.to_string()),
                    message: None,
                    received: None,
                    total: None,
                });
            }
            Ok(UpdateOutcome::InstallerLaunched(v)) => {
                tracing::info!(version = %v, "установщик обновления запущен из сеанса");
            }
            Err(err) => {
                tracing::warn!(error = %err, "обновление из сеанса не удалось");
                let _ = tx.send(update_failed(truncate_update_message(&format!("{err:#}"))));
            }
        }
    });
    Ok(rx)
}

/// Интервал опроса раскладки хоста (слайс 4.6c): её могли переключить на
/// самом хосте или хоткеем.
pub(super) const INPUT_SOURCE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// `InputSource` с текущей меткой раскладки, если она известна.
pub(super) fn input_source_message(switcher: &dyn InputSourceSwitcher) -> Option<ControlMessage> {
    switcher
        .current_label()
        .map(|label| ControlMessage::InputSource { label })
}

/// Обрабатывает `SwitchInputSource` (слайс 4.6c): без реализации --
/// игнорирует; иначе переключает раскладку и возвращает актуальный
/// `InputSource` для отправки клиенту.
pub(super) fn switch_input_source(
    switcher: Option<&dyn InputSourceSwitcher>,
) -> Option<ControlMessage> {
    let Some(switcher) = switcher else {
        tracing::debug!("switch_input_source received without a switcher, ignoring");
        return None;
    };
    if let Err(err) = switcher.select_next() {
        tracing::warn!(?err, "failed to switch input source");
    }
    input_source_message(switcher)
}

/// Опрашивает раскладку каждые `interval` и отдаёт `send` новый
/// `InputSource` только при изменении метки. `send` возвращает `true`, если
/// сообщение доставлено; иначе метка не считается отправленной и уйдёт на
/// следующем тике (канал `control` мог ещё не открыться). Стартовая метка
/// `initial` снимается синхронно до запуска задачи (её клиенту шлёт открытие
/// канала `control`), чтобы смена сразу после спавна не потерялась.
async fn poll_input_source<F, Fut>(
    switcher: Arc<dyn InputSourceSwitcher>,
    initial: Option<String>,
    interval: Duration,
    mut send: F,
) where
    F: FnMut(ControlMessage) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut last = initial;
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let label = switcher.current_label();
        if label == last {
            continue;
        }
        let Some(new) = label.clone() else {
            last = None;
            continue;
        };
        if send(ControlMessage::InputSource { label: new }).await {
            last = label;
        }
    }
}

pub(super) fn spawn_input_source_poll(
    peer: Arc<PeerSession>,
    switcher: Arc<dyn InputSourceSwitcher>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let initial = switcher.current_label();
    tokio::spawn(poll_input_source(switcher, initial, interval, move |msg| {
        let peer = Arc::clone(&peer);
        async move { matches!(peer.send_control(&msg).await, Ok(true)) }
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::signaling::test_ctx;

    use super::*;

    /// The `control` channel's opening announcements (slice 3.5f) include
    /// the host's own OS, matching `platform::host_os()` -- what the client
    /// uses to decide whether Cmd should act as Ctrl (`web/src/keyRemap.ts`).
    #[test]
    fn host_info_message_reports_this_hosts_os() {
        assert_eq!(
            host_info_message(true, false, false, false, false),
            ControlMessage::HostInfo {
                os: crate::platform::host_os(),
                files_enabled: true,
                curtain_available: false,
                input_source_available: false,
                version: Some(crate::build_info::VERSION.to_string()),
                update_available: false,
                ctrl_alt_del_available: false,
            }
        );
        assert_eq!(
            host_info_message(false, true, true, false, false),
            ControlMessage::HostInfo {
                os: crate::platform::host_os(),
                files_enabled: false,
                curtain_available: true,
                input_source_available: true,
                version: Some(crate::build_info::VERSION.to_string()),
                update_available: false,
                ctrl_alt_del_available: false,
            }
        );
    }

    /// Слайс D46: флаг `ctrl_alt_del_available` доезжает до `HostInfo`.
    #[test]
    fn host_info_reports_ctrl_alt_del_availability() {
        for available in [true, false] {
            assert!(matches!(
                host_info_message(true, false, false, false, available),
                ControlMessage::HostInfo { ctrl_alt_del_available, .. }
                    if ctrl_alt_del_available == available
            ));
        }
    }

    /// Слайс D50d: без `update_dirs` обновление недоступно.
    #[tokio::test]
    async fn update_unavailable_without_dirs() {
        let ctx = test_ctx();
        assert!(!update_available(&ctx));
        assert!(matches!(
            host_info_message(true, false, false, update_available(&ctx), false),
            ControlMessage::HostInfo {
                update_available: false,
                ..
            }
        ));
    }

    #[test]
    fn truncate_update_message_cuts_on_char_boundary() {
        let long = "я".repeat(400);
        assert_eq!(truncate_update_message(&long).chars().count(), 300);
        assert_eq!(truncate_update_message("коротко"), "коротко");
    }

    /// Фейковый переключатель раскладки (слайс 4.6c): счётчик вызовов
    /// `select_next` и управляемая метка. `select_next` переключает метку на
    /// следующую из `cycle`.
    struct FakeSwitcher {
        calls: std::sync::atomic::AtomicUsize,
        label: Mutex<Option<String>>,
        next_label: Option<String>,
        fail: bool,
    }

    impl FakeSwitcher {
        fn new(label: Option<&str>, next_label: Option<&str>, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                label: Mutex::new(label.map(str::to_string)),
                next_label: next_label.map(str::to_string),
                fail,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn set_label(&self, label: Option<&str>) {
            *self.label.lock().unwrap() = label.map(str::to_string);
        }
    }

    impl InputSourceSwitcher for FakeSwitcher {
        fn select_next(&self) -> anyhow::Result<()> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                return Err(anyhow::anyhow!("boom"));
            }
            *self.label.lock().unwrap() = self.next_label.clone();
            Ok(())
        }
        fn current_label(&self) -> Option<String> {
            self.label.lock().unwrap().clone()
        }
    }

    fn input_source_msg(label: &str) -> ControlMessage {
        ControlMessage::InputSource {
            label: label.to_string(),
        }
    }

    #[test]
    fn host_info_reports_input_source_availability() {
        for available in [true, false] {
            assert!(matches!(
                host_info_message(true, false, available, false, false),
                ControlMessage::HostInfo { input_source_available, .. }
                    if input_source_available == available
            ));
        }
    }

    #[test]
    fn switch_input_source_selects_next_and_reports_new_label() {
        let fake = FakeSwitcher::new(Some("EN"), Some("RU"), false);
        let msg = switch_input_source(Some(fake.as_ref() as &dyn InputSourceSwitcher));
        assert_eq!(fake.calls(), 1);
        assert_eq!(msg, Some(input_source_msg("RU")));
    }

    #[test]
    fn switch_input_source_error_is_not_fatal_and_reports_current_label() {
        let fake = FakeSwitcher::new(Some("EN"), Some("RU"), true);
        let msg = switch_input_source(Some(fake.as_ref() as &dyn InputSourceSwitcher));
        assert_eq!(fake.calls(), 1);
        assert_eq!(msg, Some(input_source_msg("EN")));
    }

    #[test]
    fn switch_input_source_without_switcher_sends_nothing() {
        assert_eq!(switch_input_source(None), None);
    }

    #[test]
    fn initial_input_source_message_needs_a_label() {
        let with = FakeSwitcher::new(Some("EN"), None, false);
        assert_eq!(
            input_source_message(with.as_ref()),
            Some(input_source_msg("EN"))
        );
        let without = FakeSwitcher::new(None, None, false);
        assert_eq!(input_source_message(without.as_ref()), None);
    }

    #[tokio::test]
    async fn poll_sends_on_label_change_and_not_on_repeat() {
        let fake = FakeSwitcher::new(Some("EN"), None, false);
        let (tx, mut rx) = mpsc::unbounded_channel::<ControlMessage>();
        let task = tokio::spawn(poll_input_source(
            fake.clone() as Arc<dyn InputSourceSwitcher>,
            fake.current_label(),
            Duration::from_millis(5),
            move |msg| {
                let tx = tx.clone();
                async move { tx.send(msg).is_ok() }
            },
        ));

        // Метка не менялась -- ничего не шлётся.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(rx.try_recv().is_err(), "unchanged label must not be sent");

        fake.set_label(Some("RU"));
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert_eq!(first, input_source_msg("RU"));

        // Повтор той же метки не шлётся.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            rx.try_recv().is_err(),
            "repeat of same label must not be sent"
        );

        fake.set_label(Some("EN"));
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        assert_eq!(second, input_source_msg("EN"));
        task.abort();
    }

    #[tokio::test]
    async fn poll_retries_label_until_delivered() {
        let fake = FakeSwitcher::new(Some("EN"), None, false);
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let task = tokio::spawn(poll_input_source(
            fake.clone() as Arc<dyn InputSourceSwitcher>,
            fake.current_label(),
            Duration::from_millis(5),
            move |_msg| {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { n >= 2 }
            },
        ));
        fake.set_label(Some("RU"));
        // Две недоставки + одна доставка, дальше тишина.
        tokio::time::timeout(Duration::from_secs(2), async {
            while attempts.load(std::sync::atomic::Ordering::SeqCst) < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timeout");
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
        task.abort();
    }
}
