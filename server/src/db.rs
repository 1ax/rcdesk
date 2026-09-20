//! Persistent signaling state: devices, owners, and the ownership links
//! between them (slice 3.1). Unlike `registry.rs` (in-memory, per-process
//! session state) this survives restarts -- a device's identity and an
//! owner's list of paired devices must not evaporate when the signaling
//! server redeploys.
//!
//! Guarded by a plain `std::sync::Mutex<Connection>` rather than
//! `tokio::sync::Mutex` or `spawn_blocking`, for the same reason as
//! `registry.rs`: every query here is a single row lookup/write by primary
//! key, the expected load is "an owner and a small team" (not a
//! high-concurrency workload), and no critical section below holds the
//! lock across an `.await` -- so a std mutex is strictly cheaper with no
//! downside.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension};

/// Schema version this binary expects (`PRAGMA user_version`). Bump this
/// and extend `migrate` when the schema changes; `migrate` itself must stay
/// idempotent (safe to run against an already-migrated database).
const SCHEMA_VERSION: i64 = 1;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    pub device_id: String,
    pub secret_hash: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OwnedDevice {
    pub device_id: String,
    pub name: String,
    pub alias: Option<String>,
    pub added_at: i64,
    pub last_seen_at: i64,
}

impl Db {
    /// Opens (creating if needed) a database file at `path`, applying
    /// pragmas and the schema migration. Creates the parent directory if
    /// it doesn't exist yet, so callers can point at e.g. `/data/rcdesk.db`
    /// in a fresh volume.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        configure(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// An in-memory database, for tests: same pragmas and migration, no
    /// file on disk.
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn insert_device(
        &self,
        device_id: &str,
        secret_hash: &str,
        name: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO devices (device_id, secret_hash, name, created_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            (device_id, secret_hash, name, now),
        )?;
        Ok(())
    }

    pub fn device(&self, device_id: &str) -> anyhow::Result<Option<DeviceRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let row = conn
            .query_row(
                "SELECT device_id, secret_hash, name, created_at, last_seen_at
                 FROM devices WHERE device_id = ?1",
                [device_id],
                |row| {
                    Ok(DeviceRow {
                        device_id: row.get(0)?,
                        secret_hash: row.get(1)?,
                        name: row.get(2)?,
                        created_at: row.get(3)?,
                        last_seen_at: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Updates `name` and `last_seen_at` of an existing device.
    /// `created_at` is left untouched. Returns `false` if no device with
    /// `device_id` exists.
    pub fn touch_device(&self, device_id: &str, name: &str, now: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE devices SET name = ?1, last_seen_at = ?2 WHERE device_id = ?3",
            (name, now, device_id),
        )?;
        Ok(changed > 0)
    }

    pub fn insert_owner(&self, owner_id: &str, token_hash: &str, now: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO owners (owner_id, token_hash, created_at) VALUES (?1, ?2, ?3)",
            (owner_id, token_hash, now),
        )?;
        Ok(())
    }

    pub fn owner_by_token_hash(&self, token_hash: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let owner_id = conn
            .query_row(
                "SELECT owner_id FROM owners WHERE token_hash = ?1",
                [token_hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(owner_id)
    }

    /// Links an owner to a device. Calling this again for the same pair is
    /// not an error (`INSERT OR IGNORE`).
    pub fn link_device(&self, owner_id: &str, device_id: &str, now: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO ownership (owner_id, device_id, alias, added_at)
             VALUES (?1, ?2, NULL, ?3)",
            (owner_id, device_id, now),
        )?;
        Ok(())
    }

    /// Returns `false` if no such link existed.
    pub fn unlink_device(&self, owner_id: &str, device_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "DELETE FROM ownership WHERE owner_id = ?1 AND device_id = ?2",
            (owner_id, device_id),
        )?;
        Ok(changed > 0)
    }

    /// Returns `false` if no such link exists.
    pub fn set_alias(
        &self,
        owner_id: &str,
        device_id: &str,
        alias: Option<&str>,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE ownership SET alias = ?1 WHERE owner_id = ?2 AND device_id = ?3",
            (alias, owner_id, device_id),
        )?;
        Ok(changed > 0)
    }

    /// Devices belonging to `owner_id`, ordered by `added_at` then
    /// `device_id` (a deterministic order for tests and UI listings).
    pub fn devices_for_owner(&self, owner_id: &str) -> anyhow::Result<Vec<OwnedDevice>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT d.device_id, d.name, o.alias, o.added_at, d.last_seen_at
             FROM ownership o
             JOIN devices d ON d.device_id = o.device_id
             WHERE o.owner_id = ?1
             ORDER BY o.added_at, d.device_id",
        )?;
        let rows = stmt
            .query_map([owner_id], |row| {
                Ok(OwnedDevice {
                    device_id: row.get(0)?,
                    name: row.get(1)?,
                    alias: row.get(2)?,
                    added_at: row.get(3)?,
                    last_seen_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Applies pragmas that don't persist in the database file and must be set
/// on every connection (`foreign_keys`), plus ones that do persist but are
/// cheap to (re)set on open (`journal_mode`), then runs the schema
/// migration.
fn configure(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_row| Ok(()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(conn)?;
    Ok(())
}

/// Applies the schema migration if `user_version` is `0`, then sets it to
/// `SCHEMA_VERSION`. A no-op if the database is already at
/// `SCHEMA_VERSION`. Idempotent: safe to call again against an
/// already-migrated (and populated) database.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS devices (
            device_id    TEXT PRIMARY KEY,
            secret_hash  TEXT NOT NULL,
            name         TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS owners (
            owner_id   TEXT PRIMARY KEY,
            token_hash TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS ownership (
            owner_id  TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
            device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
            alias     TEXT,
            added_at  INTEGER NOT NULL,
            PRIMARY KEY (owner_id, device_id)
         );",
    )?;

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_has_user_version_one() {
        let db = Db::in_memory().expect("open in-memory db");
        let conn = db.conn.lock().expect("db mutex poisoned");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 1);
    }

    #[test]
    fn migration_is_idempotent_and_keeps_data() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_device("dev1", "hash", "My Mac", 100)
            .expect("insert device");

        {
            let conn = db.conn.lock().expect("db mutex poisoned");
            migrate(&conn).expect("re-run migration");
        }

        let row = db
            .device("dev1")
            .expect("query device")
            .expect("device still present");
        assert_eq!(row.name, "My Mac");
    }

    #[test]
    fn insert_device_and_device_round_trip_all_fields() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_device("dev1", "hash1", "My Mac", 100)
            .expect("insert device");

        let row = db
            .device("dev1")
            .expect("query device")
            .expect("device present");

        assert_eq!(
            row,
            DeviceRow {
                device_id: "dev1".to_string(),
                secret_hash: "hash1".to_string(),
                name: "My Mac".to_string(),
                created_at: 100,
                last_seen_at: 100,
            }
        );
    }

    #[test]
    fn device_for_unknown_id_is_none() {
        let db = Db::in_memory().expect("open in-memory db");
        assert_eq!(db.device("nope").expect("query device"), None);
    }

    #[test]
    fn touch_device_updates_name_and_last_seen_but_not_created_at() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_device("dev1", "hash1", "My Mac", 100)
            .expect("insert device");

        let touched = db
            .touch_device("dev1", "My Mac (renamed)", 200)
            .expect("touch device");
        assert!(touched);

        let row = db
            .device("dev1")
            .expect("query device")
            .expect("device present");
        assert_eq!(row.name, "My Mac (renamed)");
        assert_eq!(row.created_at, 100);
        assert_eq!(row.last_seen_at, 200);

        let touched_unknown = db
            .touch_device("nope", "x", 300)
            .expect("touch unknown device");
        assert!(!touched_unknown);
    }

    #[test]
    fn insert_owner_and_owner_by_token_hash_round_trip() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", "tokenhash1", 100)
            .expect("insert owner");

        assert_eq!(
            db.owner_by_token_hash("tokenhash1")
                .expect("query owner by token hash"),
            Some("owner1".to_string())
        );
        assert_eq!(
            db.owner_by_token_hash("unknown")
                .expect("query owner by token hash"),
            None
        );
    }

    #[test]
    fn link_device_twice_is_not_an_error_and_device_appears_once() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", "tokenhash1", 100)
            .expect("insert owner");
        db.insert_device("dev1", "hash1", "My Mac", 100)
            .expect("insert device");

        db.link_device("owner1", "dev1", 200).expect("link device");
        db.link_device("owner1", "dev1", 300)
            .expect("link device again");

        let devices = db.devices_for_owner("owner1").expect("devices for owner");
        assert_eq!(devices.len(), 1);
    }

    #[test]
    fn devices_for_owner_orders_by_added_at_with_default_alias_and_live_last_seen() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", "tokenhash1", 100)
            .expect("insert owner");
        db.insert_device("dev1", "hash1", "First", 100)
            .expect("insert device 1");
        db.insert_device("dev2", "hash2", "Second", 100)
            .expect("insert device 2");

        db.link_device("owner1", "dev2", 200).expect("link dev2");
        db.link_device("owner1", "dev1", 300).expect("link dev1");

        db.touch_device("dev1", "First", 999)
            .expect("touch dev1 to update last_seen_at");

        let devices = db.devices_for_owner("owner1").expect("devices for owner");

        assert_eq!(
            devices,
            vec![
                OwnedDevice {
                    device_id: "dev2".to_string(),
                    name: "Second".to_string(),
                    alias: None,
                    added_at: 200,
                    last_seen_at: 100,
                },
                OwnedDevice {
                    device_id: "dev1".to_string(),
                    name: "First".to_string(),
                    alias: None,
                    added_at: 300,
                    last_seen_at: 999,
                },
            ]
        );

        let no_devices = db
            .devices_for_owner("owner-without-links")
            .expect("devices for owner without links");
        assert_eq!(no_devices, Vec::new());
    }

    #[test]
    fn set_alias_changes_and_clears_alias() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", "tokenhash1", 100)
            .expect("insert owner");
        db.insert_device("dev1", "hash1", "My Mac", 100)
            .expect("insert device");
        db.link_device("owner1", "dev1", 200).expect("link device");

        assert!(db
            .set_alias("owner1", "dev1", Some("Work Mac"))
            .expect("set alias"));
        let devices = db.devices_for_owner("owner1").expect("devices for owner");
        assert_eq!(devices[0].alias, Some("Work Mac".to_string()));

        assert!(db.set_alias("owner1", "dev1", None).expect("clear alias"));
        let devices = db.devices_for_owner("owner1").expect("devices for owner");
        assert_eq!(devices[0].alias, None);

        assert!(!db
            .set_alias("owner1", "unlinked-device", Some("x"))
            .expect("set alias for unlinked device"));
    }

    #[test]
    fn unlink_device_removes_from_owner_list_but_keeps_device() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", "tokenhash1", 100)
            .expect("insert owner");
        db.insert_device("dev1", "hash1", "My Mac", 100)
            .expect("insert device");
        db.link_device("owner1", "dev1", 200).expect("link device");

        assert!(db.unlink_device("owner1", "dev1").expect("unlink device"));
        assert_eq!(
            db.devices_for_owner("owner1").expect("devices for owner"),
            Vec::new()
        );
        assert!(db.device("dev1").expect("query device").is_some());

        assert!(!db
            .unlink_device("owner1", "dev1")
            .expect("unlink again is false"));
    }

    #[test]
    fn foreign_keys_are_enforced_for_link_device() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", "tokenhash1", 100)
            .expect("insert owner");

        let result = db.link_device("owner1", "no-such-device", 200);
        assert!(result.is_err());
    }
}
