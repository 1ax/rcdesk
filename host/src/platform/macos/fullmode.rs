//! Платформенный IO полного режима macOS (D51, под-шаг 2): установка и снятие
//! системного LaunchAgent, машинные каталоги, `euid`, владелец `/dev/console`.
//! Чистая логика (plist, пути, роль, автомат гейта консоли) -- в
//! `crate::fullmode`; аналогия: `platform::windows::service` ↔ `service.rs`.
//!
//! `launchctl` зовётся только отсюда (из CLI `fullmode install/uninstall`):
//! сам `rcdesk-agent` plist'ы не грузит, как и в лёгком режиме.

use std::fs;
use std::os::unix::fs::{chown, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

use crate::agent::autostart::{parse_program_arguments_path, LAUNCH_AGENT_LABEL};
use crate::agent::paths;
use crate::fullmode::{
    generate_system_plist, load_job_attempts, plist_path, print_job_attempts, role_from_uid, Role,
    ADMIN_GID, DIR_MODE, REGISTRATION_LOCK_FILE, SHARED_FILE_MODE,
};

const LAUNCHCTL: &str = "/bin/launchctl";

/// Эффективный uid процесса.
pub fn euid() -> u32 {
    // Safety: `geteuid` не принимает аргументов и указателей и не может
    // завершиться ошибкой.
    unsafe { libc::geteuid() }
}

/// Владелец `/dev/console`. На экране входа консолью владеет root, в сеансе --
/// вошедший пользователь (тот же признак, по которому работают скрипты
/// пакета). `None`, если метаданные не прочитались.
pub fn console_uid() -> Option<u32> {
    fs::metadata("/dev/console").ok().map(|m| m.uid())
}

/// Машинный каталог годен, если им владеет root и он не символическая ссылка.
pub fn machine_dir_ok(owner_uid: u32, is_symlink: bool) -> bool {
    owner_uid == 0 && !is_symlink
}

/// Состояние полного режима для печати в `rcdesk-host fullmode status`.
#[derive(Debug)]
pub struct Status {
    pub installed: bool,
    pub plist: PathBuf,
    /// `ProgramArguments[0]` из установленного plist.
    pub plist_agent: Option<String>,
    pub data_dir: PathBuf,
    pub data_dir_exists: bool,
    /// Владелец, группа и режим (младшие 12 бит) каталога данных.
    pub data_dir_owner: Option<(u32, u32, u32)>,
    pub data_dir_ok: bool,
    pub log_dir: PathBuf,
    pub log_dir_exists: bool,
    pub console_uid: Option<u32>,
    pub role: Role,
    pub device_json: bool,
    pub access_json: bool,
    /// Машинный лок регистрации занят (`None` -- проверить не удалось).
    pub registration_busy: Option<bool>,
}

pub fn status() -> anyhow::Result<Status> {
    let plist = plist_path();
    let installed = crate::fullmode::is_installed();
    let plist_agent = fs::read_to_string(&plist)
        .ok()
        .and_then(|xml| parse_program_arguments_path(&xml));
    let data_dir = paths::machine_data_dir();
    let meta = fs::symlink_metadata(&data_dir).ok();
    let data_dir_ok = meta
        .as_ref()
        .is_some_and(|m| machine_dir_ok(m.uid(), m.file_type().is_symlink()));
    let log_dir = paths::machine_log_dir();
    // Лок берём на лету и тут же отпускаем; каталог не создаём, если его нет.
    let registration_busy = if data_dir.is_dir() {
        match crate::agent::lock::acquire_named(&data_dir, REGISTRATION_LOCK_FILE) {
            Ok(Some(_guard)) => Some(false),
            Ok(None) => Some(true),
            Err(_) => None,
        }
    } else {
        Some(false)
    };
    Ok(Status {
        installed,
        plist,
        plist_agent,
        data_dir_exists: meta.is_some(),
        data_dir_owner: meta.as_ref().map(|m| (m.uid(), m.gid(), m.mode() & 0o7777)),
        data_dir_ok,
        log_dir_exists: log_dir.exists(),
        console_uid: console_uid(),
        role: role_from_uid(euid()),
        device_json: data_dir.join("device.json").is_file(),
        access_json: data_dir.join("access.json").is_file(),
        registration_busy,
        data_dir,
        log_dir,
    })
}

/// Создаёт (или проверяет) машинный каталог: root:admin, `02770`.
fn prepare_machine_dir(dir: &Path) -> anyhow::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(dir) {
        if !machine_dir_ok(meta.uid(), meta.file_type().is_symlink()) {
            bail!(
                "{} уже существует, но это символическая ссылка или владелец не root: \
                 удалите каталог (sudo rm -rf) и повторите установку",
                dir.display()
            );
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
    chown(dir, Some(0), Some(ADMIN_GID))
        .with_context(|| format!("chown root:admin {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))
        .with_context(|| format!("chmod {:o} {}", DIR_MODE, dir.display()))?;
    Ok(())
}

/// Домашний каталог пользователя, запустившего `sudo` (не root).
fn sudo_user() -> Option<String> {
    let user = std::env::var("SUDO_USER").ok()?;
    // Имя уходит в путь: пустое, root и со слэшем не принимаем.
    if user.is_empty() || user == "root" || user.contains('/') {
        None
    } else {
        Some(user)
    }
}

fn user_home(user: &str) -> PathBuf {
    PathBuf::from("/Users").join(user)
}

fn sudo_uid() -> Option<String> {
    let uid = std::env::var("SUDO_UID").ok()?;
    if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) {
        Some(uid)
    } else {
        None
    }
}

/// Запуск `launchctl`; `Ok(true)` -- код выхода 0. Вывод пишется в лог.
fn launchctl(args: &[&str]) -> anyhow::Result<bool> {
    let output = Command::new(LAUNCHCTL)
        .args(args)
        .output()
        .with_context(|| format!("запуск {LAUNCHCTL}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    tracing::info!(
        args = ?args,
        status = %output.status,
        stdout = %stdout.trim(),
        stderr = %stderr.trim(),
        "launchctl"
    );
    Ok(output.status.success())
}

/// `launchctl` со stdout: `Ok(Some(stdout))` при коде выхода 0, иначе `None`.
fn launchctl_stdout(args: &[&str]) -> anyhow::Result<Option<String>> {
    let output = Command::new(LAUNCHCTL)
        .args(args)
        .output()
        .with_context(|| format!("запуск {LAUNCHCTL}"))?;
    tracing::info!(
        args = ?args,
        status = %output.status,
        stdout = %String::from_utf8_lossy(&output.stdout).trim(),
        stderr = %String::from_utf8_lossy(&output.stderr).trim(),
        "launchctl"
    );
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
}

/// Строки состояния из вывода `launchctl print` (`state = ...`, `pid = ...`).
fn job_state_lines(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| l.starts_with("state =") || l.starts_with("pid ="))
        .map(str::to_string)
        .collect()
}

pub fn install(agent: &Path) -> anyhow::Result<()> {
    if euid() != 0 {
        bail!(
            "нужны права администратора: запустите команду через sudo \
             (sudo /Applications/rcdesk.app/Contents/MacOS/rcdesk-host fullmode install)"
        );
    }

    // 2. Машинные каталоги.
    let data_dir = paths::machine_data_dir();
    prepare_machine_dir(&data_dir)?;
    println!("Каталог данных: {} (root:admin, 2770)", data_dir.display());
    let log_dir = paths::machine_log_dir();
    prepare_machine_dir(&log_dir)?;
    println!("Каталог логов: {} (root:admin, 2770)", log_dir.display());

    // 3. Перенос состояния пользователя, запустившего sudo.
    let user = sudo_user();
    let source = match &user {
        Some(user) => user_home(user).join("Library/Application Support/rcdesk"),
        None => paths::data_dir(),
    };
    let copied = crate::service::migrate_files(&source, &data_dir, &crate::service::MIGRATED_FILES)
        .with_context(|| format!("перенос состояния из {}", source.display()))?;
    for name in &copied {
        let dst = data_dir.join(name);
        chown(&dst, Some(0), Some(ADMIN_GID))
            .with_context(|| format!("chown root:admin {}", dst.display()))?;
        fs::set_permissions(&dst, fs::Permissions::from_mode(SHARED_FILE_MODE))
            .with_context(|| format!("chmod {:o} {}", SHARED_FILE_MODE, dst.display()))?;
    }
    if copied.is_empty() {
        println!(
            "Перенос состояния из {}: нечего переносить (либо уже есть в машинном каталоге)",
            source.display()
        );
    } else {
        println!(
            "Состояние перенесено из {}: {}",
            source.display(),
            copied.join(", ")
        );
    }

    // 4. Снять пользовательский автозапуск 2.6d: та же метка, два задания в
    // одном домене launchd не загрузятся.
    let uid = sudo_uid();
    if let Some(uid) = &uid {
        let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
        if launchctl(&["bootout", &target])? {
            println!("Пользовательский автозапуск выгружен ({target})");
        } else {
            println!("Пользовательский автозапуск не был загружен ({target})");
        }
    }
    if let Some(user) = &user {
        let user_plist = user_home(user)
            .join("Library/LaunchAgents")
            .join(crate::fullmode::PLIST_FILE_NAME);
        if user_plist.exists() {
            fs::remove_file(&user_plist)
                .with_context(|| format!("удаление {}", user_plist.display()))?;
            println!("Пользовательский plist удалён: {}", user_plist.display());
        } else {
            println!("Пользовательского plist нет: {}", user_plist.display());
        }
    }

    // 5. Системный plist: root:wheel, 0644 (иначе launchd его не загрузит).
    let plist = plist_path();
    fs::write(&plist, generate_system_plist(agent))
        .with_context(|| format!("запись {}", plist.display()))?;
    chown(&plist, Some(0), Some(0))
        .with_context(|| format!("chown root:wheel {}", plist.display()))?;
    fs::set_permissions(&plist, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("chmod 644 {}", plist.display()))?;
    println!(
        "Системный plist записан: {} ({})",
        plist.display(),
        agent.display()
    );

    // 6. Загрузить задание в текущий GUI-сеанс. Из root голый bootstrap падает
    // (Input/output error), поэтому сначала `launchctl asuser` (см. doc
    // `load_job_attempts`); потом проверка фактом через `launchctl print`.
    match &uid {
        Some(uid) => {
            let domain = format!("gui/{uid}");
            let target = format!("{domain}/{LAUNCH_AGENT_LABEL}");
            let plist_str = plist.to_string_lossy().into_owned();
            let mut loaded = false;
            for attempt in load_job_attempts(LAUNCHCTL, uid, &plist_str) {
                let args: Vec<&str> = attempt.iter().map(String::as_str).collect();
                if launchctl(&args)? {
                    if args.contains(&"bootstrap") {
                        println!("Задание загружено в текущий сеанс ({domain})");
                    } else {
                        println!("Задание уже было загружено, перезапущено ({target})");
                    }
                    loaded = true;
                    break;
                }
            }
            if !loaded {
                bail!(
                    "системный plist {p} установлен и поднимется при следующем входе, \
                     но агент сейчас НЕ запущен (пользовательский автозапуск уже снят, \
                     поэтому удалённый доступ потерян до входа в систему). Поднять вручную \
                     от имени пользователя: launchctl bootstrap gui/{uid} {p}",
                    p = plist.display()
                );
            }
            let mut state_lines = Vec::new();
            for attempt in print_job_attempts(LAUNCHCTL, uid) {
                let args: Vec<&str> = attempt.iter().map(String::as_str).collect();
                if let Some(out) = launchctl_stdout(&args)? {
                    state_lines = job_state_lines(&out);
                    break;
                }
            }
            if state_lines.is_empty() {
                println!(
                    "Предупреждение: задание загружено, но подтвердить состояние не удалось. \
                     Проверьте сами от имени пользователя: launchctl print {target}"
                );
            }
            for line in state_lines {
                println!("  {line}");
            }
        }
        None => println!("SUDO_UID не задана: задание поднимется при следующем входе в систему"),
    }

    // 7. Что дальше.
    println!();
    println!("Полный режим установлен. Чтобы проверить: выйдите из системы и подключитесь");
    println!("с другого устройства по списку устройств аккаунта (PIN не нужен).");
    println!(
        "Лог роли «экран входа»: {}/loginwindow.log",
        log_dir.display()
    );
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    if euid() != 0 {
        bail!(
            "нужны права администратора: запустите команду через sudo \
             (sudo /Applications/rcdesk.app/Contents/MacOS/rcdesk-host fullmode uninstall)"
        );
    }
    if let Some(uid) = sudo_uid() {
        let target = format!("gui/{uid}/{LAUNCH_AGENT_LABEL}");
        if launchctl(&["bootout", &target])? {
            println!("Задание выгружено ({target})");
        } else {
            println!("Задание не было загружено ({target})");
        }
    }
    let plist = plist_path();
    match fs::remove_file(&plist) {
        Ok(()) => println!("Системный plist удалён: {}", plist.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("Системного plist уже нет: {}", plist.display());
        }
        Err(err) => return Err(err).with_context(|| format!("удаление {}", plist.display())),
    }
    println!("Данные сохранены: {}", paths::machine_data_dir().display());
    println!("Логи сохранены: {}", paths::machine_log_dir().display());
    println!("Вернуть лёгкий режим (автозапуск при входе): rcdesk-host autostart on, от имени пользователя.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_dir_ok_requires_root_owner_and_no_symlink() {
        assert!(machine_dir_ok(0, false));
        assert!(!machine_dir_ok(501, false));
        assert!(!machine_dir_ok(0, true));
        assert!(!machine_dir_ok(501, true));
    }

    #[test]
    fn euid_matches_role_mapping() {
        let expected = if euid() == 0 {
            Role::LoginScreen
        } else {
            Role::Session
        };
        assert_eq!(role_from_uid(euid()), expected);
    }

    #[test]
    fn console_uid_is_readable_on_macos() {
        assert!(console_uid().is_some());
    }
}
