//! Cross-platform logic for `rcdesk-agent`, the tray/menu-bar frontend on
//! top of `crate::app::run_agent` (slice 2.6c). Nothing in this module calls
//! into AppKit/Win32 or the `tray-icon`/`muda` crates directly -- that's
//! `agent_main.rs`'s job, which is why everything here is plain, unit
//! testable Rust: menu text/enabled-state derivation (`menu`), the icon
//! pixels (`icon`), permission probing (`permissions`), per-OS paths
//! (`paths`), file logging with rotation and a panic hook (`logging`), the
//! single-instance lock (`lock`), and "start at login" path resolution plus
//! the pure half of each platform's mechanism (`autostart`, slice 2.6d).

pub mod autostart;
pub mod icon;
pub mod lock;
pub mod logging;
pub mod menu;
pub mod paths;
pub mod permissions;

pub use icon::{render_icon, IconState};
pub use menu::{menu_model, MenuModel};
pub use permissions::{probe_permissions, Permissions};
