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
const SCHEMA_VERSION: i64 = 3;

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

/// Один passkey владельца в списке (слайс 3.7b). Сам `passkey_json` и
/// идентификатор credential наружу не отдаются.
#[derive(Debug, Clone, PartialEq)]
pub struct PasskeyRow {
    pub passkey_id: String,
    pub user_agent: String,
    pub created_at: i64,
    pub last_used_at: i64,
}

/// Найденный по credential id passkey: `(passkey_id, owner_id, passkey_json)`.
pub type PasskeyLookup = (String, String, String);

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

    /// UUID пользователя WebAuthn этого владельца, если он уже заведён.
    pub fn webauthn_user_id(&self, owner_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found: Option<Option<String>> = conn
            .query_row(
                "SELECT webauthn_user_id FROM owners WHERE owner_id = ?1",
                [owner_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.flatten())
    }

    /// Назначает владельцу UUID пользователя WebAuthn, только если у него
    /// ещё нет своего (первая регистрация passkey). `false`, если владельца
    /// нет или UUID уже назначен.
    pub fn set_webauthn_user_id(&self, owner_id: &str, uuid: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE owners SET webauthn_user_id = ?1
             WHERE owner_id = ?2 AND webauthn_user_id IS NULL",
            (uuid, owner_id),
        )?;
        Ok(changed > 0)
    }

    /// Владелец по UUID пользователя WebAuthn.
    pub fn owner_by_webauthn_user_id(&self, uuid: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found = conn
            .query_row(
                "SELECT owner_id FROM owners WHERE webauthn_user_id = ?1",
                [uuid],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found)
    }

    /// Добавляет passkey владельца; `created_at = last_used_at = now`.
    pub fn insert_passkey(
        &self,
        passkey_id: &str,
        owner_id: &str,
        credential_id: &str,
        passkey_json: &str,
        user_agent: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO passkeys
                 (passkey_id, owner_id, credential_id, passkey_json,
                  user_agent, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            (
                passkey_id,
                owner_id,
                credential_id,
                passkey_json,
                user_agent,
                now,
            ),
        )?;
        Ok(())
    }

    /// Ищет passkey по base64url credential id.
    pub fn passkey_by_credential_id(
        &self,
        credential_id: &str,
    ) -> anyhow::Result<Option<PasskeyLookup>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found = conn
            .query_row(
                "SELECT passkey_id, owner_id, passkey_json FROM passkeys
                 WHERE credential_id = ?1",
                [credential_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(found)
    }

    /// Обновляет `last_used_at` passkey и, если передан, его `passkey_json`
    /// (после `update_credential`). `false`, если passkey нет.
    pub fn touch_passkey(
        &self,
        passkey_id: &str,
        passkey_json: Option<&str>,
        now: i64,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE passkeys
             SET passkey_json = COALESCE(?1, passkey_json), last_used_at = ?2
             WHERE passkey_id = ?3",
            (passkey_json, now, passkey_id),
        )?;
        Ok(changed > 0)
    }

    /// Passkey владельца (без JSON), самые недавно использованные первыми.
    pub fn passkeys_for_owner(&self, owner_id: &str) -> anyhow::Result<Vec<PasskeyRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT passkey_id, user_agent, created_at, last_used_at
             FROM passkeys
             WHERE owner_id = ?1
             ORDER BY last_used_at DESC, passkey_id",
        )?;
        let rows = stmt
            .query_map([owner_id], |row| {
                Ok(PasskeyRow {
                    passkey_id: row.get(0)?,
                    user_agent: row.get(1)?,
                    created_at: row.get(2)?,
                    last_used_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// base64url credential id всех passkey владельца (для
    /// `exclude_credentials` при регистрации).
    pub fn credential_ids_for_owner(&self, owner_id: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT credential_id FROM passkeys WHERE owner_id = ?1 ORDER BY passkey_id",
        )?;
        let rows = stmt
            .query_map([owner_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    /// Удаляет passkey владельца. `false`, если такого passkey у него нет.
    pub fn delete_passkey(&self, owner_id: &str, passkey_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "DELETE FROM passkeys WHERE owner_id = ?1 AND passkey_id = ?2",
            (owner_id, passkey_id),
        )?;
        Ok(changed > 0)
    }

    /// Сливает анонимного владельца `from` (браузер без аккаунта) в
    /// аккаунт `to`: привязки устройств переезжают к `to` (`alias` и
    /// `added_at` сохраняются, уже привязанные к `to` остаются как были),
    /// затем строка `from` удаляется целиком (его токены уходят каскадом).
    /// Одна транзакция. Ничего не делает и возвращает `false`, если
    /// `from == to` или у `from` есть passkey (он сам аккаунт).
    pub fn merge_anonymous_owner(&self, from: &str, to: &str) -> anyhow::Result<bool> {
        if from == to {
            return Ok(false);
        }
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        let has_passkey: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM passkeys WHERE owner_id = ?1)",
            [from],
            |row| row.get(0),
        )?;
        if has_passkey {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR IGNORE INTO ownership (owner_id, device_id, alias, added_at)
             SELECT ?2, device_id, alias, added_at FROM ownership WHERE owner_id = ?1",
            (from, to),
        )?;
        tx.execute("DELETE FROM ownership WHERE owner_id = ?1", [from])?;
        tx.execute("DELETE FROM owners WHERE owner_id = ?1", [from])?;
        tx.commit()?;
        Ok(true)
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

/// Полная схема v3.
const SCHEMA_V3: &str = "CREATE TABLE IF NOT EXISTS devices (
            device_id    TEXT PRIMARY KEY,
            secret_hash  TEXT NOT NULL,
            name         TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS owners (
            owner_id         TEXT PRIMARY KEY,
            created_at       INTEGER NOT NULL,
            webauthn_user_id TEXT
         );
         CREATE UNIQUE INDEX IF NOT EXISTS owners_webauthn_user_id
            ON owners(webauthn_user_id);
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
         );
         CREATE TABLE IF NOT EXISTS passkeys (
            passkey_id    TEXT PRIMARY KEY,
            owner_id      TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
            credential_id TEXT NOT NULL UNIQUE,
            passkey_json  TEXT NOT NULL,
            user_agent    TEXT NOT NULL DEFAULT '',
            created_at    INTEGER NOT NULL,
            last_used_at  INTEGER NOT NULL
         );";

/// Приводит базу к `SCHEMA_VERSION`: версия 0 — сразу полная схема v3,
/// версия 1 — цепочка v1 -> v2 (токен из `owners` переезжает в
/// `owner_tokens`) -> v3, версия 2 — v2 -> v3 (passkey). Идемпотентна: на
/// актуальной базе — no-op.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    if version == 0 {
        conn.execute_batch(SCHEMA_V3)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }

    if version == 1 {
        migrate_v1_to_v2(conn)?;
    }
    migrate_v2_to_v3(conn)
}

/// v2 -> v3 (слайс 3.7b): колонка `owners.webauthn_user_id` с уникальным
/// индексом (`UNIQUE` в `ADD COLUMN` SQLite не разрешает) и таблица
/// `passkeys`. Одна транзакция; существующие строки не трогаются.
fn migrate_v2_to_v3(conn: &Connection) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "ALTER TABLE owners ADD COLUMN webauthn_user_id TEXT;
         CREATE UNIQUE INDEX owners_webauthn_user_id ON owners(webauthn_user_id);
         CREATE TABLE passkeys (
            passkey_id    TEXT PRIMARY KEY,
            owner_id      TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
            credential_id TEXT NOT NULL UNIQUE,
            passkey_json  TEXT NOT NULL,
            user_agent    TEXT NOT NULL DEFAULT '',
            created_at    INTEGER NOT NULL,
            last_used_at  INTEGER NOT NULL
         );",
    )?;
    tx.pragma_update(None, "user_version", 3)?;
    tx.commit()?;
    Ok(())
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
        tx.pragma_update(None, "user_version", 2)?;
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
    fn fresh_db_has_user_version_three() {
        let db = Db::in_memory().expect("open in-memory db");
        let conn = db.conn.lock().expect("db mutex poisoned");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 3);
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
    fn migration_v1_to_v3_keeps_owners_ownership_devices_and_tokens() {
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

        migrate(&conn).expect("migrate v1 -> v3");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 3);
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

        // Новые таблицы/колонки v3 на месте и работают.
        assert_eq!(db.webauthn_user_id("owner1").expect("uuid"), None);
        assert!(db.set_webauthn_user_id("owner1", "u1").expect("set uuid"));
        db.insert_passkey("pk1", "owner1", "cred1", "{}", "UA", 70)
            .expect("insert passkey");
    }

    #[test]
    fn migration_v2_to_v3_keeps_owners_tokens_and_ownership() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        conn.execute_batch(
            "CREATE TABLE devices (
                device_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL, name TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);
             CREATE TABLE owners (
                owner_id TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
             CREATE TABLE owner_tokens (
                token_id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                token_hash TEXT NOT NULL UNIQUE, user_agent TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);
             CREATE TABLE ownership (
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
                alias TEXT, added_at INTEGER NOT NULL, PRIMARY KEY (owner_id, device_id));
             INSERT INTO devices VALUES ('dev1', 'sh', 'My Mac', 10, 20);
             INSERT INTO owners VALUES ('owner1', 50);
             INSERT INTO owner_tokens VALUES ('tok1', 'owner1', 'h1', 'UA', 50, 55);
             INSERT INTO ownership VALUES ('owner1', 'dev1', 'Work', 60);
             PRAGMA user_version = 2;",
        )
        .expect("build v2 database");

        migrate(&conn).expect("migrate v2 -> v3");
        migrate(&conn).expect("re-run migration is a no-op");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 3);

        let db = Db {
            conn: Arc::new(Mutex::new(conn)),
        };
        assert_eq!(
            db.token_by_hash("h1").expect("token"),
            Some(("owner1".to_string(), "tok1".to_string()))
        );
        let owned = db.devices_for_owner("owner1").expect("devices");
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].alias.as_deref(), Some("Work"));
        assert_eq!(db.webauthn_user_id("owner1").expect("uuid"), None);
    }

    #[test]
    fn webauthn_user_id_is_assigned_once_and_unique() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o1", 1).expect("owner");
        db.insert_owner("o2", 1).expect("owner");

        assert!(db.set_webauthn_user_id("o1", "u1").expect("set"));
        assert!(!db.set_webauthn_user_id("o1", "u2").expect("set again"));
        assert_eq!(
            db.webauthn_user_id("o1").expect("get"),
            Some("u1".to_string())
        );
        assert_eq!(
            db.owner_by_webauthn_user_id("u1").expect("by uuid"),
            Some("o1".to_string())
        );
        assert_eq!(db.owner_by_webauthn_user_id("zz").expect("by uuid"), None);
        assert!(db.set_webauthn_user_id("o2", "u1").is_err());
    }

    #[test]
    fn passkey_crud_is_scoped_to_owner() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o1", 1).expect("owner");
        db.insert_owner("o2", 1).expect("owner");
        db.insert_passkey("old", "o1", "c-old", "{\"a\":1}", "UA", 100)
            .expect("insert");
        db.insert_passkey("new", "o1", "c-new", "{}", "UA2", 200)
            .expect("insert");
        assert!(db
            .insert_passkey("dup", "o2", "c-new", "{}", "", 1)
            .is_err());

        assert_eq!(
            db.passkey_by_credential_id("c-old").expect("find"),
            Some(("old".to_string(), "o1".to_string(), "{\"a\":1}".to_string()))
        );
        assert_eq!(db.passkey_by_credential_id("nope").expect("find"), None);

        assert!(db
            .touch_passkey("old", Some("{\"a\":2}"), 300)
            .expect("touch"));
        assert!(db.touch_passkey("new", None, 150).expect("touch"));
        let found = db
            .passkey_by_credential_id("c-old")
            .expect("find")
            .expect("present");
        assert_eq!(found.2, "{\"a\":2}");
        assert!(!db.touch_passkey("nope", None, 1).expect("touch unknown"));

        let rows = db.passkeys_for_owner("o1").expect("list");
        let ids: Vec<_> = rows.iter().map(|r| r.passkey_id.as_str()).collect();
        assert_eq!(ids, vec!["old", "new"]);
        assert_eq!(rows[0].created_at, 100);
        assert_eq!(rows[0].last_used_at, 300);
        assert_eq!(
            db.credential_ids_for_owner("o1").expect("ids"),
            vec!["c-new".to_string(), "c-old".to_string()]
        );
        assert!(db.passkeys_for_owner("o2").expect("list").is_empty());

        assert!(!db.delete_passkey("o2", "old").expect("foreign delete"));
        assert!(db.delete_passkey("o1", "old").expect("delete"));
        assert!(!db.delete_passkey("o1", "old").expect("delete again"));
        assert_eq!(db.passkeys_for_owner("o1").expect("list").len(), 1);
    }

    #[test]
    fn merge_anonymous_owner_moves_devices_and_removes_the_owner() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        db.insert_token("a", "tok-a", "ha", "", 1).expect("token");
        for d in ["dev1", "dev2", "dev3"] {
            db.insert_device(d, "h", d, 1).expect("device");
        }
        db.link_device("a", "dev1", 10).expect("link");
        db.link_device("a", "dev2", 20).expect("link");
        db.set_alias("a", "dev1", Some("From A")).expect("alias");
        db.link_device("b", "dev2", 99).expect("link");
        db.set_alias("b", "dev2", Some("From B")).expect("alias");
        db.link_device("b", "dev3", 30).expect("link");

        assert!(db.merge_anonymous_owner("a", "b").expect("merge"));

        assert!(db.devices_for_owner("a").expect("a").is_empty());
        assert!(!db.token_exists("tok-a").expect("token cascaded"));
        assert_eq!(db.webauthn_user_id("a").expect("uuid"), None);
        let conn = db.conn.lock().expect("db mutex poisoned");
        let owners: i64 = conn
            .query_row(
                "SELECT count(*) FROM owners WHERE owner_id = 'a'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(owners, 0);
        drop(conn);
        let b = db.devices_for_owner("b").expect("b");
        let view: Vec<_> = b
            .iter()
            .map(|d| (d.device_id.as_str(), d.alias.as_deref(), d.added_at))
            .collect();
        assert_eq!(
            view,
            vec![
                ("dev1", Some("From A"), 10),
                ("dev3", None, 30),
                ("dev2", Some("From B"), 99),
            ]
        );
    }

    #[test]
    fn merge_anonymous_owner_leaves_accounts_and_same_owner_alone() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        db.insert_token("a", "tok-a", "ha", "", 1).expect("token");
        db.insert_device("dev1", "h", "d", 1).expect("device");
        db.link_device("a", "dev1", 10).expect("link");
        db.insert_passkey("pk", "a", "c", "{}", "", 1)
            .expect("passkey");

        assert!(!db.merge_anonymous_owner("a", "b").expect("account"));
        assert_eq!(db.devices_for_owner("a").expect("a").len(), 1);
        assert!(db.devices_for_owner("b").expect("b").is_empty());
        assert!(db.token_exists("tok-a").expect("token"));

        assert!(!db.merge_anonymous_owner("b", "b").expect("same"));
        assert!(!db.merge_anonymous_owner("a", "a").expect("same"));
        assert_eq!(db.devices_for_owner("a").expect("a").len(), 1);
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
