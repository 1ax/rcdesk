//! Полный режим macOS (D51, под-шаг 2), `ARCHITECTURE.md` §8: один системный
//! LaunchAgent `/Library/LaunchAgents/app.rcdesk.agent.plist` с
//! `LimitLoadToSessionType = [Aqua, LoginWindow]`. launchd поднимает из него
//! два вида инстансов одного бинарника: в GUI-сеансе пользователя (роль
//! «сеанс») и в сеансе loginwindow от root (роль «экран входа»).
//!
//! Здесь только чистая логика без `cfg` и платформенных вызовов (plist,
//! пути, роль, автомат гейта консоли), тестируемая на любой ОС. Платформенный
//! IO будет в `platform::macos::fullmode` (аналогия: `service.rs` ↔
//! `platform::windows::service`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::autostart::{escape_xml, LAUNCH_AGENT_LABEL};

pub const SYSTEM_LAUNCH_AGENTS_DIR: &str = "/Library/LaunchAgents";
pub const PLIST_FILE_NAME: &str = "app.rcdesk.agent.plist";
pub const MACHINE_DATA_DIR: &str = "/Library/Application Support/rcdesk";
pub const MACHINE_LOG_DIR: &str = "/Library/Logs/rcdesk";
/// Машинный лок «кто владеет регистрацией», в машинном каталоге данных.
pub const REGISTRATION_LOCK_FILE: &str = "registration.lock";

/// Группа `admin` на macOS.
pub const ADMIN_GID: u32 = 80;
/// Режим машинного каталога: setgid, чтобы новые файлы наследовали `admin`.
pub const DIR_MODE: u32 = 0o2770;
/// Режим файлов машинного каталога: читают и пишут и root, и администратор.
pub const SHARED_FILE_MODE: u32 = 0o660;

/// Код выхода роли «экран входа» при потере консоли. Не 0: в plist
/// `KeepAlive { SuccessfulExit = false }`, поэтому launchd перезапустит
/// инстанс в режим ожидания, а чистый выход (0) перезапуска не вызывает.
pub const EXIT_CONSOLE_TAKEN: i32 = 3;

/// Период опроса гейта консоли.
pub const GATE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Сколько сеансовая роль ждёт освобождения машинного лока регистрации при
/// старте. Роль «экран входа» уступает за `GATE_POLL_INTERVAL`; запас — на её
/// выход и на перезапуск launchd.
pub const REGISTRATION_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Период повторных попыток взять машинный лок регистрации при ожидании.
pub const REGISTRATION_WAIT_POLL: Duration = Duration::from_millis(500);
/// Сколько условие «свободно» должно держаться непрерывно, прежде чем
/// начинать служить (при быстрой смене пользователя консоль кратко уходит root).
pub const GATE_OPEN_DEBOUNCE: Duration = Duration::from_secs(3);

/// XML системного plist полного режима. Отличие от
/// `agent::autostart::generate_plist`: `LimitLoadToSessionType` -- массив
/// из двух типов сеанса, а не одиночная строка.
pub fn generate_system_plist(agent: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{path}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <array>
        <string>Aqua</string>
        <string>LoginWindow</string>
    </array>
</dict>
</plist>
"#,
        label = LAUNCH_AGENT_LABEL,
        path = escape_xml(&agent.to_string_lossy()),
    )
}

/// `/Library/LaunchAgents/app.rcdesk.agent.plist`.
pub fn plist_path() -> PathBuf {
    plist_path_in(Path::new(SYSTEM_LAUNCH_AGENTS_DIR))
}

/// Путь plist в произвольном каталоге (для тестов).
pub fn plist_path_in(dir: &Path) -> PathBuf {
    dir.join(PLIST_FILE_NAME)
}

/// Полный режим установлен = системный plist существует.
pub fn is_installed() -> bool {
    is_installed_in(Path::new(SYSTEM_LAUNCH_AGENTS_DIR))
}

/// [`is_installed`] для произвольного каталога (для тестов).
pub fn is_installed_in(dir: &Path) -> bool {
    plist_path_in(dir).exists()
}

/// Последовательность попыток загрузить задание в GUI-сеанс пользователя `uid`
/// (аргументы `launchctl` без самого пути к нему, кроме `asuser`-форм).
///
/// Почему `asuser`: `install` идёт из root-процесса (`sudo`), а он не находится
/// в bootstrap-контексте сеанса Aqua. Голый `launchctl bootstrap gui/<uid> ...`
/// оттуда падает с `Bootstrap failed: 5: Input/output error`, а `kickstart` --
/// кодом 37, и агент остаётся мёртвым. `launchctl asuser <uid> ...` переносит
/// команду в контекст сессии пользователя, и она срабатывает. Не «упрощать»
/// обратно до голых команд. Попытки 1-2 -- боевой путь из root; 3-4 запасные,
/// на случай когда `install` зовут уже из пользовательского контекста.
pub fn load_job_attempts(launchctl: &str, uid: &str, plist: &str) -> Vec<Vec<String>> {
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{LAUNCH_AGENT_LABEL}");
    let v = |parts: &[&str]| parts.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
    vec![
        v(&["asuser", uid, launchctl, "bootstrap", &domain, plist]),
        v(&["asuser", uid, launchctl, "kickstart", "-k", &target]),
        v(&["bootstrap", &domain, plist]),
        v(&["kickstart", "-k", &target]),
    ]
}

/// Попытки подтвердить состояние задания после загрузки (`launchctl print`).
///
/// `asuser` первым: тот же контекст сеанса, что у сработавшего `bootstrap`; из
/// root голая форма может быть недоступна. Запасная -- без `asuser`. Это только
/// чтение: отсутствие результата не признак поломки агента.
pub fn print_job_attempts(launchctl: &str, uid: &str) -> Vec<Vec<String>> {
    let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
    let v = |parts: &[&str]| parts.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
    vec![
        v(&["asuser", uid, launchctl, "print", &target]),
        v(&["print", &target]),
    ]
}

/// Роль инстанса, поднятого из системного plist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// GUI-сеанс пользователя: трей, шторка, обновления.
    Session,
    /// Сеанс loginwindow (root): без трея, шторки и обновлений.
    LoginScreen,
}

/// `uid == 0` -- экран входа, иначе сеанс пользователя.
pub fn role_from_uid(uid: u32) -> Role {
    if uid == 0 {
        Role::LoginScreen
    } else {
        Role::Session
    }
}

/// Решение гейта консоли на очередном опросе.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Не служим, ждём.
    Wait,
    /// Начать служить (возвращается ровно один раз).
    Serve,
    /// Служим, условие в силе.
    Keep,
    /// Служили, условие ушло: выйти с [`EXIT_CONSOLE_TAKEN`].
    Yield,
}

/// Автомат гейта консоли роли «экран входа». Условие «свободно» =
/// `console_uid == 0 && !registration_busy`.
#[derive(Debug, Default)]
pub struct ConsoleGate {
    serving: bool,
    /// С какого момента условие выполняется непрерывно (пока не служим).
    open_since: Option<Instant>,
}

impl ConsoleGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(
        &mut self,
        now: Instant,
        console_uid: u32,
        registration_busy: bool,
    ) -> GateDecision {
        let open = console_uid == 0 && !registration_busy;
        if self.serving {
            return if open {
                GateDecision::Keep
            } else {
                GateDecision::Yield
            };
        }
        if !open {
            self.open_since = None;
            return GateDecision::Wait;
        }
        let since = *self.open_since.get_or_insert(now);
        if now.duration_since(since) >= GATE_OPEN_DEBOUNCE {
            self.serving = true;
            self.open_since = None;
            GateDecision::Serve
        } else {
            GateDecision::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::autostart::parse_program_arguments_path;

    #[test]
    fn load_job_attempts_order_and_values() {
        let a = load_job_attempts("/bin/launchctl", "501", "/Library/LaunchAgents/x.plist");
        let expected: Vec<Vec<&str>> = vec![
            vec![
                "asuser",
                "501",
                "/bin/launchctl",
                "bootstrap",
                "gui/501",
                "/Library/LaunchAgents/x.plist",
            ],
            vec![
                "asuser",
                "501",
                "/bin/launchctl",
                "kickstart",
                "-k",
                "gui/501/app.rcdesk.agent",
            ],
            vec!["bootstrap", "gui/501", "/Library/LaunchAgents/x.plist"],
            vec!["kickstart", "-k", "gui/501/app.rcdesk.agent"],
        ];
        assert_eq!(a.len(), 4);
        assert_eq!(a, expected);
    }

    #[test]
    fn print_job_attempts_order_and_values() {
        let a = print_job_attempts("/bin/launchctl", "501");
        let expected: Vec<Vec<&str>> = vec![
            vec![
                "asuser",
                "501",
                "/bin/launchctl",
                "print",
                "gui/501/app.rcdesk.agent",
            ],
            vec!["print", "gui/501/app.rcdesk.agent"],
        ];
        assert_eq!(a, expected);
    }

    #[test]
    fn plist_has_all_keys_and_both_session_types() {
        let xml = generate_system_plist(Path::new("/usr/local/bin/rcdesk-agent"));
        for needle in [
            "<key>Label</key>",
            "<string>app.rcdesk.agent</string>",
            "<key>ProgramArguments</key>",
            "<key>RunAtLoad</key>",
            "<key>KeepAlive</key>",
            "<key>SuccessfulExit</key>",
            "<false/>",
            "<key>ProcessType</key>",
            "<string>Interactive</string>",
            "<key>LimitLoadToSessionType</key>",
            "<array>\n        <string>Aqua</string>\n        <string>LoginWindow</string>\n    </array>",
        ] {
            assert!(xml.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn plist_path_round_trips() {
        let xml = generate_system_plist(Path::new("/Applications/rcdesk/rcdesk-agent"));
        assert_eq!(
            parse_program_arguments_path(&xml).as_deref(),
            Some("/Applications/rcdesk/rcdesk-agent")
        );
    }

    #[test]
    fn plist_escapes_path() {
        let path = "/tmp/a&b<c>/rcdesk-agent";
        let xml = generate_system_plist(Path::new(path));
        assert!(xml.contains("/tmp/a&amp;b&lt;c&gt;/rcdesk-agent"));
        assert!(!xml.contains("a&b<c>"));
        assert_eq!(parse_program_arguments_path(&xml).as_deref(), Some(path));
    }

    #[test]
    fn is_installed_in_checks_plist_existence() {
        let dir = std::env::temp_dir().join(format!("rcdesk-fullmode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_installed_in(&dir));
        std::fs::write(plist_path_in(&dir), "x").unwrap();
        assert!(is_installed_in(&dir));
        assert!(plist_path_in(&dir).ends_with(PLIST_FILE_NAME));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn role_from_uid_maps_root_to_login_screen() {
        assert_eq!(role_from_uid(0), Role::LoginScreen);
        assert_eq!(role_from_uid(501), Role::Session);
    }

    #[test]
    fn gate_debounces_then_serves_exactly_once() {
        let t0 = Instant::now();
        let mut gate = ConsoleGate::new();
        assert_eq!(gate.tick(t0, 0, false), GateDecision::Wait);
        let almost = t0 + GATE_OPEN_DEBOUNCE - Duration::from_millis(1);
        assert_eq!(gate.tick(almost, 0, false), GateDecision::Wait);
        assert_eq!(
            gate.tick(t0 + GATE_OPEN_DEBOUNCE, 0, false),
            GateDecision::Serve
        );
    }

    #[test]
    fn gate_keeps_after_serve() {
        let t0 = Instant::now();
        let mut gate = ConsoleGate::new();
        gate.tick(t0, 0, false);
        let t1 = t0 + GATE_OPEN_DEBOUNCE;
        assert_eq!(gate.tick(t1, 0, false), GateDecision::Serve);
        assert_eq!(
            gate.tick(t1 + GATE_POLL_INTERVAL, 0, false),
            GateDecision::Keep
        );
    }

    #[test]
    fn gate_yields_when_user_takes_console() {
        let t0 = Instant::now();
        let mut gate = ConsoleGate::new();
        gate.tick(t0, 0, false);
        let t1 = t0 + GATE_OPEN_DEBOUNCE;
        assert_eq!(gate.tick(t1, 0, false), GateDecision::Serve);
        assert_eq!(
            gate.tick(t1 + GATE_POLL_INTERVAL, 501, false),
            GateDecision::Yield
        );
    }

    #[test]
    fn gate_waits_while_registration_is_busy() {
        let t0 = Instant::now();
        let mut gate = ConsoleGate::new();
        for i in 0..5 {
            let now = t0 + GATE_POLL_INTERVAL * i;
            assert_eq!(gate.tick(now, 0, true), GateDecision::Wait);
        }
    }

    #[test]
    fn gate_flicker_resets_debounce() {
        let t0 = Instant::now();
        let mut gate = ConsoleGate::new();
        assert_eq!(gate.tick(t0, 0, false), GateDecision::Wait);
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(gate.tick(t1, 501, false), GateDecision::Wait);
        let t2 = t0 + Duration::from_secs(4);
        assert_eq!(gate.tick(t2, 0, false), GateDecision::Wait);
        // 3 с от первого замера прошло, но отсчёт начался заново с t2.
        assert_eq!(
            gate.tick(t2 + Duration::from_secs(2), 0, false),
            GateDecision::Wait
        );
        assert_eq!(
            gate.tick(t2 + GATE_OPEN_DEBOUNCE, 0, false),
            GateDecision::Serve
        );
    }
}
