pub(super) fn name() -> &'static str {
    "other"
}

/// No autostart mechanism on this platform -- only macOS/Windows ship
/// `rcdesk-agent` -- but `rcdesk-host autostart` (slice 2.6d) still needs to
/// build and run here since CI's `ubuntu-latest` job runs `cargo test
/// --workspace`/clippy over the whole workspace. It simply always reports
/// "unsupported".
pub mod autostart {
    use std::path::Path;

    pub fn is_enabled(_agent: &Path) -> anyhow::Result<bool> {
        anyhow::bail!("autostart is not supported on this platform")
    }

    pub fn enable(_agent: &Path) -> anyhow::Result<()> {
        anyhow::bail!("autostart is not supported on this platform")
    }

    pub fn disable() -> anyhow::Result<()> {
        anyhow::bail!("autostart is not supported on this platform")
    }
}

/// No platform-specific "computer name" API on this target; `$HOSTNAME` is
/// the best available stand-in. `None` if unset or empty --
/// `app::resolve_host_name` checks the same variable again as its own next
/// fallback, so this is only ever the deciding step when it's unset.
pub(super) fn computer_name() -> Option<String> {
    let name = std::env::var("HOSTNAME").ok()?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Самообновление не поддерживается (D50c): хосты только macOS/Windows, модуль
/// нужен, чтобы workspace собирался на Linux в CI.
pub mod update {
    use std::path::{Path, PathBuf};

    pub const ASSET_NAME: &str = "";

    pub fn curl_path() -> PathBuf {
        PathBuf::from("curl")
    }

    pub fn hide_console(_command: &mut tokio::process::Command) {}

    pub fn launch_installer(_installer: &Path, _log: &Path) -> anyhow::Result<()> {
        anyhow::bail!("обновление не поддерживается на этой ОС")
    }
}
