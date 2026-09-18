//! Single-instance guard for `rcdesk-agent` (slice 2.6c): a lock file at
//! `paths::data_dir()/agent.lock`, held via `std::fs::File::try_lock`
//! (stable since Rust 1.89; this workspace builds on 1.98) for the whole
//! process lifetime. A second launch sees the lock already held and exits
//! instead of running two agents against the same signaling registration.
//!
//! Verified locally (macOS, this executor run) that a *second* `open()` of
//! the same path from *within the same process* still correctly reports
//! "busy": `try_lock` denies it (`TryLockError::WouldBlock`), it doesn't
//! silently succeed just because the first lock is held by the same
//! process. That's the exact scenario this guard exists to catch (the owner
//! double-clicking the agent), so it was worth confirming rather than
//! assuming.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Held for as long as the agent runs; dropping it (including on process
/// exit) releases the OS-level lock automatically.
pub struct SingleInstance(#[allow(dead_code)] File);

/// Attempts to acquire the lock at `dir/agent.lock`, creating `dir` and the
/// lock file if needed. `Ok(None)` means another instance already holds it
/// -- the caller should log and exit(0), not treat it as an error (see
/// `agent_main`).
pub fn acquire(dir: &Path) -> io::Result<Option<SingleInstance>> {
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        // The lock file's contents are irrelevant -- only its existence and
        // the OS lock on it matter -- so an existing file is left as-is
        // rather than truncated.
        .truncate(false)
        .open(dir.join("agent.lock"))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(SingleInstance(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(err)) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "rcdesk-agent-lock-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn a_second_acquire_in_the_same_process_reports_busy() {
        let dir = scratch_dir("busy");

        let first = acquire(&dir).unwrap();
        assert!(first.is_some(), "the first acquire must succeed");

        let second = acquire(&dir).unwrap();
        assert!(
            second.is_none(),
            "a second acquire while the first is still held must report busy"
        );

        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquiring_again_after_the_first_is_dropped_succeeds() {
        let dir = scratch_dir("released");

        let first = acquire(&dir).unwrap();
        assert!(first.is_some());
        drop(first);

        let second = acquire(&dir).unwrap();
        assert!(
            second.is_some(),
            "dropping the first guard must release the lock for the next acquire"
        );

        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
