//! Чистая (без Win32) логика «полного режима» Windows (слайс 4.1a),
//! `ARCHITECTURE.md` §4.1/§8: служба SYSTEM `rcdesk` + помощник
//! (`rcdesk-host serve --machine`) в активной консольной сессии. Всё
//! платформенное (SCM, токены, `CreateProcessAsUserW`) живёт в
//! `platform::windows::service`; здесь -- то, что тестируется на любой ОС:
//! командные строки службы и помощника, автомат-супервизор помощника,
//! файл статуса помощника `status.json` в машинном каталоге данных (PIN и
//! состояние), перенос файлов из
//! пользовательского каталога установщика.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::signaling::AgentStatus;

/// Имя службы в SCM.
pub const SERVICE_NAME: &str = "rcdesk";
/// Статус помощника (состояние + PIN): помощник пишет, `service status`
/// (и значок трея) читает.
pub const STATUS_FILE: &str = "status.json";
/// Устаревший файл PIN (до D83a): помощник удаляет его при старте.
pub const LEGACY_PIN_FILE: &str = "pin.txt";
/// Лог самой службы (`machine_log_dir()`).
pub const SERVICE_LOG: &str = "service.log";
/// Лог помощника (`serve --machine`, `machine_log_dir()`).
pub const HELPER_LOG: &str = "helper.log";

/// `WTSGetActiveConsoleSessionId` возвращает это, когда активной консольной
/// сессии нет (идёт переключение сеанса).
pub const NO_SESSION: u32 = 0xFFFF_FFFF;

/// Ожидание перед первым перезапуском помощника после выхода.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
/// Потолок экспоненциальной паузы между перезапусками.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Помощник, проживший не меньше этого, считается здоровым: пауза сбрасывается.
pub const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

/// Файлы, которые `install` переносит из каталога установщика в машинный.
pub const MIGRATED_FILES: [&str; 2] = ["device.json", "access.json"];

/// Адрес сервера уходит в командную строку без экранирования, поэтому
/// пробелы и кавычки в нём недопустимы.
pub fn validate_server(server: &str) -> Result<(), String> {
    if server.is_empty() {
        return Err("адрес сервера пуст".to_string());
    }
    if server.chars().any(|c| c.is_whitespace() || c == '"') {
        return Err(format!(
            "адрес сервера не должен содержать пробелов и кавычек: {server}"
        ));
    }
    Ok(())
}

/// `"<exe>" service run [--server <server>]` -- binpath службы. Без явного
/// адреса флага нет: службу настраивает `settings.json` (D47-5).
pub fn service_command_line(exe: &Path, server: Option<&str>) -> String {
    match server {
        Some(server) => format!("\"{}\" service run --server {server}", exe.display()),
        None => format!("\"{}\" service run", exe.display()),
    }
}

/// Адрес сервера для помощника внутри службы и признак режима хаба. Если в
/// `dir/settings.json` включён хаб -- помощник ходит на локальный слушатель
/// хаба, а флаг и поле `server` игнорируются; иначе флаг > settings >
/// [`crate::device::DEFAULT_SERVER`].
pub fn helper_server(flag: Option<String>, dir: &Path) -> (String, bool) {
    if crate::settings::hub_enabled(dir) {
        if flag.is_some() {
            tracing::warn!("включён режим хаба: --server игнорируется");
        }
        let server = format!("ws://127.0.0.1:{}/ws", crate::hub::DEFAULT_LOCAL_PORT);
        tracing::info!(%server, source = "hub", "адрес сигнального сервера");
        (server, true)
    } else {
        (
            crate::settings::resolve_server(flag, dir, crate::device::DEFAULT_SERVER),
            false,
        )
    }
}

/// `"<exe>" serve --machine --server <server>` -- команда помощника.
pub fn helper_command_line(exe: &Path, server: &str) -> String {
    format!("\"{}\" serve --machine --server {server}", exe.display())
}

/// Номер активной консольной сессии из сырого ответа Win32.
pub fn active_session_from_raw(raw: u32) -> Option<u32> {
    if raw == NO_SESSION {
        None
    } else {
        Some(raw)
    }
}

/// Состояние службы для печати (`service status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    ContinuePending,
    PausePending,
    Paused,
}

impl ServiceState {
    /// `dwCurrentState` из `SERVICE_STATUS` (значения `SERVICE_*` из winsvc.h).
    pub fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::Stopped,
            2 => Self::StartPending,
            3 => Self::StopPending,
            4 => Self::Running,
            5 => Self::ContinuePending,
            6 => Self::PausePending,
            7 => Self::Paused,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "не установлена",
            Self::Stopped => "остановлена",
            Self::StartPending => "запускается",
            Self::StopPending => "останавливается",
            Self::Running => "работает",
            Self::ContinuePending => "возобновляется",
            Self::PausePending => "приостанавливается",
            Self::Paused => "приостановлена",
        }
    }
}

/// Что супервизор знает о помощнике. `Some` пока служба держит его хендл,
/// в том числе после выхода (`exited`), до первого тика, который это заметил.
#[derive(Debug, Clone, Copy)]
pub struct HelperState {
    pub session: u32,
    pub exited: bool,
    pub started_at: Instant,
}

/// Решение супервизора на один тик.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// Запустить помощника в этой сессии.
    Launch(u32),
    /// Убить живого помощника (он не в активной консольной сессии).
    Kill,
}

/// Автомат надзора за помощником; времени и Win32 не знает, всё приходит
/// аргументами `tick`.
#[derive(Debug, Default)]
pub struct Supervisor {
    /// Последняя применённая пауза (для удвоения).
    last_backoff: Option<Duration>,
    /// Не запускать раньше этого момента.
    not_before: Option<Instant>,
    /// `started_at` помощника, чей выход уже учтён (тик идемпотентен).
    handled_exit: Option<Instant>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(
        &mut self,
        now: Instant,
        active_session: Option<u32>,
        helper: Option<HelperState>,
    ) -> Action {
        if let Some(h) = helper {
            if h.exited {
                if self.handled_exit != Some(h.started_at) {
                    self.handled_exit = Some(h.started_at);
                    let lived = now.saturating_duration_since(h.started_at);
                    self.register_failure(now, lived);
                }
                // Помощник мёртв: убивать нечего, запуск -- на следующих
                // тиках, когда хендл уже сброшен и пауза истекла.
                return Action::None;
            }
        }

        let Some(active) = active_session else {
            // Сессии нет: ничего не запускаем, живого не трогаем.
            return Action::None;
        };

        match helper {
            Some(h) if h.session != active => Action::Kill,
            Some(_) => Action::None,
            None => match self.not_before {
                Some(t) if now < t => Action::None,
                _ => Action::Launch(active),
            },
        }
    }

    /// `CreateProcessAsUserW` не удался: пауза как после мгновенного выхода.
    pub fn launch_failed(&mut self, now: Instant) {
        self.register_failure(now, Duration::ZERO);
    }

    fn register_failure(&mut self, now: Instant, lived: Duration) {
        let delay = if lived >= HEALTHY_UPTIME {
            BACKOFF_INITIAL
        } else {
            match self.last_backoff {
                Some(prev) => (prev * 2).min(BACKOFF_MAX),
                None => BACKOFF_INITIAL,
            }
        };
        self.last_backoff = Some(delay);
        self.not_before = Some(now + delay);
    }
}

/// Состояние помощника в файле статуса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    Connecting,
    Registered,
    InSession,
}

/// Содержимое `status.json`: `{"state":"in_session","pin":"123456"}`.
/// `pin` есть только у `registered` и `in_session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperStatus {
    pub state: StatusState,
    pub pin: Option<String>,
}

impl HelperStatus {
    /// `Reconnecting` и `Connecting` -- оба `connecting` без PIN.
    pub fn from_agent(status: &AgentStatus) -> Self {
        match status {
            AgentStatus::Connecting | AgentStatus::Reconnecting { .. } => Self {
                state: StatusState::Connecting,
                pin: None,
            },
            AgentStatus::Registered { pin } => Self {
                state: StatusState::Registered,
                pin: Some(pin.clone()),
            },
            AgentStatus::InSession { pin } => Self {
                state: StatusState::InSession,
                pin: Some(pin.clone()),
            },
        }
    }

    /// Обратно в `AgentStatus`; `None`, если у `registered`/`in_session`
    /// нет непустого PIN.
    pub fn to_agent(&self) -> Option<AgentStatus> {
        let pin = self.pin.as_deref().filter(|p| !p.is_empty());
        match self.state {
            StatusState::Connecting => Some(AgentStatus::Connecting),
            StatusState::Registered => pin.map(|p| AgentStatus::Registered { pin: p.to_string() }),
            StatusState::InSession => pin.map(|p| AgentStatus::InSession { pin: p.to_string() }),
        }
    }

    /// Сериализованное содержимое файла (для записи и дедупа).
    pub fn to_bytes(&self) -> Vec<u8> {
        // Структура из строк и enum сериализуется всегда.
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// Статус из `dir/status.json`; `None`, если файла нет, он не читается или
/// содержит не тот JSON.
pub fn read_status(dir: &Path) -> Option<HelperStatus> {
    let bytes = std::fs::read(dir.join(STATUS_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// PIN из `dir/status.json` (есть в `registered` и `in_session`).
pub fn read_pin(dir: &Path) -> Option<String> {
    let status = read_status(dir)?;
    match status.state {
        StatusState::Connecting => None,
        StatusState::Registered | StatusState::InSession => {
            status.pin.filter(|pin| !pin.is_empty())
        }
    }
}

/// Удаляет устаревший `pin.txt` (ошибки игнорируются).
pub fn remove_legacy_pin_file(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(LEGACY_PIN_FILE));
}

/// Запись файла статуса не на Windows: приватно (0600) и атомарно. На
/// Windows то же делает `platform::windows::status_file` с DACL.
pub fn write_status_file_portable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::fsutil::write_private_atomic(path, bytes)
}

/// SDDL файла статуса на Windows: SYSTEM и Administrators -- полный доступ,
/// пользователь активной консоли (если он есть) -- чтение. DACL защищён от
/// наследования.
pub fn status_file_sddl(console_user_sid: Option<&str>) -> String {
    let mut sddl = String::from("D:P(A;;FA;;;SY)(A;;FA;;;BA)");
    if let Some(sid) = console_user_sid {
        sddl.push_str("(A;;FR;;;");
        sddl.push_str(sid);
        sddl.push(')');
    }
    sddl
}

/// Копирует из `from` в `to` те файлы из `names`, которые есть в `from` и
/// отсутствуют в `to`. Возвращает имена скопированных.
pub fn migrate_files(from: &Path, to: &Path, names: &[&str]) -> io::Result<Vec<String>> {
    let mut copied = Vec::new();
    for name in names {
        let src = from.join(name);
        let dst = to.join(name);
        if dst.exists() || !src.is_file() {
            continue;
        }
        std::fs::copy(&src, &dst)?;
        copied.push((*name).to_string());
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-service-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn alive(session: u32, started_at: Instant) -> Option<HelperState> {
        Some(HelperState {
            session,
            exited: false,
            started_at,
        })
    }

    fn dead(session: u32, started_at: Instant) -> Option<HelperState> {
        Some(HelperState {
            session,
            exited: true,
            started_at,
        })
    }

    #[test]
    fn command_lines_quote_the_exe() {
        let exe = Path::new(r"C:\Program Files\rcdesk\rcdesk-host.exe");
        assert_eq!(
            service_command_line(exe, Some("wss://rcdesk.app/ws")),
            r#""C:\Program Files\rcdesk\rcdesk-host.exe" service run --server wss://rcdesk.app/ws"#
        );
        assert_eq!(
            service_command_line(exe, None),
            r#""C:\Program Files\rcdesk\rcdesk-host.exe" service run"#
        );
        assert_eq!(
            helper_command_line(exe, "wss://rcdesk.app/ws"),
            r#""C:\Program Files\rcdesk\rcdesk-host.exe" serve --machine --server wss://rcdesk.app/ws"#
        );
    }

    #[test]
    fn helper_server_in_hub_mode_ignores_the_flag_and_the_file_server() {
        let dir = scratch_dir("hub-mode");
        std::fs::write(
            dir.join(crate::settings::SETTINGS_FILE),
            r#"{"hub":true,"server":"wss://other/ws"}"#,
        )
        .unwrap();
        let want = ("ws://127.0.0.1:8444/ws".to_string(), true);
        assert_eq!(helper_server(None, &dir), want);
        assert_eq!(helper_server(Some("ws://x:1/ws".into()), &dir), want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn helper_server_without_hub_prefers_flag_then_settings_then_default() {
        let dir = scratch_dir("no-hub");
        assert_eq!(
            helper_server(None, &dir),
            ("wss://rcdesk.app/ws".to_string(), false)
        );
        std::fs::write(
            dir.join(crate::settings::SETTINGS_FILE),
            r#"{"server":"wss://hub.lan:8443/ws"}"#,
        )
        .unwrap();
        assert_eq!(
            helper_server(None, &dir),
            ("wss://hub.lan:8443/ws".to_string(), false)
        );
        assert_eq!(
            helper_server(Some("ws://x:1/ws".into()), &dir),
            ("ws://x:1/ws".to_string(), false)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_validation_rejects_spaces_and_quotes() {
        assert!(validate_server("wss://rcdesk.app/ws").is_ok());
        assert!(validate_server("").is_err());
        assert!(validate_server("wss://a b/ws").is_err());
        assert!(validate_server("wss://a\"b/ws").is_err());
    }

    #[test]
    fn session_raw_conversion() {
        assert_eq!(active_session_from_raw(NO_SESSION), None);
        assert_eq!(active_session_from_raw(1), Some(1));
        assert_eq!(active_session_from_raw(0), Some(0));
    }

    #[test]
    fn service_state_from_raw() {
        assert_eq!(ServiceState::from_raw(4), Some(ServiceState::Running));
        assert_eq!(ServiceState::from_raw(1), Some(ServiceState::Stopped));
        assert_eq!(ServiceState::from_raw(99), None);
        assert_eq!(ServiceState::Running.label(), "работает");
    }

    #[test]
    fn no_session_launches_nothing_and_spares_a_live_helper() {
        let now = Instant::now();
        let mut sup = Supervisor::new();
        assert_eq!(sup.tick(now, None, None), Action::None);
        assert_eq!(sup.tick(now, None, alive(1, now)), Action::None);
    }

    #[test]
    fn launches_in_the_active_session_when_there_is_no_helper() {
        let now = Instant::now();
        let mut sup = Supervisor::new();
        assert_eq!(sup.tick(now, Some(2), None), Action::Launch(2));
    }

    #[test]
    fn healthy_helper_in_the_active_session_is_left_alone() {
        let now = Instant::now();
        let mut sup = Supervisor::new();
        assert_eq!(sup.tick(now, Some(1), alive(1, now)), Action::None);
    }

    #[test]
    fn helper_in_another_session_is_killed() {
        let now = Instant::now();
        let mut sup = Supervisor::new();
        assert_eq!(sup.tick(now, Some(2), alive(1, now)), Action::Kill);
    }

    #[test]
    fn kill_is_not_followed_by_backoff() {
        let now = Instant::now();
        let mut sup = Supervisor::new();
        assert_eq!(sup.tick(now, Some(2), alive(1, now)), Action::Kill);
        // Хендл сброшен исполнителем -- запуск в новой сессии сразу.
        assert_eq!(sup.tick(now, Some(2), None), Action::Launch(2));
    }

    #[test]
    fn exited_helper_waits_one_second_before_relaunch() {
        let t0 = Instant::now();
        let mut sup = Supervisor::new();
        let started = t0;
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(sup.tick(t1, Some(1), dead(1, started)), Action::None);
        assert_eq!(sup.tick(t1, Some(1), None), Action::None);
        assert_eq!(
            sup.tick(t1 + Duration::from_millis(999), Some(1), None),
            Action::None
        );
        assert_eq!(
            sup.tick(t1 + Duration::from_secs(1), Some(1), None),
            Action::Launch(1)
        );
    }

    #[test]
    fn repeated_quick_exits_double_the_backoff_up_to_the_cap() {
        let mut now = Instant::now();
        let mut sup = Supervisor::new();
        let expected = [1u64, 2, 4, 8, 16, 30, 30];
        for secs in expected {
            let started = now;
            now += Duration::from_millis(100);
            assert_eq!(sup.tick(now, Some(1), dead(1, started)), Action::None);
            let wait = Duration::from_secs(secs);
            assert_eq!(
                sup.tick(now + wait - Duration::from_millis(1), Some(1), None),
                Action::None,
                "pause {secs}s not over yet"
            );
            now += wait;
            assert_eq!(sup.tick(now, Some(1), None), Action::Launch(1));
        }
    }

    #[test]
    fn exit_is_counted_once_even_if_the_handle_lingers() {
        let t0 = Instant::now();
        let mut sup = Supervisor::new();
        let h = dead(1, t0);
        let t1 = t0 + Duration::from_secs(1);
        sup.tick(t1, Some(1), h);
        sup.tick(t1 + Duration::from_millis(300), Some(1), h);
        sup.tick(t1 + Duration::from_millis(600), Some(1), h);
        // Пауза осталась 1 с, а не 4 с.
        assert_eq!(
            sup.tick(t1 + Duration::from_secs(1), Some(1), None),
            Action::Launch(1)
        );
    }

    #[test]
    fn backoff_resets_after_a_long_run() {
        let mut now = Instant::now();
        let mut sup = Supervisor::new();
        // Две быстрые смерти: пауза выросла до 2 с.
        for secs in [1u64, 2] {
            let started = now;
            now += Duration::from_millis(100);
            sup.tick(now, Some(1), dead(1, started));
            now += Duration::from_secs(secs);
            assert_eq!(sup.tick(now, Some(1), None), Action::Launch(1));
        }
        // Следующий прожил минуту -- пауза снова 1 с.
        let started = now;
        now += HEALTHY_UPTIME;
        sup.tick(now, Some(1), dead(1, started));
        assert_eq!(
            sup.tick(now + Duration::from_secs(1), Some(1), None),
            Action::Launch(1)
        );
    }

    #[test]
    fn launch_failure_applies_the_same_backoff() {
        let now = Instant::now();
        let mut sup = Supervisor::new();
        assert_eq!(sup.tick(now, Some(1), None), Action::Launch(1));
        sup.launch_failed(now);
        assert_eq!(sup.tick(now, Some(1), None), Action::None);
        assert_eq!(
            sup.tick(now + Duration::from_secs(1), Some(1), None),
            Action::Launch(1)
        );
        sup.launch_failed(now + Duration::from_secs(1));
        assert_eq!(
            sup.tick(now + Duration::from_secs(2), Some(1), None),
            Action::None
        );
        assert_eq!(
            sup.tick(now + Duration::from_secs(3), Some(1), None),
            Action::Launch(1)
        );
    }

    #[test]
    fn exited_helper_without_a_session_still_starts_the_backoff() {
        let t0 = Instant::now();
        let mut sup = Supervisor::new();
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(sup.tick(t1, None, dead(1, t0)), Action::None);
        assert_eq!(sup.tick(t1, Some(1), None), Action::None);
        assert_eq!(
            sup.tick(t1 + Duration::from_secs(1), Some(1), None),
            Action::Launch(1)
        );
    }

    #[test]
    fn agent_status_round_trips_through_the_status_file() {
        let cases = [
            (AgentStatus::Connecting, AgentStatus::Connecting),
            (
                AgentStatus::Reconnecting {
                    error: "x".into(),
                    retry_in: Duration::from_secs(3),
                },
                AgentStatus::Connecting,
            ),
            (
                AgentStatus::Registered {
                    pin: "123456".into(),
                },
                AgentStatus::Registered {
                    pin: "123456".into(),
                },
            ),
            (
                AgentStatus::InSession {
                    pin: "654321".into(),
                },
                AgentStatus::InSession {
                    pin: "654321".into(),
                },
            ),
        ];
        let dir = scratch_dir("status-round-trip");
        for (input, expected) in cases {
            let bytes = HelperStatus::from_agent(&input).to_bytes();
            std::fs::write(dir.join(STATUS_FILE), &bytes).unwrap();
            let back = read_status(&dir).and_then(|s| s.to_agent());
            assert_eq!(back, Some(expected));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_json_has_the_documented_shape() {
        let bytes = HelperStatus::from_agent(&AgentStatus::InSession {
            pin: "111222".into(),
        })
        .to_bytes();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"state":"in_session","pin":"111222"}"#
        );
        let bytes = HelperStatus::from_agent(&AgentStatus::Connecting).to_bytes();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"state":"connecting","pin":null}"#
        );
    }

    #[test]
    fn registered_without_pin_is_rejected() {
        let status = HelperStatus {
            state: StatusState::Registered,
            pin: None,
        };
        assert_eq!(status.to_agent(), None);
    }

    #[test]
    fn pin_is_read_from_the_status_file() {
        let dir = scratch_dir("pin");
        assert_eq!(read_pin(&dir), None);
        let write = |text: &str| std::fs::write(dir.join(STATUS_FILE), text).unwrap();
        write(r#"{"state":"in_session","pin":"123456"}"#);
        assert_eq!(read_pin(&dir).as_deref(), Some("123456"));
        write(r#"{"state":"registered","pin":"654321"}"#);
        assert_eq!(read_pin(&dir).as_deref(), Some("654321"));
        write(r#"{"state":"connecting","pin":null}"#);
        assert_eq!(read_pin(&dir), None);
        write(r#"{"state":"connecting","pin":"111111"}"#);
        assert_eq!(read_pin(&dir), None);
        write("{broken");
        assert_eq!(read_pin(&dir), None);
        assert_eq!(read_status(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_pin_file_is_removed() {
        let dir = scratch_dir("legacy-pin");
        std::fs::write(dir.join(LEGACY_PIN_FILE), "123456").unwrap();
        remove_legacy_pin_file(&dir);
        assert!(!dir.join(LEGACY_PIN_FILE).exists());
        remove_legacy_pin_file(&dir); // повторно и без файла -- не ошибка
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_sddl_with_and_without_user() {
        assert_eq!(status_file_sddl(None), "D:P(A;;FA;;;SY)(A;;FA;;;BA)");
        assert_eq!(
            status_file_sddl(Some("S-1-5-21-1-2-3-1001")),
            "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;S-1-5-21-1-2-3-1001)"
        );
    }

    #[test]
    fn migration_copies_only_missing_files() {
        let from = scratch_dir("mig-from");
        let to = scratch_dir("mig-to");
        std::fs::write(from.join("device.json"), b"user-device").unwrap();
        std::fs::write(from.join("access.json"), b"user-access").unwrap();
        std::fs::write(to.join("device.json"), b"machine-device").unwrap();

        let copied = migrate_files(&from, &to, &MIGRATED_FILES).unwrap();

        assert_eq!(copied, vec!["access.json".to_string()]);
        assert_eq!(
            std::fs::read(to.join("device.json")).unwrap(),
            b"machine-device"
        );
        assert_eq!(
            std::fs::read(to.join("access.json")).unwrap(),
            b"user-access"
        );
        let _ = std::fs::remove_dir_all(&from);
        let _ = std::fs::remove_dir_all(&to);
    }

    #[test]
    fn migration_skips_files_missing_in_the_source() {
        let from = scratch_dir("mig-empty-from");
        let to = scratch_dir("mig-empty-to");
        let copied = migrate_files(&from, &to, &MIGRATED_FILES).unwrap();
        assert!(copied.is_empty());
        let _ = std::fs::remove_dir_all(&from);
        let _ = std::fs::remove_dir_all(&to);
    }
}
