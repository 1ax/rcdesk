//! Per-user log/data directories for the tray agent (slice 2.6c),
//! `ARCHITECTURE.md` §4.1. Env-only (`HOME`/`LOCALAPPDATA`) -- no `dirs`
//! crate, no new dependency: these two variables are always set on a real
//! login session on either platform, which is the only place `rcdesk-agent`
//! runs.

use std::path::PathBuf;

/// `~/Library/Logs/rcdesk/` on macOS, `%LOCALAPPDATA%\rcdesk\logs\` on
/// Windows.
pub fn log_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home_dir().join("Library/Logs/rcdesk")
    }
    #[cfg(target_os = "windows")]
    {
        local_app_data().join("rcdesk").join("logs")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::temp_dir().join("rcdesk-logs")
    }
}

/// `~/Library/Application Support/rcdesk/` on macOS, `%LOCALAPPDATA%\rcdesk\`
/// on Windows.
pub fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home_dir().join("Library/Application Support/rcdesk")
    }
    #[cfg(target_os = "windows")]
    {
        local_app_data().join("rcdesk")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::temp_dir().join("rcdesk-data")
    }
}

/// Машинный каталог данных полного режима (слайс 4.1a):
/// `%ProgramData%\rcdesk\`. Берётся из переменной окружения `ProgramData`
/// (как `LOCALAPPDATA` выше, без Win32-вызова): она задана и у службы
/// SYSTEM, и у пользователя; запасной вариант -- `C:\ProgramData`.
/// Каталог создаётся и закрывается DACL командой `service install`.
#[cfg(target_os = "windows")]
pub fn machine_data_dir() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string()))
        .join("rcdesk")
}

/// Логи полного режима: `%ProgramData%\rcdesk\logs\`.
#[cfg(target_os = "windows")]
pub fn machine_log_dir() -> PathBuf {
    machine_data_dir().join("logs")
}

/// Машинный каталог данных полного режима macOS (D51, под-шаг 2):
/// `/Library/Application Support/rcdesk`. Общий для роли «сеанс» и роли
/// «экран входа» (root), иначе у них были бы разные `host_id`.
#[cfg(target_os = "macos")]
pub fn machine_data_dir() -> PathBuf {
    PathBuf::from(crate::fullmode::MACHINE_DATA_DIR)
}

/// Логи полного режима macOS: `/Library/Logs/rcdesk`.
#[cfg(target_os = "macos")]
pub fn machine_log_dir() -> PathBuf {
    PathBuf::from(crate::fullmode::MACHINE_LOG_DIR)
}

/// Каталог `device.json`/`access.json` на macOS: машинный, если установлен
/// полный режим (состояние общее для обеих ролей), иначе пользовательский.
#[cfg(target_os = "macos")]
pub fn state_dir() -> PathBuf {
    state_dir_in(crate::fullmode::is_installed())
}

/// Чистая часть [`state_dir`]: выбор каталога по признаку полного режима.
#[cfg(target_os = "macos")]
pub fn state_dir_in(full_mode: bool) -> PathBuf {
    if full_mode {
        machine_data_dir()
    } else {
        data_dir()
    }
}

#[cfg(target_os = "macos")]
fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

#[cfg(target_os = "windows")]
fn local_app_data() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_and_data_dir_differ() {
        assert_ne!(log_dir(), data_dir());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_paths_are_under_the_library_directory() {
        assert!(log_dir().ends_with("Library/Logs/rcdesk"));
        assert!(data_dir().ends_with("Library/Application Support/rcdesk"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_machine_paths_are_absolute_and_distinct() {
        assert!(machine_data_dir().is_absolute());
        assert!(machine_log_dir().is_absolute());
        assert_ne!(machine_data_dir(), machine_log_dir());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn state_dir_in_picks_machine_dir_only_in_full_mode() {
        assert_eq!(state_dir_in(true), machine_data_dir());
        assert_eq!(state_dir_in(false), data_dir());
    }
}
