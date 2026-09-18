pub(super) fn name() -> &'static str {
    "other"
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
