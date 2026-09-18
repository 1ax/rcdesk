//! Pure derivation of "what should the tray menu show right now" from
//! `signaling::AgentStatus` and `permissions::Permissions` -- kept separate
//! from `agent_main`'s actual `tray_icon`/`muda` calls so the mapping is
//! unit-testable without an event loop or a real tray icon.

use crate::agent::icon::IconState;
use crate::agent::permissions::Permissions;
use crate::signaling::AgentStatus;

/// Everything `agent_main` needs to update the tray menu/icon/tooltip for
/// one status+permissions snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    /// The disabled "status" menu item's text, e.g. `"PIN 123 456"`.
    pub status_text: String,
    /// The tray icon's tooltip, `"rcdesk — <status_text>"`.
    pub tooltip: String,
    /// The raw (unspaced) PIN, when the agent has one right now -- for
    /// "Copy PIN"; `None` disables that menu item.
    pub pin: Option<String>,
    /// Whether "End session" should be enabled.
    pub can_end_session: bool,
    /// Which tray icon to show.
    pub icon: IconState,
    /// Permission warnings to show (macOS only in practice --
    /// `permissions::probe_permissions` always reports both `true` on
    /// Windows, so this is empty there). Each string is one menu item.
    pub warnings: Vec<String>,
}

/// Splits a 6-digit PIN into `"123 456"` for display; anything else
/// (shouldn't happen -- the server always mints 6 digits) passes through
/// unchanged rather than panicking.
fn format_pin(pin: &str) -> String {
    if pin.len() == 6 {
        format!("{} {}", &pin[..3], &pin[3..])
    } else {
        pin.to_string()
    }
}

/// Builds the menu model for one `status`/`perms` snapshot. See the fields
/// above for what each part feeds.
pub fn menu_model(status: &AgentStatus, perms: &Permissions) -> MenuModel {
    let (status_text, pin, can_end_session, icon) = match status {
        AgentStatus::Connecting => ("Connecting…".to_string(), None, false, IconState::Idle),
        AgentStatus::Registered { pin } => (
            format!("PIN {}", format_pin(pin)),
            Some(pin.clone()),
            false,
            IconState::Idle,
        ),
        AgentStatus::InSession { pin } => (
            format!("Session active · PIN {}", format_pin(pin)),
            Some(pin.clone()),
            true,
            IconState::Session,
        ),
        AgentStatus::Reconnecting { retry_in, .. } => (
            format!("Offline — retrying in {}s", retry_in.as_secs()),
            None,
            false,
            IconState::Offline,
        ),
    };

    let mut warnings = Vec::new();
    if !perms.screen {
        warnings.push("Screen Recording permission is required".to_string());
    }
    if !perms.input {
        warnings.push(
            "Accessibility permission is required for mouse and keyboard control".to_string(),
        );
    }

    MenuModel {
        tooltip: format!("rcdesk — {status_text}"),
        status_text,
        pin,
        can_end_session,
        icon,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const OK: Permissions = Permissions {
        screen: true,
        input: true,
    };

    #[test]
    fn connecting() {
        let model = menu_model(&AgentStatus::Connecting, &OK);
        assert_eq!(model.status_text, "Connecting…");
        assert_eq!(model.tooltip, "rcdesk — Connecting…");
        assert_eq!(model.pin, None);
        assert!(!model.can_end_session);
        assert_eq!(model.icon, IconState::Idle);
        assert!(model.warnings.is_empty());
    }

    #[test]
    fn registered_shows_a_spaced_pin_and_no_end_session() {
        let status = AgentStatus::Registered {
            pin: "123456".to_string(),
        };
        let model = menu_model(&status, &OK);
        assert_eq!(model.status_text, "PIN 123 456");
        assert_eq!(model.tooltip, "rcdesk — PIN 123 456");
        assert_eq!(model.pin.as_deref(), Some("123456"));
        assert!(!model.can_end_session);
        assert_eq!(model.icon, IconState::Idle);
    }

    #[test]
    fn in_session_enables_end_session_and_shows_the_session_icon() {
        let status = AgentStatus::InSession {
            pin: "123456".to_string(),
        };
        let model = menu_model(&status, &OK);
        assert_eq!(model.status_text, "Session active · PIN 123 456");
        assert_eq!(model.pin.as_deref(), Some("123456"));
        assert!(model.can_end_session);
        assert_eq!(model.icon, IconState::Session);
    }

    #[test]
    fn reconnecting_shows_retry_countdown_and_no_pin() {
        let status = AgentStatus::Reconnecting {
            error: "connection reset".to_string(),
            retry_in: Duration::from_secs(8),
        };
        let model = menu_model(&status, &OK);
        assert_eq!(model.status_text, "Offline — retrying in 8s");
        assert_eq!(model.pin, None);
        assert!(!model.can_end_session);
        assert_eq!(model.icon, IconState::Offline);
    }

    #[test]
    fn no_warnings_when_all_permissions_are_granted() {
        let model = menu_model(&AgentStatus::Connecting, &OK);
        assert!(model.warnings.is_empty());
    }

    #[test]
    fn warns_about_each_missing_permission() {
        let missing_screen = Permissions {
            screen: false,
            input: true,
        };
        let model = menu_model(&AgentStatus::Connecting, &missing_screen);
        assert_eq!(model.warnings.len(), 1);

        let missing_both = Permissions {
            screen: false,
            input: false,
        };
        let model = menu_model(&AgentStatus::Connecting, &missing_both);
        assert_eq!(model.warnings.len(), 2);
    }
}
