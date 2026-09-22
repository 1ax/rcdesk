#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod other;
#[cfg(target_os = "windows")]
pub mod windows;

/// `rcdesk-agent`'s minimal main-thread event loop (slice 2.6c) --
/// `platform::macos::event_loop`/`platform::windows::event_loop`
/// re-exported under one cfg-free path so `agent_main` doesn't need its own
/// per-OS `use`. `macos` itself stays a private module (see `mod macos`
/// above); this re-export is what makes `pump` (and, on macOS, `init`)
/// reachable from outside `platform` at all.
#[cfg(target_os = "macos")]
pub use macos::event_loop;
#[cfg(target_os = "windows")]
pub use windows::event_loop;

/// "Stay awake for this session" (slice 2.6c) -- `platform::macos::activity`
/// re-exported the same way `event_loop` is above, so `agent_main` can name
/// it as `platform::activity::SessionActivity` without reaching into the
/// private `macos` module itself. No Windows re-export here: the Windows
/// equivalent (`platform::windows::power`) is reachable directly, since
/// `windows` (unlike `macos`) is already a `pub mod`.
#[cfg(target_os = "macos")]
pub use macos::activity;

/// "Start at login" (slice 2.6d) -- `platform::{macos,windows,other}::autostart`
/// re-exported under one cfg-free path the same way `event_loop` is above,
/// so `agent_main`/`rcdesk-host`'s `autostart` CLI subcommand can call
/// `platform::autostart::{is_enabled,enable,disable}` without their own
/// per-OS `use`. The `other` (Linux, etc.) implementation always errors --
/// see its own doc comment -- rather than being absent, so the CLI
/// subcommand itself doesn't need cfg-gating either.
#[cfg(target_os = "macos")]
pub use macos::autostart;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use other::autostart;
#[cfg(target_os = "windows")]
pub use windows::autostart;

/// "Is the foreground window running elevated relative to us" (slice
/// 2.6e) -- re-exported the same way `event_loop` is above, so
/// `signaling::start_session`'s background watcher can name it as
/// `platform::elevation::foreground_input_blocked`. `windows` (unlike
/// `macos`) is already `pub mod`, so this re-export exists purely for the
/// same cfg-free-path consistency `event_loop`'s doc comment explains, not
/// because the module would otherwise be unreachable. No macOS/other
/// re-export: the check is meaningless off Windows (no UIPI), so `windows`
/// is the only platform module with an `elevation` submodule at all.
#[cfg(target_os = "windows")]
pub use windows::elevation;

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

/// This host's OS as reported to the client over the `control` channel
/// (`ControlMessage::HostInfo`, slice 3.5f) -- e.g. so the client can decide
/// whether Cmd should act as Ctrl for this session (Mac client, Windows
/// host only; see `web/src/keyRemap.ts`'s `cmdAsCtrlApplies`). Re-exported
/// the same cfg-free-path way `name()`/`computer_name()` above are, even
/// though (unlike those) its whole body already lives here rather than in a
/// per-platform submodule -- there's nothing platform-specific to call into,
/// just a different `proto::control::HostOs` value per `cfg`.
#[cfg(target_os = "macos")]
pub fn host_os() -> proto::control::HostOs {
    proto::control::HostOs::Macos
}

#[cfg(target_os = "windows")]
pub fn host_os() -> proto::control::HostOs {
    proto::control::HostOs::Windows
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn host_os() -> proto::control::HostOs {
    proto::control::HostOs::Other
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

    #[cfg(target_os = "macos")]
    #[test]
    fn host_os_is_macos_on_macos() {
        assert_eq!(host_os(), proto::control::HostOs::Macos);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn host_os_is_windows_on_windows() {
        assert_eq!(host_os(), proto::control::HostOs::Windows);
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn host_os_is_other_elsewhere() {
        assert_eq!(host_os(), proto::control::HostOs::Other);
    }
}
