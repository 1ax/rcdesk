//! Probes whether the OS has granted the permissions a real session needs:
//! screen recording (macOS TCC) and input injection (macOS Accessibility).
//! Windows has no equivalent gate for either, so both are always `true`
//! there (see `ARCHITECTURE.md` §9/§7).

/// Snapshot of what a new session would be able to do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Whether screen capture is permitted (macOS: TCC "Screen Recording").
    pub screen: bool,
    /// Whether input injection is permitted (macOS: TCC "Accessibility").
    pub input: bool,
}

/// Probes current permission state. Cheap enough to call on a timer
/// (`agent_main` polls it every 10s, plus once at startup) -- both checks
/// below are the same ones a real session already performs, just run ahead
/// of time so the tray menu can warn before the owner tries to connect.
#[cfg(target_os = "macos")]
pub fn probe_permissions() -> Permissions {
    let screen = ::scap::has_permission();
    // The same check `app::build_real_injector` performs for a real
    // session (`EnigoInjector::new()` fails with `NoPermission` iff
    // Accessibility isn't granted) -- reusing it here means this probe can
    // never drift from what a session would actually see.
    let input = crate::input::enigo::EnigoInjector::new().is_ok();
    Permissions { screen, input }
}

/// Windows has no screen-recording/accessibility TCC equivalent -- both
/// capabilities are simply available (see `ARCHITECTURE.md` §9).
#[cfg(target_os = "windows")]
pub fn probe_permissions() -> Permissions {
    Permissions {
        screen: true,
        input: true,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn probe_permissions() -> Permissions {
    Permissions {
        screen: true,
        input: true,
    }
}
