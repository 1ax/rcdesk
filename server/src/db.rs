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
const SCHEMA_VERSION: i64 = 2;

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

/// Один токен браузера владельца (слайс 3.7a). Хеш наружу не отдаётся.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenRow {
    pub token_id: String,
    pub user_agent: String,
    pub created_at: i64,
    pub last_used_at: i64,
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

    pub fn insert_owner(&self, owner_id: &str, now: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO owners (owner_id, created_at) VALUES (?1, ?2)",
            (owner_id, now),
        )?;
        Ok(())
    }

    /// Добавляет владельцу токен браузера; `created_at = last_used_at = now`.
    pub fn insert_token(
        &self,
        owner_id: &str,
        token_id: &str,
        token_hash: &str,
        user_agent: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO owner_tokens
                 (token_id, owner_id, token_hash, user_agent, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            (token_id, owner_id, token_hash, user_agent, now),
        )?;
        Ok(())
    }

    /// Ищет токен по хешу: `(owner_id, token_id)`.
    pub fn token_by_hash(&self, token_hash: &str) -> anyhow::Result<Option<(String, String)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found = conn
            .query_row(
                "SELECT owner_id, token_id FROM owner_tokens WHERE token_hash = ?1",
                [token_hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(found)
    }

    /// Обновляет `last_used_at` токена и, если `user_agent` непустой,
    /// его `user_agent`. Возвращает `false`, если токена нет.
    pub fn touch_token(&self, token_id: &str, user_agent: &str, now: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE owner_tokens
             SET last_used_at = ?1,
                 user_agent = CASE WHEN ?2 = '' THEN user_agent ELSE ?2 END
             WHERE token_id = ?3",
            (now, user_agent, token_id),
        )?;
        Ok(changed > 0)
    }

    /// Токены владельца, самые недавно использованные первыми.
    pub fn tokens_for_owner(&self, owner_id: &str) -> anyhow::Result<Vec<TokenRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT token_id, user_agent, created_at, last_used_at
             FROM owner_tokens
             WHERE owner_id = ?1
             ORDER BY last_used_at DESC, token_id",
        )?;
        let rows = stmt
            .query_map([owner_id], |row| {
                Ok(TokenRow {
                    token_id: row.get(0)?,
                    user_agent: row.get(1)?,
                    created_at: row.get(2)?,
                    last_used_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Удаляет токен владельца. `false`, если такого токена у него нет.
    pub fn delete_token(&self, owner_id: &str, token_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "DELETE FROM owner_tokens WHERE owner_id = ?1 AND token_id = ?2",
            (owner_id, token_id),
        )?;
        Ok(changed > 0)
    }

    /// Жив ли ещё токен (не отозван).
    pub fn token_exists(&self, token_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found = conn
            .query_row(
                "SELECT 1 FROM owner_tokens WHERE token_id = ?1",
                [token_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
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

/// Полная схема v2.
const SCHEMA_V2: &str = "CREATE TABLE IF NOT EXISTS devices (
            device_id    TEXT PRIMARY KEY,
            secret_hash  TEXT NOT NULL,
            name         TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS owners (
            owner_id   TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS owner_tokens (
            token_id     TEXT PRIMARY KEY,
            owner_id     TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
            token_hash   TEXT NOT NULL UNIQUE,
            user_agent   TEXT NOT NULL DEFAULT '',
            created_at   INTEGER NOT NULL,
            last_used_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS ownership (
            owner_id  TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
            device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
            alias     TEXT,
            added_at  INTEGER NOT NULL,
            PRIMARY KEY (owner_id, device_id)
         );";

/// Приводит базу к `SCHEMA_VERSION`: версия 0 — сразу полная схема v2,
/// версия 1 — миграция v1 -> v2 (токен из `owners` переезжает в
/// `owner_tokens`). Идемпотентна: на актуальной базе — no-op.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    if version == 0 {
        conn.execute_batch(SCHEMA_V2)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }

    migrate_v1_to_v2(conn)
}

/// Процедура SQLite «12 шагов»: `DROP TABLE owners` при включённых
/// внешних ключах каскадно снёс бы `ownership`, поэтому `foreign_keys`
/// выключается ДО транзакции (внутри неё pragma — no-op) и включается
/// обратно после коммита.
fn migrate_v1_to_v2(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "foreign_keys", false)?;
    let fk_on: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if fk_on != 0 {
        anyhow::bail!("could not disable foreign_keys for migration");
    }

    let result = (|| -> anyhow::Result<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE owners_new (
                owner_id   TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
             );
             INSERT INTO owners_new (owner_id, created_at)
                 SELECT owner_id, created_at FROM owners;
             CREATE TABLE owner_tokens (
                token_id     TEXT PRIMARY KEY,
                owner_id     TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                token_hash   TEXT NOT NULL UNIQUE,
                user_agent   TEXT NOT NULL DEFAULT '',
                created_at   INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL
             );
             INSERT INTO owner_tokens
                    (token_id, owner_id, token_hash, user_agent, created_at, last_used_at)
                 SELECT lower(hex(randomblob(8))), owner_id, token_hash, '', created_at, created_at
                 FROM owners;
             DROP TABLE owners;
             ALTER TABLE owners_new RENAME TO owners;",
        )?;
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            anyhow::bail!("foreign_key_check found {violations} violations after migration");
        }
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    })();

    // Включаем обратно при любом исходе.
    conn.pragma_update(None, "foreign_keys", true)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_db_has_user_version_two() {
        let db = Db::in_memory().expect("open in-memory db");
        let conn = db.conn.lock().expect("db mutex poisoned");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 2);
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
    fn insert_token_and_token_by_hash_round_trip() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", 100).expect("insert owner");
        db.insert_token("owner1", "tok1", "tokenhash1", "UA", 100)
            .expect("insert token");

        assert_eq!(
            db.token_by_hash("tokenhash1").expect("query token"),
            Some(("owner1".to_string(), "tok1".to_string()))
        );
        assert_eq!(db.token_by_hash("unknown").expect("query token"), None);
        assert!(db.token_exists("tok1").expect("exists"));
        assert!(!db.token_exists("nope").expect("exists"));
    }

    #[test]
    fn touch_token_keeps_user_agent_when_new_one_is_empty() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", 100).expect("insert owner");
        db.insert_token("owner1", "tok1", "h1", "UA1", 100)
            .expect("insert token");

        assert!(db.touch_token("tok1", "", 200).expect("touch"));
        let rows = db.tokens_for_owner("owner1").expect("tokens");
        assert_eq!(rows[0].user_agent, "UA1");
        assert_eq!(rows[0].last_used_at, 200);

        assert!(db.touch_token("tok1", "UA2", 300).expect("touch"));
        let rows = db.tokens_for_owner("owner1").expect("tokens");
        assert_eq!(rows[0].user_agent, "UA2");
        assert_eq!(rows[0].created_at, 100);
        assert!(!db.touch_token("nope", "x", 1).expect("touch unknown"));
    }

    #[test]
    fn tokens_for_owner_orders_by_last_used_desc_and_delete_is_scoped_to_owner() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", 100).expect("insert owner");
        db.insert_owner("owner2", 100).expect("insert owner");
        db.insert_token("owner1", "old", "h1", "", 100)
            .expect("insert token");
        db.insert_token("owner1", "new", "h2", "", 200)
            .expect("insert token");

        let ids: Vec<_> = db
            .tokens_for_owner("owner1")
            .expect("tokens")
            .into_iter()
            .map(|t| t.token_id)
            .collect();
        assert_eq!(ids, vec!["new", "old"]);

        assert!(!db.delete_token("owner2", "old").expect("foreign delete"));
        assert!(db.token_exists("old").expect("exists"));
        assert!(db.delete_token("owner1", "old").expect("delete"));
        assert!(!db.token_exists("old").expect("exists"));
    }

    #[test]
    fn migration_v1_to_v2_keeps_owners_ownership_devices_and_tokens() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        conn.execute_batch(
            "CREATE TABLE devices (
                device_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL, name TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);
             CREATE TABLE owners (
                owner_id TEXT PRIMARY KEY, token_hash TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL);
             CREATE TABLE ownership (
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
                alias TEXT, added_at INTEGER NOT NULL, PRIMARY KEY (owner_id, device_id));
             INSERT INTO devices VALUES ('dev1', 'sh', 'My Mac', 10, 20);
             INSERT INTO owners VALUES ('owner1', 'oldhash', 50);
             INSERT INTO ownership VALUES ('owner1', 'dev1', 'Work', 60);
             PRAGMA user_version = 1;",
        )
        .expect("build v1 database");

        migrate(&conn).expect("migrate v1 -> v2");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 2);
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("fk");
        assert_eq!(fk, 1, "foreign_keys must be re-enabled");
        let violations: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .expect("fk check");
        assert_eq!(violations, 0);

        let db = Db {
            conn: Arc::new(Mutex::new(conn)),
        };
        let owned = db.devices_for_owner("owner1").expect("devices");
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].alias.as_deref(), Some("Work"));
        assert_eq!(owned[0].added_at, 60);
        assert!(db.device("dev1").expect("device").is_some());

        let (owner_id, token_id) = db
            .token_by_hash("oldhash")
            .expect("token")
            .expect("old token found");
        assert_eq!(owner_id, "owner1");
        assert_eq!(token_id.len(), 16);
        let tokens = db.tokens_for_owner("owner1").expect("tokens");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].created_at, 50);
        assert_eq!(tokens[0].last_used_at, 50);
        assert_eq!(tokens[0].user_agent, "");

        {
            let conn = db.conn.lock().expect("db mutex poisoned");
            migrate(&conn).expect("re-run migration is a no-op");
        }
        assert_eq!(db.tokens_for_owner("owner1").expect("tokens").len(), 1);
    }

    #[test]
    fn link_device_twice_is_not_an_error_and_device_appears_once() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("owner1", 100).expect("insert owner");
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
        db.insert_owner("owner1", 100).expect("insert owner");
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
        db.insert_owner("owner1", 100).expect("insert owner");
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
        db.insert_owner("owner1", 100).expect("insert owner");
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
        db.insert_owner("owner1", 100).expect("insert owner");

        let result = db.link_device("owner1", "no-such-device", 200);
        assert!(result.is_err());
    }
}
