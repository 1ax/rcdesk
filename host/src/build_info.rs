//! Версия и номер сборки хоста: единое место для `--version`, логов старта и
//! меню агента. Номер сборки задаёт `build.rs` (`GITHUB_SHA` или `dev`).

/// Версия пакета (`[workspace.package] version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Короткий номер сборки: 7 символов `GITHUB_SHA` или `dev`.
pub const BUILD: &str = env!("RCDESK_BUILD");
/// Полная версия вида `0.1.0 (abc1234)`.
pub const FULL_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RCDESK_BUILD"), ")");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_version_has_version_and_build_in_parens() {
        assert!(FULL_VERSION.starts_with(VERSION));
        assert!(FULL_VERSION.contains(&format!("({BUILD})")));
        assert_eq!(FULL_VERSION, format!("{VERSION} ({BUILD})"));
    }
}
