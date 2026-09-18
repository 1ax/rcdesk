pub mod autostart;
pub mod d3d;
pub mod dpi;
pub mod event_loop;
pub mod keyboard;
pub mod monitors;
pub mod mouse;
pub mod power;

pub(super) fn name() -> &'static str {
    "windows"
}

/// This PC's computer name (`%COMPUTERNAME%`, Settings -> System -> About).
/// `None` if the variable is unset or empty -- shouldn't happen on a real
/// Windows install, but `app::resolve_host_name` falls back further either
/// way.
pub(super) fn computer_name() -> Option<String> {
    let name = std::env::var("COMPUTERNAME").ok()?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}
