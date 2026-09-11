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
}
