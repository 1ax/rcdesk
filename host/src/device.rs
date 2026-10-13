//! Persistent device credentials (slice 3.1c): once the signaling server
//! issues this host a `DeviceCredentials` (its very first registration), the
//! host saves it to `device.json` next to its own data
//! (`agent::paths::data_dir()` -- `~/Library/Application Support/rcdesk` on
//! macOS, `%LOCALAPPDATA%\rcdesk` on Windows) and presents it on every later
//! `HostRegister`, so the server recognizes this device and hands back the
//! same `host_id` instead of minting a fresh one on every reconnect/restart.
//!
//! The file is per signaling server (D60): the production server
//! ([`DEFAULT_SERVER`]) keeps the historical `device.json`, any other server
//! gets `device-<slug>.json` (see [`DeviceStore::for_server`]). Reason: a
//! local `rcdesk-host serve --server ws://127.0.0.1:...` used to overwrite the
//! production credentials in the shared `device.json`, the production server
//! then answered `unknown device` and the agent re-registered as a new device.
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

/// The production signaling server. Its credentials live in the historical
/// `device.json`; other servers get their own file (see
/// [`DeviceStore::for_server`]).
pub const DEFAULT_SERVER: &str = "wss://rcdesk.app/ws";

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

    /// The store for credentials issued by `server`: `dir/device.json` for
    /// [`DEFAULT_SERVER`] (trailing `/` ignored), otherwise
    /// `dir/device-<slug>.json`, where the slug is the URL without its scheme
    /// with every char outside `[A-Za-z0-9.-]` replaced by `_` and trailing
    /// `_` trimmed (`ws://127.0.0.1:8100/ws` -> `device-127.0.0.1_8100_ws.json`).
    pub fn for_server(dir: &Path, server: &str) -> Self {
        let server = server.trim_end_matches('/');
        if server == DEFAULT_SERVER {
            return Self::new(dir);
        }
        let rest = ["wss://", "ws://", "https://", "http://"]
            .iter()
            .find_map(|scheme| server.strip_prefix(scheme))
            .unwrap_or(server);
        let slug: String = rest
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let slug = slug.trim_end_matches('_');
        Self {
            path: dir.join(format!("device-{slug}.json")),
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

    /// Writes `credentials` atomically with mode `0600` on Unix -- see this
    /// module's doc comment, and `fsutil::write_private_atomic` (slice
    /// 3.2b) for how.
    pub fn save(&self, credentials: &DeviceCredentials) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(credentials)?;
        crate::fsutil::write_private_atomic(&self.path, &json)?;
        Ok(())
    }

    /// Moves the credentials file aside to `<file name>.bak` next to it
    /// (overwriting an older `.bak`) instead of deleting it: if the server's
    /// `unknown device` was a mistake, the keys can be put back by hand. Its
    /// absence is not an error -- there's nothing to forget on a device that
    /// never had (or already lost) saved credentials.
    pub fn forget(&self) -> anyhow::Result<()> {
        let mut bak = self.path.clone().into_os_string();
        bak.push(".bak");
        match std::fs::rename(&self.path, PathBuf::from(bak)) {
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

    fn creds(id: &str) -> DeviceCredentials {
        DeviceCredentials {
            device_id: id.to_string(),
            secret: "secret".to_string(),
        }
    }

    #[test]
    fn for_server_default_uses_device_json_with_or_without_trailing_slash() {
        let dir = Path::new("/d");
        assert_eq!(
            DeviceStore::for_server(dir, DEFAULT_SERVER).path(),
            dir.join("device.json")
        );
        assert_eq!(
            DeviceStore::for_server(dir, "wss://rcdesk.app/ws/").path(),
            dir.join("device.json")
        );
    }

    #[test]
    fn for_server_other_server_gets_a_slugged_file() {
        let dir = Path::new("/d");
        assert_eq!(
            DeviceStore::for_server(dir, "ws://127.0.0.1:8100/ws").path(),
            dir.join("device-127.0.0.1_8100_ws.json")
        );
    }

    #[test]
    fn stores_for_different_servers_in_one_dir_do_not_interfere() {
        let dir = scratch_dir("two-servers");
        let prod = DeviceStore::for_server(&dir, DEFAULT_SERVER);
        let local = DeviceStore::for_server(&dir, "ws://127.0.0.1:8100/ws");

        prod.save(&creds("prod")).unwrap();
        assert_eq!(local.load(), None);
        local.save(&creds("local")).unwrap();
        assert_eq!(prod.load(), Some(creds("prod")));
        assert_eq!(local.load(), Some(creds("local")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_moves_the_file_to_bak_and_a_second_forget_is_not_an_error() {
        let dir = scratch_dir("forget");
        let store = DeviceStore::new(&dir);
        store.save(&creds("dev-1")).unwrap();
        let original = std::fs::read(store.path()).unwrap();

        store.forget().unwrap();
        assert!(!store.path().exists());
        assert_eq!(
            std::fs::read(dir.join("device.json.bak")).unwrap(),
            original
        );

        store.forget().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_overwrites_an_existing_bak() {
        let dir = scratch_dir("forget-overwrite");
        let store = DeviceStore::new(&dir);
        store.save(&creds("old")).unwrap();
        store.forget().unwrap();
        store.save(&creds("new")).unwrap();
        let newer = std::fs::read(store.path()).unwrap();

        store.forget().unwrap();
        assert_eq!(std::fs::read(dir.join("device.json.bak")).unwrap(), newer);

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
