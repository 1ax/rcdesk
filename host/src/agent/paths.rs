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
}
