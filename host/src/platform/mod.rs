#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod other;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub fn name() -> &'static str {
    macos::name()
}

#[cfg(target_os = "windows")]
pub fn name() -> &'static str {
    windows::name()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn name() -> &'static str {
    other::name()
}

/// This computer's user-visible name, used by `app::resolve_host_name` as
/// the preferred default for `serve --name` (ahead of `$HOSTNAME`) so a
/// host registers under something the owner actually recognizes -- see that
/// function's doc comment for the full fallback chain. `None` when the
/// platform has no such name, or it comes back empty.
#[cfg(target_os = "macos")]
pub fn computer_name() -> Option<String> {
    macos::computer_name()
}

#[cfg(target_os = "windows")]
pub fn computer_name() -> Option<String> {
    windows::computer_name()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn computer_name() -> Option<String> {
    other::computer_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_non_empty() {
        assert!(!name().is_empty());
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn name_matches_std_consts_os() {
        assert_eq!(name(), std::env::consts::OS);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn computer_name_returns_something_on_macos() {
        let name = computer_name().expect("macOS should always report a computer name");
        assert!(!name.is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn computer_name_returns_something_on_windows() {
        let name = computer_name().expect("COMPUTERNAME should be set on any real Windows box");
        assert!(!name.is_empty());
    }
}
