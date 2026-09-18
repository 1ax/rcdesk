//! LaunchAgent-based "start at login" (slice 2.6d). Everything about the
//! plist's content lives in `crate::agent::autostart` (generation, escaping,
//! and parsing our own generated format back out) -- this file only turns
//! that into file IO under `~/Library/LaunchAgents/app.rcdesk.agent.plist`.
//!
//! `launchctl` is deliberately never called, from either `enable` or
//! `disable`: `bootstrap`-ing right after writing the plist would start a
//! second running instance alongside the one already handling the "Start at
//! login" menu click, and `bootout`-ing on disable would kill that same
//! process out from under itself if the click came from its own menu. The
//! written/removed file only takes effect on the *next* login -- see
//! docs/dev-run.md's `launchctl print` diagnosis note.

use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::autostart::{generate_plist, parse_program_arguments_path};

const PLIST_FILE_NAME: &str = "app.rcdesk.agent.plist";

fn launch_agents_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("Library/LaunchAgents")
}

/// Enabled means the plist exists *and* its `ProgramArguments[0]` matches
/// `agent` exactly -- a plist left over from a different install path counts
/// as disabled (see this slice's plan): `enable` below will happily
/// overwrite it with the current path.
pub fn is_enabled(agent: &Path) -> anyhow::Result<bool> {
    is_enabled_in(&launch_agents_dir(), agent)
}

pub fn enable(agent: &Path) -> anyhow::Result<()> {
    enable_in(&launch_agents_dir(), agent)
}

pub fn disable() -> anyhow::Result<()> {
    disable_in(&launch_agents_dir())
}

// The `_in` functions below take the LaunchAgents directory as a parameter
// so the tests can point them at a temp directory instead of the real
// `~/Library/LaunchAgents` -- see this slice's plan.

fn is_enabled_in(dir: &Path, agent: &Path) -> anyhow::Result<bool> {
    match fs::read_to_string(dir.join(PLIST_FILE_NAME)) {
        Ok(xml) => {
            let stored = parse_program_arguments_path(&xml);
            Ok(stored.as_deref() == Some(agent.to_string_lossy().as_ref()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn enable_in(dir: &Path, agent: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(PLIST_FILE_NAME), generate_plist(agent))?;
    Ok(())
}

fn disable_in(dir: &Path) -> anyhow::Result<()> {
    match fs::remove_file(dir.join(PLIST_FILE_NAME)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rcdesk-macos-autostart-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn disabled_when_no_plist_exists() {
        let dir = scratch_dir("no-plist");
        let agent = PathBuf::from("/Applications/rcdesk-agent");
        assert!(!is_enabled_in(&dir, &agent).unwrap());
    }

    #[test]
    fn enable_then_is_enabled_then_disable_round_trip() {
        let dir = scratch_dir("round-trip");
        let agent = PathBuf::from("/Applications/rcdesk-agent");

        enable_in(&dir, &agent).unwrap();
        assert!(dir.join(PLIST_FILE_NAME).exists());
        assert!(is_enabled_in(&dir, &agent).unwrap());

        disable_in(&dir).unwrap();
        assert!(!dir.join(PLIST_FILE_NAME).exists());
        assert!(!is_enabled_in(&dir, &agent).unwrap());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plist_for_a_different_path_reads_as_disabled() {
        let dir = scratch_dir("other-path");
        enable_in(&dir, Path::new("/Applications/rcdesk-agent")).unwrap();

        assert!(!is_enabled_in(&dir, Path::new("/opt/rcdesk/rcdesk-agent")).unwrap());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabling_when_nothing_is_enabled_is_not_an_error() {
        let dir = scratch_dir("disable-missing");
        disable_in(&dir).unwrap();
    }
}
