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
