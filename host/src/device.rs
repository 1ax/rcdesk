//! Persistent device credentials (slice 3.1c): once the signaling server
//! issues this host a `DeviceCredentials` (its very first registration), the
//! host saves it to `device.json` next to its own data
//! (`agent::paths::data_dir()` -- `~/Library/Application Support/rcdesk` on
//! macOS, `%LOCALAPPDATA%\rcdesk` on Windows) and presents it on every later
//! `HostRegister`, so the server recognizes this device and hands back the
//! same `host_id` instead of minting a fresh one on every reconnect/restart.
//!
//! On Unix the file is written with mode `0600` (owner read/write only):
//! it's a bearer secret, and `data_dir()` isn't private the way `~/.ssh` is
//! by convention. On Windows there's no POSIX mode bit to set, but none is
//! needed either -- `%LOCALAPPDATA%` is already ACL'd by the OS to the
//! owning user only (no separate service account, no shared "Users" group
//! access), the same assumption `agent::logging`/`agent::lock` already rely
//! on for `agent.log`/`agent.lock` in the same directory tree.

use std::path::{Path, PathBuf};

use proto::signal::DeviceCredentials;

/// The `device.json` file in the agent's data directory. See this module's
/// doc comment for the full path on each platform.
pub struct DeviceStore {
    path: PathBuf,
}

impl DeviceStore {
    /// `dir/device.json`.
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("device.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved credentials, or `None` when there aren't any usable ones:
    /// the file doesn't exist, isn't readable, or isn't the JSON this store
    /// writes. A corrupt file is deliberately `None` plus a `tracing::warn!`
    /// rather than an `Err` -- the host must be able to just register afresh
    /// (see `app::run_agent`), not crash because of a damaged cache file.
    pub fn load(&self) -> Option<DeviceCredentials> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "failed to read device credentials file, registering as a new device"
                );
                return None;
            }
        };
        match serde_json::from_slice::<DeviceCredentials>(&bytes) {
            Ok(credentials) => Some(credentials),
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "device credentials file is not valid JSON, registering as a new device"
                );
                None
            }
        }
    }

    /// Writes `credentials` atomically: a temporary file next to `path`,
    /// then `rename` over it, so a crash or power loss mid-write never
    /// leaves a half-written `device.json` behind. Mode `0600` on Unix (see
    /// this module's doc comment); the parent directory is created if it
    /// doesn't exist yet, the same as `agent::logging::init`/`agent::lock::acquire`.
    pub fn save(&self, credentials: &DeviceCredentials) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut tmp_name = self.path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp_path = self.path.with_file_name(tmp_name);

        let json = serde_json::to_vec_pretty(credentials)?;

        // The mode is set when the file is *created*, not after it already
        // holds the secret: writing first and `set_permissions` after leaves
        // a window in which the temporary file is world-readable (umask),
        // and it holds exactly the bytes this mode is meant to protect.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            use std::io::Write as _;
            let mut file = options.open(&tmp_path)?;
            file.write_all(&json)?;
            file.sync_all()?;
        }

        std::fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// Removes the credentials file. Its absence is not an error -- there's
    /// nothing to forget on a device that never had (or already lost) saved
    /// credentials.
    pub fn forget(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, per-test directory under the system temp dir -- not
    /// `agent::paths::data_dir()` (that's the real per-user path) and not
    /// `tempfile` (not an approved new dependency): a unique name is enough
    /// since nothing else touches it. Mirrors `agent::logging`'s test helper.
    fn scratch_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rcdesk-host-device-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn save_then_load_round_trips_the_same_credentials() {
        let dir = scratch_dir("round-trip");
        let store = DeviceStore::new(&dir);
        let credentials = DeviceCredentials {
            device_id: "dev-123".to_string(),
            secret: "s3cret".to_string(),
        };

        store.save(&credentials).unwrap();
        assert_eq!(store.load(), Some(credentials));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_in_an_empty_directory_returns_none() {
        let dir = scratch_dir("empty");
        let store = DeviceStore::new(&dir);

        assert_eq!(store.load(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_with_garbage_instead_of_json_returns_none_and_keeps_the_file() {
        let dir = scratch_dir("garbage");
        std::fs::create_dir_all(&dir).unwrap();
        let store = DeviceStore::new(&dir);
        std::fs::write(store.path(), b"not json at all").unwrap();

        assert_eq!(store.load(), None);
        assert!(
            store.path().exists(),
            "a corrupt file must not be deleted by load()"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_removes_the_file_and_a_second_forget_is_not_an_error() {
        let dir = scratch_dir("forget");
        let store = DeviceStore::new(&dir);
        store
            .save(&DeviceCredentials {
                device_id: "dev-1".to_string(),
                secret: "secret".to_string(),
            })
            .unwrap();
        assert!(store.path().exists());

        store.forget().unwrap();
        assert!(!store.path().exists());

        store.forget().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_the_file_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("perms");
        let store = DeviceStore::new(&dir);
        store
            .save(&DeviceCredentials {
                device_id: "dev-1".to_string(),
                secret: "secret".to_string(),
            })
            .unwrap();

        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
