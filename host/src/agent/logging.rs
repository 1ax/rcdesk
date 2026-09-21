//! File logging for `rcdesk-agent` (slice 2.6c). `rcdesk-host`'s CLI logs to
//! stderr (`main.rs`'s `tracing_subscriber::fmt().init()`), which works fine
//! from a terminal but is invisible for a background app launched without
//! one -- this writes to `agent.log` in `paths::log_dir()` instead, rotating
//! it once at startup if it's grown past `MAX_LOG_BYTES`, and installs a
//! panic hook so a panic lands in that same file instead of vanishing along
//! with the console nobody's watching.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Rotation threshold: past this size, `agent.log` is renamed to
/// `agent.log.1` (overwriting any previous one) before a fresh `agent.log`
/// is opened. Checked once, at startup only -- the agent doesn't rotate
/// mid-run.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Rotates `log_path` if it exists and is over `MAX_LOG_BYTES`, then
/// installs a `tracing_subscriber` writing to it (append mode) with the
/// same env-filter default (`RUST_LOG` or `crate::DEFAULT_LOG_FILTER`) the CLI uses, plus a panic
/// hook that logs the payload/location via `tracing::error!` before
/// chaining to whatever hook was previously installed. Returns `log_path`
/// unchanged, for the caller (`agent_main`) to show in the "Open log" menu
/// item and startup log line.
pub fn init(log_dir: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join("agent.log");
    rotate_if_too_large(&log_path)?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(crate::DEFAULT_LOG_FILTER)),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();

    install_panic_hook();

    Ok(log_path)
}

/// Renames `log_path` to `<log_path>.1` (overwriting any existing one) if it
/// exists and is larger than `MAX_LOG_BYTES`. A no-op (not an error) if
/// `log_path` doesn't exist yet -- the very first run on a machine.
fn rotate_if_too_large(log_path: &Path) -> io::Result<()> {
    let metadata = match std::fs::metadata(log_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if metadata.len() > MAX_LOG_BYTES {
        let rotated = log_path.with_extension("log.1");
        std::fs::rename(log_path, rotated)?;
    }
    Ok(())
}

/// Logs a panic's payload and location via `tracing::error!` before chaining
/// to the hook that was installed before this one (so a debugger/default
/// stderr message, if any, still happens too).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        tracing::error!(%location, %payload, "panic in rcdesk-agent");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, per-test directory under the system temp dir -- not
    /// `paths::log_dir()` (that's the real per-user path) and not `tempfile`
    /// (not an approved new dependency): a unique name is enough since
    /// nothing else touches it.
    fn scratch_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rcdesk-agent-logging-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn rotates_when_over_the_threshold() {
        let dir = scratch_dir("rotates");
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("agent.log");
        std::fs::write(&log_path, vec![0u8; (MAX_LOG_BYTES + 1) as usize]).unwrap();

        rotate_if_too_large(&log_path).unwrap();

        assert!(
            !log_path.exists(),
            "the oversized log should have been moved aside"
        );
        assert!(dir.join("agent.log.1").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_rotate_when_under_the_threshold() {
        let dir = scratch_dir("small");
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("agent.log");
        std::fs::write(&log_path, b"a small log").unwrap();

        rotate_if_too_large(&log_path).unwrap();

        assert!(log_path.exists());
        assert!(!dir.join("agent.log.1").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotating_overwrites_a_previous_dot_one() {
        let dir = scratch_dir("overwrite");
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("agent.log");
        std::fs::write(&log_path, vec![0u8; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        std::fs::write(dir.join("agent.log.1"), b"stale previous rotation").unwrap();

        rotate_if_too_large(&log_path).unwrap();

        let rotated = std::fs::metadata(dir.join("agent.log.1")).unwrap();
        assert_eq!(rotated.len(), MAX_LOG_BYTES + 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_a_no_op_when_the_log_does_not_exist_yet() {
        let dir = scratch_dir("missing");
        // Deliberately not creating `dir` at all.
        let log_path = dir.join("agent.log");

        rotate_if_too_large(&log_path).unwrap();

        assert!(!log_path.exists());
    }
}
