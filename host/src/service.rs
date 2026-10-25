//! Чистая (без Win32) логика «полного режима» Windows (слайс 4.1a),
//! `ARCHITECTURE.md` §4.1/§8: служба SYSTEM `rcdesk` + помощник
//! (`rcdesk-host serve --machine`) в активной консольной сессии. Всё
//! платформенное (SCM, токены, `CreateProcessAsUserW`) живёт в
//! `platform::windows::service`; здесь -- то, что тестируется на любой ОС:
//! командные строки службы и помощника, автомат-супервизор помощника,
//! файл `pin.txt` в машинном каталоге данных, перенос файлов из
//! пользовательского каталога установщика.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Имя службы в SCM.
pub const SERVICE_NAME: &str = "rcdesk";
/// PIN текущей регистрации: помощник пишет, `service status` читает.
pub const PIN_FILE: &str = "pin.txt";
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

/// Записывает PIN в `dir/pin.txt` (атомарно, через `fsutil`).
pub fn write_pin(dir: &Path, pin: &str) -> io::Result<()> {
    crate::fsutil::write_private_atomic(&dir.join(PIN_FILE), pin.as_bytes())
}

/// PIN из `dir/pin.txt`; `None`, если файла нет, он пуст или не читается.
pub fn read_pin(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(PIN_FILE)).ok()?;
    let pin = text.trim();
    if pin.is_empty() {
        None
    } else {
        Some(pin.to_string())
    }
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
    fn pin_round_trip() {
        let dir = scratch_dir("pin");
        assert_eq!(read_pin(&dir), None);
        write_pin(&dir, "123456").unwrap();
        assert_eq!(read_pin(&dir).as_deref(), Some("123456"));
        write_pin(&dir, "654321").unwrap();
        assert_eq!(read_pin(&dir).as_deref(), Some("654321"));
        let _ = std::fs::remove_dir_all(&dir);
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
