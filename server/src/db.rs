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
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

/// Schema version this binary expects (`PRAGMA user_version`). Bump this
/// and extend `migrate` when the schema changes; `migrate` itself must stay
/// idempotent (safe to run against an already-migrated database).
const SCHEMA_VERSION: i64 = 6;

/// Сколько секунд владелец без устройств и passkey может не появляться,
/// прежде чем `prune_empty_owners` его удалит (долг D53).
pub const OWNER_IDLE_TTL_SECS: u64 = 30 * 24 * 3600;

/// Сколько секунд устройство без владельца может не регистрироваться,
/// прежде чем `prune_unowned_devices` его удалит (B2, аудит S-H2).
pub const DEVICE_IDLE_TTL_SECS: u64 = 30 * 24 * 3600;

/// Исход `Db::set_account_email`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetEmailOutcome {
    Updated,
    /// Почта принадлежит другому аккаунту.
    EmailTaken,
    /// У владельца нет аккаунта.
    NoAccount,
}

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

/// Сколько секунд хранится запись журнала сеансов (`prune_sessions`).
pub const SESSION_RETENTION_SECS: u64 = 365 * 24 * 3600;

/// Запись журнала сеансов (`sessions`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub session_id: String,
    pub device_id: String,
    pub owner_id: Option<String>,
    pub via: String,
    pub client_ua: String,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub ended_at: Option<i64>,
    pub end_reason: Option<String>,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub relay: bool,
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: row.get(0)?,
        device_id: row.get(1)?,
        owner_id: row.get(2)?,
        via: row.get(3)?,
        client_ua: row.get(4)?,
        started_at: row.get(5)?,
        last_seen_at: row.get(6)?,
        ended_at: row.get(7)?,
        end_reason: row.get(8)?,
        bytes_sent: row.get(9)?,
        bytes_received: row.get(10)?,
        relay: row.get::<_, i64>(11)? != 0,
    })
}

/// Владелец для админского кабинета (A3).
#[derive(Debug, Clone, PartialEq)]
pub struct AdminOwnerRow {
    pub owner_id: String,
    pub email: Option<String>,
    pub note: Option<String>,
    pub created_at: i64,
    /// max(created_at, последнее использование токена, passkey).
    pub last_active_at: i64,
    pub devices: i64,
    pub browsers: i64,
    pub passkeys: i64,
}

/// Устройство для админского кабинета (A3).
#[derive(Debug, Clone, PartialEq)]
pub struct AdminDeviceRow {
    pub device_id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub host_version: Option<String>,
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

    /// Переносит WAL в основной файл и обнуляет `-wal` (`wal_checkpoint(TRUNCATE)`).
    /// Зовётся при штатной остановке. Возвращает `(busy, log, checkpointed)`
    /// как есть: `busy != 0` значит, что чекпоинт не дошёл до конца (другой
    /// читатель/писатель держит БД) — логирует вызывающий.
    pub fn checkpoint(&self) -> anyhow::Result<(i64, i64, i64)> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let row = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(row)
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

    /// Записывает версию хоста (из `Hello`) в `devices.host_version`.
    /// Возвращает `false`, если устройства нет.
    pub fn set_host_version(&self, device_id: &str, version: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE devices SET host_version = ?1 WHERE device_id = ?2",
            (version, device_id),
        )?;
        Ok(changed > 0)
    }

    /// Слайс W: заменяет набор сетевых карт устройства целиком (одна
    /// транзакция).
    pub fn set_device_ifaces(
        &self,
        device_id: &str,
        ifaces: &[proto::signal::NetIface],
    ) -> anyhow::Result<()> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM device_ifaces WHERE device_id = ?1",
            [device_id],
        )?;
        for iface in ifaces {
            tx.execute(
                "INSERT INTO device_ifaces (device_id, mac, ipv4, prefix) VALUES (?1, ?2, ?3, ?4)",
                (device_id, &iface.mac, &iface.ipv4, iface.prefix),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Слайс W: сохранённые сетевые карты устройства.
    pub fn device_ifaces(&self, device_id: &str) -> anyhow::Result<Vec<proto::signal::NetIface>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT mac, ipv4, prefix FROM device_ifaces WHERE device_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map([device_id], |row| {
                Ok(proto::signal::NetIface {
                    mac: row.get(0)?,
                    ipv4: row.get(1)?,
                    prefix: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Открывает запись журнала сеансов. `via` — `'pin'` или `'device'`.
    /// Повторная вставка того же `session_id` игнорируется.
    pub fn insert_session(
        &self,
        session_id: &str,
        device_id: &str,
        owner_id: Option<&str>,
        via: &str,
        client_ua: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO sessions
                (session_id, device_id, owner_id, via, client_ua, started_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            (session_id, device_id, owner_id, via, client_ua, now),
        )?;
        Ok(())
    }

    /// Запись журнала сеансов по `session_id`.
    pub fn session(&self, session_id: &str) -> anyhow::Result<Option<SessionRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let row = conn
            .query_row(
                "SELECT session_id, device_id, owner_id, via, client_ua, started_at,
                        last_seen_at, ended_at, end_reason, bytes_sent, bytes_received, relay
                 FROM sessions WHERE session_id = ?1",
                [session_id],
                session_from_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Версия хоста из `devices.host_version`.
    pub fn host_version(&self, device_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let v = conn
            .query_row(
                "SELECT host_version FROM devices WHERE device_id = ?1",
                [device_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    }

    /// Обновляет статистику сеанса от хоста `device_id` (чужие строки не
    /// трогаются). Значения кумулятивные: байты = max(старое, новое),
    /// `relay` = старое OR новое. `final_report` закрывает ещё не закрытый
    /// сеанс (`end_reason = 'host'`). Возвращает `false`, если такой строки
    /// нет.
    #[allow(clippy::too_many_arguments)]
    pub fn record_session_stats(
        &self,
        session_id: &str,
        device_id: &str,
        bytes_sent: u64,
        bytes_received: u64,
        relay: bool,
        final_report: bool,
        now: i64,
    ) -> anyhow::Result<bool> {
        // SQLite хранит INTEGER как i64.
        let sent = i64::try_from(bytes_sent).unwrap_or(i64::MAX);
        let received = i64::try_from(bytes_received).unwrap_or(i64::MAX);
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET
                bytes_sent = MAX(bytes_sent, ?3),
                bytes_received = MAX(bytes_received, ?4),
                relay = MAX(relay, ?5),
                last_seen_at = ?7,
                ended_at = CASE WHEN ?6 AND ended_at IS NULL THEN ?7 ELSE ended_at END,
                end_reason = CASE WHEN ?6 AND ended_at IS NULL THEN 'host' ELSE end_reason END
             WHERE session_id = ?1 AND device_id = ?2",
            (
                session_id,
                device_id,
                sent,
                received,
                relay as i64,
                final_report,
                now,
            ),
        )?;
        Ok(changed > 0)
    }

    /// Завершает сеанс в журнале, только если он ещё не завершён.
    /// `last_seen_at` обновляется всегда. Возвращает `false`, если строки нет.
    pub fn end_session(&self, session_id: &str, reason: &str, now: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET
                last_seen_at = ?3,
                ended_at = COALESCE(ended_at, ?3),
                end_reason = CASE WHEN ended_at IS NULL THEN ?2 ELSE end_reason END
             WHERE session_id = ?1",
            (session_id, reason, now),
        )?;
        Ok(changed > 0)
    }

    /// Удаляет записи журнала с `started_at` старше `now - max_age_secs`.
    pub fn prune_sessions(&self, now: u64, max_age_secs: u64) -> anyhow::Result<usize> {
        let cutoff = now.saturating_sub(max_age_secs) as i64;
        let conn = self.conn.lock().expect("db mutex poisoned");
        let removed = conn.execute("DELETE FROM sessions WHERE started_at < ?1", [cutoff])?;
        Ok(removed)
    }

    /// Все владельцы со счётчиками для админского кабинета (A3).
    pub fn admin_owner_rows(&self) -> anyhow::Result<Vec<AdminOwnerRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT o.owner_id, a.email, o.admin_note, o.created_at,
                    MAX(o.created_at,
                        COALESCE((SELECT MAX(last_used_at) FROM owner_tokens t
                                  WHERE t.owner_id = o.owner_id), 0),
                        COALESCE((SELECT MAX(last_used_at) FROM passkeys p
                                  WHERE p.owner_id = o.owner_id), 0)),
                    (SELECT COUNT(*) FROM ownership w WHERE w.owner_id = o.owner_id),
                    (SELECT COUNT(*) FROM owner_tokens t WHERE t.owner_id = o.owner_id),
                    (SELECT COUNT(*) FROM passkeys p WHERE p.owner_id = o.owner_id)
             FROM owners o LEFT JOIN accounts a ON a.owner_id = o.owner_id
             ORDER BY o.owner_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AdminOwnerRow {
                    owner_id: row.get(0)?,
                    email: row.get(1)?,
                    note: row.get(2)?,
                    created_at: row.get(3)?,
                    last_active_at: row.get(4)?,
                    devices: row.get(5)?,
                    browsers: row.get(6)?,
                    passkeys: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Все устройства для админского кабинета (A3).
    pub fn admin_device_rows(&self) -> anyhow::Result<Vec<AdminDeviceRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT device_id, name, created_at, last_seen_at, host_version
             FROM devices ORDER BY device_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AdminDeviceRow {
                    device_id: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    last_seen_at: row.get(3)?,
                    host_version: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Пары `(device_id, owner_id)` из `ownership` (A3).
    pub fn admin_ownership(&self) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare("SELECT device_id, owner_id FROM ownership")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Весь журнал сеансов, свежие первыми (A3).
    pub fn all_sessions(&self) -> anyhow::Result<Vec<SessionRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT session_id, device_id, owner_id, via, client_ua, started_at,
                    last_seen_at, ended_at, end_reason, bytes_sent, bytes_received, relay
             FROM sessions ORDER BY started_at DESC, session_id",
        )?;
        let rows = stmt
            .query_map([], session_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Пишет заметку администратора о владельце (`None` снимает её).
    /// `false`, если владельца нет.
    pub fn set_admin_note(&self, owner_id: &str, note: Option<&str>) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE owners SET admin_note = ?2 WHERE owner_id = ?1",
            (owner_id, note),
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

    /// Заводит владельцу учётную запись «почта + пароль» (слайс 3.8a).
    /// `email` уже нормализована. Возвращает `false`, если почта занята или у
    /// владельца уже есть аккаунт (нарушение уникальности), иначе `true`.
    pub fn insert_account(
        &self,
        owner_id: &str,
        email: &str,
        password_hash: &str,
        now: i64,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let result = conn.execute(
            "INSERT INTO accounts
                 (owner_id, email, password_hash, created_at, password_changed_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            (owner_id, email, password_hash, now),
        );
        match result {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(false)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Ищет аккаунт по нормализованной почте: `(owner_id, password_hash)`.
    pub fn account_by_email(&self, email: &str) -> anyhow::Result<Option<(String, String)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found = conn
            .query_row(
                "SELECT owner_id, password_hash FROM accounts WHERE email = ?1",
                [email],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(found)
    }

    /// Почта и хеш пароля аккаунта владельца: `(email, password_hash)`.
    pub fn account_for_owner(&self, owner_id: &str) -> anyhow::Result<Option<(String, String)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found = conn
            .query_row(
                "SELECT email, password_hash FROM accounts WHERE owner_id = ?1",
                [owner_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(found)
    }

    /// Меняет почту аккаунта владельца. `email` уже нормализована.
    pub fn set_account_email(
        &self,
        owner_id: &str,
        email: &str,
    ) -> anyhow::Result<SetEmailOutcome> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let result = conn.execute(
            "UPDATE accounts SET email = ?1 WHERE owner_id = ?2",
            (email, owner_id),
        );
        match result {
            Ok(0) => Ok(SetEmailOutcome::NoAccount),
            Ok(_) => Ok(SetEmailOutcome::Updated),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(SetEmailOutcome::EmailTaken)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Почта аккаунта владельца, если он завёл учётную запись.
    pub fn email_for_owner(&self, owner_id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found = conn
            .query_row(
                "SELECT email FROM accounts WHERE owner_id = ?1",
                [owner_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found)
    }

    /// Меняет хеш пароля аккаунта и `password_changed_at`. `false`, если у
    /// владельца нет аккаунта.
    pub fn set_password_hash(
        &self,
        owner_id: &str,
        password_hash: &str,
        now: i64,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let changed = conn.execute(
            "UPDATE accounts SET password_hash = ?1, password_changed_at = ?2
             WHERE owner_id = ?3",
            (password_hash, now, owner_id),
        )?;
        Ok(changed > 0)
    }

    /// Удаляет ВСЕ токены браузеров владельца (сброс пароля выкидывает все
    /// сессии). Возвращает число удалённых токенов.
    pub fn delete_all_tokens(&self, owner_id: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let removed = conn.execute("DELETE FROM owner_tokens WHERE owner_id = ?1", [owner_id])?;
        Ok(removed)
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
            "SELECT EXISTS(SELECT 1 FROM passkeys WHERE owner_id = ?1)
                 OR EXISTS(SELECT 1 FROM accounts WHERE owner_id = ?1)",
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
        tx.execute(
            "UPDATE sessions SET owner_id = ?2 WHERE owner_id = ?1",
            (from, to),
        )?;
        tx.execute("DELETE FROM owners WHERE owner_id = ?1", [from])?;
        tx.commit()?;
        Ok(true)
    }

    /// Удаляет пустых анонимных владельцев (долг D53): без привязанных
    /// устройств (`ownership`), без passkey и без учётной записи
    /// (`accounts`) и с последней активностью
    /// (`MAX(owner_tokens.last_used_at)`, а без токенов -- `owners.created_at`)
    /// старше `now - max_idle_secs`. Владелец с `webauthn_user_id`, но без
    /// passkey -- тоже кандидат (регистрация не завершена). Токены уходят
    /// каскадом (`foreign_keys = ON` на каждом соединении). Один SQL,
    /// возвращает число удалённых владельцев.
    pub fn prune_empty_owners(&self, now: u64, max_idle_secs: u64) -> anyhow::Result<usize> {
        let cutoff = now.saturating_sub(max_idle_secs) as i64;
        let conn = self.conn.lock().expect("db mutex poisoned");
        let removed = conn.execute(
            "DELETE FROM owners
             WHERE NOT EXISTS (SELECT 1 FROM ownership WHERE ownership.owner_id = owners.owner_id)
               AND NOT EXISTS (SELECT 1 FROM passkeys WHERE passkeys.owner_id = owners.owner_id)
               AND NOT EXISTS (SELECT 1 FROM accounts WHERE accounts.owner_id = owners.owner_id)
               AND COALESCE(
                       (SELECT MAX(last_used_at) FROM owner_tokens
                        WHERE owner_tokens.owner_id = owners.owner_id),
                       owners.created_at
                   ) < ?1",
            [cutoff],
        )?;
        Ok(removed)
    }

    /// Удаляет устройства без владельца (ни одной строки `ownership`) с
    /// `last_seen_at` старше `now - max_idle_secs` (B2, аудит S-H2). Id из
    /// `keep` (сейчас подключённые хосты) не трогаются: их `last_seen_at`
    /// обновляется только при регистрации. Кандидаты выбираются и удаляются
    /// в одной транзакции; возвращает число удалённых устройств.
    pub fn prune_unowned_devices(
        &self,
        now: u64,
        max_idle_secs: u64,
        keep: &[String],
    ) -> anyhow::Result<usize> {
        let cutoff = now.saturating_sub(max_idle_secs) as i64;
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        let candidates: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT device_id FROM devices
                 WHERE NOT EXISTS (SELECT 1 FROM ownership WHERE ownership.device_id = devices.device_id)
                   AND last_seen_at < ?1",
            )?;
            let rows = stmt.query_map([cutoff], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut removed = 0;
        for device_id in candidates.iter().filter(|id| !keep.contains(id)) {
            removed += tx.execute("DELETE FROM devices WHERE device_id = ?1", [device_id])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Проверка живости БД для `/healthz`: мьютекс не отравлен, запрос идёт.
    pub fn ping(&self) -> anyhow::Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
        Ok(())
    }

    /// Отравляет мьютекс соединения (поток берёт lock и паникует).
    #[cfg(test)]
    pub fn poison_for_test(&self) {
        let conn = Arc::clone(&self.conn);
        let _ = std::thread::spawn(move || {
            let _guard = conn.lock().expect("lock");
            panic!("poison db mutex for test");
        })
        .join();
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

    /// D34: все владельцы, к которым привязано устройство.
    pub fn owners_of_device(&self, device_id: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt =
            conn.prepare("SELECT owner_id FROM ownership WHERE device_id = ?1 ORDER BY owner_id")?;
        let rows = stmt
            .query_map([device_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
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
/// migration. `busy_timeout` makes a concurrent external writer (e.g. an
/// open `sqlite3` shell) wait up to 2 s instead of failing at once;
/// `synchronous = NORMAL` is safe in WAL (only the last transactions can be
/// lost on power failure, integrity is kept) and avoids an fsync per commit
/// under the std mutex on tokio workers.
fn configure(conn: &Connection) -> anyhow::Result<()> {
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_row| Ok(()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(conn)?;
    Ok(())
}

/// Полная схема v6.
const SCHEMA_V6: &str = "CREATE TABLE IF NOT EXISTS devices (
            device_id    TEXT PRIMARY KEY,
            secret_hash  TEXT NOT NULL,
            name         TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            host_version TEXT
         );
         CREATE TABLE IF NOT EXISTS owners (
            owner_id         TEXT PRIMARY KEY,
            created_at       INTEGER NOT NULL,
            webauthn_user_id TEXT,
            admin_note       TEXT
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
         );
         CREATE TABLE IF NOT EXISTS accounts (
            owner_id            TEXT PRIMARY KEY REFERENCES owners(owner_id) ON DELETE CASCADE,
            email               TEXT NOT NULL UNIQUE,
            password_hash       TEXT NOT NULL,
            created_at          INTEGER NOT NULL,
            password_changed_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sessions (
            session_id     TEXT PRIMARY KEY,
            device_id      TEXT NOT NULL,
            owner_id       TEXT,
            via            TEXT NOT NULL,
            client_ua      TEXT NOT NULL DEFAULT '',
            started_at     INTEGER NOT NULL,
            last_seen_at   INTEGER NOT NULL,
            ended_at       INTEGER,
            end_reason     TEXT,
            bytes_sent     INTEGER NOT NULL DEFAULT 0,
            bytes_received INTEGER NOT NULL DEFAULT 0,
            relay          INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS sessions_started_at ON sessions(started_at);
         CREATE INDEX IF NOT EXISTS sessions_owner_id ON sessions(owner_id);
         CREATE INDEX IF NOT EXISTS sessions_device_id ON sessions(device_id);
         CREATE TABLE IF NOT EXISTS device_ifaces (
            device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
            mac       TEXT NOT NULL,
            ipv4      TEXT NOT NULL,
            prefix    INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS device_ifaces_device_id ON device_ifaces(device_id);";

/// Приводит базу к `SCHEMA_VERSION`: версия 0 — сразу полная схема v6,
/// версия 1 — цепочка v1 -> v2 (токен из `owners` переезжает в
/// `owner_tokens`) -> v3 -> v4, версия 2 — v2 -> v3 (passkey) -> v4,
/// версия 3 — v3 -> v4 (учётные записи «почта + пароль»), версия 4 —
/// v4 -> v5 (журнал сеансов), версия 5 — v5 -> v6 (сетевые карты
/// устройств для Wake-on-LAN). Идемпотентна: на актуальной базе — no-op.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    if version == 0 {
        conn.execute_batch(SCHEMA_V6)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }

    if version == 1 {
        migrate_v1_to_v2(conn)?;
    }
    if version <= 2 {
        migrate_v2_to_v3(conn)?;
    }
    if version <= 3 {
        migrate_v3_to_v4(conn)?;
    }
    if version <= 4 {
        migrate_v4_to_v5(conn)?;
    }
    migrate_v5_to_v6(conn)
}

/// v5 -> v6 (слайс W): таблица `device_ifaces` (сетевые карты хоста для
/// Wake-on-LAN), каскадно чистится вместе с устройством. Одна транзакция.
fn migrate_v5_to_v6(conn: &Connection) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE device_ifaces (
            device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
            mac       TEXT NOT NULL,
            ipv4      TEXT NOT NULL,
            prefix    INTEGER NOT NULL
         );
         CREATE INDEX device_ifaces_device_id ON device_ifaces(device_id);",
    )?;
    tx.pragma_update(None, "user_version", 6)?;
    tx.commit()?;
    Ok(())
}

/// v4 -> v5 (админский кабинет): `devices.host_version`, `owners.admin_note`
/// и таблица `sessions` (журнал сеансов, без внешних ключей — переживает
/// уборку владельцев и устройств). Одна транзакция.
fn migrate_v4_to_v5(conn: &Connection) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "ALTER TABLE devices ADD COLUMN host_version TEXT;
         ALTER TABLE owners ADD COLUMN admin_note TEXT;
         CREATE TABLE sessions (
            session_id     TEXT PRIMARY KEY,
            device_id      TEXT NOT NULL,
            owner_id       TEXT,
            via            TEXT NOT NULL,
            client_ua      TEXT NOT NULL DEFAULT '',
            started_at     INTEGER NOT NULL,
            last_seen_at   INTEGER NOT NULL,
            ended_at       INTEGER,
            end_reason     TEXT,
            bytes_sent     INTEGER NOT NULL DEFAULT 0,
            bytes_received INTEGER NOT NULL DEFAULT 0,
            relay          INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX sessions_started_at ON sessions(started_at);
         CREATE INDEX sessions_owner_id ON sessions(owner_id);
         CREATE INDEX sessions_device_id ON sessions(device_id);",
    )?;
    tx.pragma_update(None, "user_version", 5)?;
    tx.commit()?;
    Ok(())
}

/// v3 -> v4 (слайс 3.8a): таблица `accounts` (почта + хеш пароля владельца).
/// Одна транзакция; существующие строки не трогаются.
fn migrate_v3_to_v4(conn: &Connection) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE accounts (
            owner_id            TEXT PRIMARY KEY REFERENCES owners(owner_id) ON DELETE CASCADE,
            email               TEXT NOT NULL UNIQUE,
            password_hash       TEXT NOT NULL,
            created_at          INTEGER NOT NULL,
            password_changed_at INTEGER NOT NULL
         );",
    )?;
    tx.pragma_update(None, "user_version", 4)?;
    tx.commit()?;
    Ok(())
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

    fn owner_count(db: &Db) -> i64 {
        let conn = db.conn.lock().expect("db mutex poisoned");
        conn.query_row("SELECT COUNT(*) FROM owners", [], |r| r.get(0))
            .expect("count owners")
    }

    fn token_count(db: &Db, owner_id: &str) -> i64 {
        let conn = db.conn.lock().expect("db mutex poisoned");
        conn.query_row(
            "SELECT COUNT(*) FROM owner_tokens WHERE owner_id = ?1",
            [owner_id],
            |r| r.get(0),
        )
        .expect("count tokens")
    }

    #[test]
    fn checkpoint_truncates_wal_and_keeps_data() {
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-checkpoint-test-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        let path = dir.join("rcdesk.db");
        let wal = dir.join("rcdesk.db-wal");
        {
            let db = Db::open(&path).expect("open file db");
            for i in 0..20 {
                db.insert_owner(&format!("o{i}"), 100).expect("owner");
            }
            let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
            assert!(wal_len > 0, "WAL должен быть непустым до чекпоинта");

            let (busy, _log, _done) = db.checkpoint().expect("checkpoint");
            assert_eq!(busy, 0);
            let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
            assert_eq!(wal_len, 0, "WAL должен быть обнулён после чекпоинта");
        }
        let reopened = Db::open(&path).expect("reopen db");
        assert_eq!(owner_count(&reopened), 20);
        drop(reopened);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    }

    #[test]
    fn prune_removes_old_empty_owner_with_tokens() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("old", 100).expect("owner");
        db.insert_token("old", "t1", "h1", "", 100).expect("token");
        let n = db.prune_empty_owners(10_000, 1_000).expect("prune");
        assert_eq!(n, 1);
        assert_eq!(owner_count(&db), 0);
        assert_eq!(token_count(&db, "old"), 0);
    }

    #[test]
    fn prune_removes_old_owner_with_webauthn_id_but_no_passkey() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o", 100).expect("owner");
        db.set_webauthn_user_id("o", "uuid").expect("webauthn id");
        assert_eq!(db.prune_empty_owners(10_000, 1_000).expect("prune"), 1);
    }

    #[test]
    fn prune_keeps_owner_with_device() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_device("d", "h", "Mac", 100).expect("device");
        db.insert_owner("o", 100).expect("owner");
        db.link_device("o", "d", 100).expect("link");
        assert_eq!(db.prune_empty_owners(10_000, 1_000).expect("prune"), 0);
        assert_eq!(owner_count(&db), 1);
    }

    #[test]
    fn prune_keeps_owner_with_passkey() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o", 100).expect("owner");
        db.insert_passkey("pk", "o", "c", "{}", "", 100)
            .expect("passkey");
        assert_eq!(db.prune_empty_owners(10_000, 1_000).expect("prune"), 0);
        assert_eq!(owner_count(&db), 1);
    }

    #[test]
    fn prune_keeps_owner_with_fresh_token_despite_old_created_at() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o", 100).expect("owner");
        db.insert_token("o", "t1", "h1", "", 9_500).expect("token");
        assert_eq!(db.prune_empty_owners(10_000, 1_000).expect("prune"), 0);
        assert_eq!(owner_count(&db), 1);
    }

    #[test]
    fn prune_keeps_fresh_owner_without_tokens() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o", 9_500).expect("owner");
        assert_eq!(db.prune_empty_owners(10_000, 1_000).expect("prune"), 0);
        assert_eq!(owner_count(&db), 1);
    }

    #[test]
    fn prune_keeps_old_owner_when_one_of_two_tokens_is_fresh() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o", 100).expect("owner");
        db.insert_token("o", "t1", "h1", "", 100).expect("token");
        db.insert_token("o", "t2", "h2", "", 9_500).expect("token");
        assert_eq!(db.prune_empty_owners(10_000, 1_000).expect("prune"), 0);
        assert_eq!(token_count(&db, "o"), 2);
    }

    #[test]
    fn prune_returns_count_of_removed_owners_only() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 100).expect("owner");
        db.insert_owner("b", 200).expect("owner");
        db.insert_owner("fresh", 9_900).expect("owner");
        assert_eq!(db.prune_empty_owners(10_000, 1_000).expect("prune"), 2);
        assert_eq!(owner_count(&db), 1);
    }

    fn device_count(db: &Db) -> i64 {
        let conn = db.conn.lock().expect("db mutex poisoned");
        conn.query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .expect("count devices")
    }

    #[test]
    fn prune_unowned_devices_removes_old_unowned() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_device("old", "h", "Old", 100).expect("device");
        db.insert_device("old2", "h", "Old2", 200).expect("device");
        let n = db
            .prune_unowned_devices(10_000, 1_000, &[])
            .expect("prune devices");
        assert_eq!(n, 2);
        assert_eq!(device_count(&db), 0);
    }

    #[test]
    fn prune_unowned_devices_keeps_owned_and_fresh() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o", 100).expect("owner");
        db.insert_device("owned", "h", "Owned", 100)
            .expect("device");
        db.link_device("o", "owned", 100).expect("link");
        db.insert_device("fresh", "h", "Fresh", 9_900)
            .expect("device");
        let n = db
            .prune_unowned_devices(10_000, 1_000, &[])
            .expect("prune devices");
        assert_eq!(n, 0);
        assert_eq!(device_count(&db), 2);
    }

    #[test]
    fn prune_unowned_devices_keeps_online_ids() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_device("online", "h", "Online", 100)
            .expect("device");
        db.insert_device("gone", "h", "Gone", 100).expect("device");
        let n = db
            .prune_unowned_devices(10_000, 1_000, &["online".to_string()])
            .expect("prune devices");
        assert_eq!(n, 1);
        assert!(db.device("online").expect("query").is_some());
        assert!(db.device("gone").expect("query").is_none());
    }

    #[test]
    fn fresh_db_has_user_version_six() {
        let db = Db::in_memory().expect("open in-memory db");
        let conn = db.conn.lock().expect("db mutex poisoned");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 6);
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
    fn migration_v1_to_v5_keeps_owners_ownership_devices_and_tokens() {
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

        migrate(&conn).expect("migrate v1 -> v5");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 6);
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
        // Новая таблица v4 на месте и работает.
        assert!(db
            .insert_account("owner1", "a@b.co", "hash", 80)
            .expect("insert account"));
    }

    #[test]
    fn migration_v2_to_v5_keeps_owners_tokens_and_ownership() {
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

        migrate(&conn).expect("migrate v2 -> v5");
        migrate(&conn).expect("re-run migration is a no-op");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 6);

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
        assert!(db
            .insert_account("owner1", "a@b.co", "hash", 80)
            .expect("insert account"));
    }

    #[test]
    fn migration_v3_to_v5_keeps_rows_and_adds_accounts() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        conn.execute_batch(
            "CREATE TABLE devices (
                device_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL, name TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);
             CREATE TABLE owners (
                owner_id TEXT PRIMARY KEY, created_at INTEGER NOT NULL,
                webauthn_user_id TEXT);
             CREATE UNIQUE INDEX owners_webauthn_user_id ON owners(webauthn_user_id);
             CREATE TABLE owner_tokens (
                token_id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                token_hash TEXT NOT NULL UNIQUE, user_agent TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);
             CREATE TABLE ownership (
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
                alias TEXT, added_at INTEGER NOT NULL, PRIMARY KEY (owner_id, device_id));
             CREATE TABLE passkeys (
                passkey_id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                credential_id TEXT NOT NULL UNIQUE, passkey_json TEXT NOT NULL,
                user_agent TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);
             INSERT INTO devices VALUES ('dev1', 'sh', 'My Mac', 10, 20);
             INSERT INTO owners VALUES ('owner1', 50, 'u1');
             INSERT INTO owner_tokens VALUES ('tok1', 'owner1', 'h1', 'UA', 50, 55);
             INSERT INTO ownership VALUES ('owner1', 'dev1', 'Work', 60);
             INSERT INTO passkeys VALUES ('pk1', 'owner1', 'cred1', '{}', 'UA', 70, 71);
             PRAGMA user_version = 3;",
        )
        .expect("build v3 database");

        migrate(&conn).expect("migrate v3 -> v5");
        migrate(&conn).expect("re-run migration is a no-op");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 6);

        let db = Db {
            conn: Arc::new(Mutex::new(conn)),
        };
        assert_eq!(db.tokens_for_owner("owner1").expect("tokens").len(), 1);
        assert_eq!(db.devices_for_owner("owner1").expect("devices").len(), 1);
        assert_eq!(db.passkeys_for_owner("owner1").expect("passkeys").len(), 1);
        assert_eq!(
            db.webauthn_user_id("owner1").expect("uuid").as_deref(),
            Some("u1")
        );
        assert!(db
            .insert_account("owner1", "a@b.co", "hash", 80)
            .expect("insert account"));
    }

    #[test]
    fn migration_v4_to_v5_keeps_rows_and_adds_sessions() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        conn.execute_batch(
            "CREATE TABLE devices (
                device_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL, name TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL);
             CREATE TABLE owners (
                owner_id TEXT PRIMARY KEY, created_at INTEGER NOT NULL,
                webauthn_user_id TEXT);
             CREATE UNIQUE INDEX owners_webauthn_user_id ON owners(webauthn_user_id);
             CREATE TABLE owner_tokens (
                token_id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                token_hash TEXT NOT NULL UNIQUE, user_agent TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);
             CREATE TABLE ownership (
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                device_id TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
                alias TEXT, added_at INTEGER NOT NULL, PRIMARY KEY (owner_id, device_id));
             CREATE TABLE passkeys (
                passkey_id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL REFERENCES owners(owner_id) ON DELETE CASCADE,
                credential_id TEXT NOT NULL UNIQUE, passkey_json TEXT NOT NULL,
                user_agent TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);
             CREATE TABLE accounts (
                owner_id TEXT PRIMARY KEY REFERENCES owners(owner_id) ON DELETE CASCADE,
                email TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL, password_changed_at INTEGER NOT NULL);
             INSERT INTO devices VALUES ('dev1', 'sh', 'My Mac', 10, 20);
             INSERT INTO owners VALUES ('owner1', 50, 'u1');
             INSERT INTO ownership VALUES ('owner1', 'dev1', 'Work', 60);
             INSERT INTO accounts VALUES ('owner1', 'a@b.co', 'h', 70, 70);
             PRAGMA user_version = 4;",
        )
        .expect("build v4 database");

        migrate(&conn).expect("migrate v4 -> v5");
        migrate(&conn).expect("re-run migration is a no-op");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 6);

        let db = Db {
            conn: Arc::new(Mutex::new(conn)),
        };
        assert_eq!(db.devices_for_owner("owner1").expect("devices").len(), 1);
        assert_eq!(db.host_version("dev1").expect("version"), None);
        assert!(db.set_host_version("dev1", "0.3.0").expect("set"));
        assert_eq!(
            db.host_version("dev1").expect("version").as_deref(),
            Some("0.3.0")
        );
        db.insert_session("s1", "dev1", Some("owner1"), "pin", "UA", 100)
            .expect("insert session");
        assert!(db.session("s1").expect("query").is_some());
        let conn = db.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "UPDATE owners SET admin_note = 'n' WHERE owner_id = 'owner1'",
            [],
        )
        .expect("admin_note column exists");
    }

    #[test]
    fn migration_v5_to_v6_adds_device_ifaces_and_cascades() {
        let conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        conn.execute_batch(
            "CREATE TABLE devices (
                device_id TEXT PRIMARY KEY, secret_hash TEXT NOT NULL, name TEXT NOT NULL,
                created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
                host_version TEXT);
             INSERT INTO devices VALUES ('dev1', 'sh', 'My Mac', 10, 20, NULL);
             PRAGMA user_version = 5;",
        )
        .expect("build v5 database");

        migrate(&conn).expect("migrate v5 -> v6");
        migrate(&conn).expect("re-run migration is a no-op");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, 6);

        let db = Db {
            conn: Arc::new(Mutex::new(conn)),
        };
        let iface = proto::signal::NetIface {
            mac: "aa:bb:cc:dd:ee:ff".to_string(),
            ipv4: "192.168.1.5".to_string(),
            prefix: 24,
        };
        db.set_device_ifaces("dev1", std::slice::from_ref(&iface))
            .expect("set");
        assert_eq!(db.device_ifaces("dev1").expect("get"), vec![iface]);
        {
            let conn = db.conn.lock().expect("db mutex poisoned");
            conn.execute("DELETE FROM devices WHERE device_id = 'dev1'", [])
                .expect("delete device");
        }
        assert!(db.device_ifaces("dev1").expect("get").is_empty());
    }

    #[test]
    fn set_device_ifaces_replaces_whole_set() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_device("dev1", "h", "Mac", 1).expect("device");
        let mk = |mac: &str| proto::signal::NetIface {
            mac: mac.to_string(),
            ipv4: "10.0.0.5".to_string(),
            prefix: 24,
        };
        db.set_device_ifaces("dev1", &[mk("aa:aa:aa:aa:aa:01"), mk("aa:aa:aa:aa:aa:02")])
            .expect("set");
        assert_eq!(db.device_ifaces("dev1").expect("get").len(), 2);
        db.set_device_ifaces("dev1", &[mk("aa:aa:aa:aa:aa:03")])
            .expect("set");
        let got = db.device_ifaces("dev1").expect("get");
        assert_eq!(got, vec![mk("aa:aa:aa:aa:aa:03")]);
        db.set_device_ifaces("dev1", &[]).expect("clear");
        assert!(db.device_ifaces("dev1").expect("get").is_empty());
    }

    fn session_db() -> Db {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_session("s1", "dev1", Some("owner1"), "pin", "UA", 100)
            .expect("insert session");
        db
    }

    #[test]
    fn insert_session_round_trips_and_is_open() {
        let db = session_db();
        let row = db.session("s1").expect("query").expect("row");
        assert_eq!(row.device_id, "dev1");
        assert_eq!(row.owner_id.as_deref(), Some("owner1"));
        assert_eq!(row.via, "pin");
        assert_eq!(row.client_ua, "UA");
        assert_eq!((row.started_at, row.last_seen_at), (100, 100));
        assert_eq!((row.ended_at, row.end_reason.as_deref()), (None, None));
        assert_eq!(
            (row.bytes_sent, row.bytes_received, row.relay),
            (0, 0, false)
        );
        // Повторная вставка того же id не падает и не меняет строку.
        db.insert_session("s1", "dev9", None, "device", "X", 500)
            .expect("duplicate insert ignored");
        assert_eq!(db.session("s1").expect("q").expect("row"), row);
    }

    #[test]
    fn record_session_stats_is_cumulative_max_and_relay_sticky() {
        let db = session_db();
        assert!(db
            .record_session_stats("s1", "dev1", 100, 200, false, false, 110)
            .expect("stats"));
        // Меньшие значения (например, после сброса счётчика) не уменьшают байты.
        assert!(db
            .record_session_stats("s1", "dev1", 50, 300, true, false, 120)
            .expect("stats"));
        assert!(db
            .record_session_stats("s1", "dev1", 150, 100, false, false, 130)
            .expect("stats"));
        let row = db.session("s1").expect("q").expect("row");
        assert_eq!((row.bytes_sent, row.bytes_received), (150, 300));
        assert!(row.relay);
        assert_eq!(row.last_seen_at, 130);
        assert_eq!(row.ended_at, None);
    }

    #[test]
    fn record_session_stats_ignores_foreign_device_and_unknown_session() {
        let db = session_db();
        assert!(!db
            .record_session_stats("s1", "other", 999, 999, true, true, 110)
            .expect("stats"));
        assert!(!db
            .record_session_stats("nope", "dev1", 1, 1, false, false, 110)
            .expect("stats"));
        let row = db.session("s1").expect("q").expect("row");
        assert_eq!(
            (row.bytes_sent, row.bytes_received, row.relay),
            (0, 0, false)
        );
        assert_eq!((row.last_seen_at, row.ended_at), (100, None));
    }

    #[test]
    fn record_session_stats_final_closes_once() {
        let db = session_db();
        db.record_session_stats("s1", "dev1", 10, 20, false, true, 150)
            .expect("stats");
        let row = db.session("s1").expect("q").expect("row");
        assert_eq!(row.ended_at, Some(150));
        assert_eq!(row.end_reason.as_deref(), Some("host"));
        // Второй final не перезаписывает момент и причину.
        db.record_session_stats("s1", "dev1", 30, 40, false, true, 200)
            .expect("stats");
        let row = db.session("s1").expect("q").expect("row");
        assert_eq!(row.ended_at, Some(150));
        assert_eq!(row.end_reason.as_deref(), Some("host"));
        assert_eq!((row.bytes_sent, row.bytes_received), (30, 40));
        assert_eq!(row.last_seen_at, 200);
    }

    #[test]
    fn end_session_is_idempotent_and_keeps_first_reason() {
        let db = session_db();
        assert!(db.end_session("s1", "client_bye", 120).expect("end"));
        assert!(db.end_session("s1", "host_bye", 130).expect("end again"));
        let row = db.session("s1").expect("q").expect("row");
        assert_eq!(row.ended_at, Some(120));
        assert_eq!(row.end_reason.as_deref(), Some("client_bye"));
        assert_eq!(row.last_seen_at, 130);
        assert!(!db.end_session("nope", "x", 1).expect("unknown"));
    }

    #[test]
    fn prune_sessions_removes_only_old_rows() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_session("old", "d", None, "pin", "", 100)
            .expect("insert");
        db.insert_session("new", "d", None, "pin", "", 900)
            .expect("insert");
        assert_eq!(db.prune_sessions(1000, 500).expect("prune"), 1);
        assert!(db.session("old").expect("q").is_none());
        assert!(db.session("new").expect("q").is_some());
        assert_eq!(db.prune_sessions(1000, 500).expect("prune"), 0);
    }

    #[test]
    fn sessions_survive_owner_and_device_cleanup() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("o1", 10).expect("owner");
        db.insert_device("d1", "h", "Mac", 10).expect("device");
        db.insert_session("s1", "d1", Some("o1"), "pin", "", 20)
            .expect("insert");
        assert_eq!(db.prune_empty_owners(10_000_000, 1).expect("prune"), 1);
        assert_eq!(
            db.prune_unowned_devices(10_000_000, 1, &[]).expect("prune"),
            1
        );
        assert!(db.session("s1").expect("q").is_some());
    }

    #[test]
    fn merge_anonymous_owner_moves_sessions() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("anon", 10).expect("owner");
        db.insert_owner("acc", 10).expect("owner");
        db.insert_session("s1", "d1", Some("anon"), "pin", "", 20)
            .expect("insert");
        db.insert_session("s2", "d1", Some("acc"), "pin", "", 20)
            .expect("insert");
        assert!(db.merge_anonymous_owner("anon", "acc").expect("merge"));
        assert_eq!(
            db.session("s1")
                .expect("q")
                .expect("row")
                .owner_id
                .as_deref(),
            Some("acc")
        );
        assert_eq!(
            db.session("s2")
                .expect("q")
                .expect("row")
                .owner_id
                .as_deref(),
            Some("acc")
        );
    }

    #[test]
    fn set_host_version_unknown_device_is_false() {
        let db = Db::in_memory().expect("open in-memory db");
        assert!(!db.set_host_version("nope", "1").expect("set"));
    }

    #[test]
    fn account_insert_lookup_and_unique_email() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        assert_eq!(db.email_for_owner("a").expect("email"), None);
        assert_eq!(db.account_by_email("a@b.co").expect("find"), None);

        assert!(db.insert_account("a", "a@b.co", "h1", 10).expect("insert"));
        assert_eq!(
            db.account_by_email("a@b.co").expect("find"),
            Some(("a".to_string(), "h1".to_string()))
        );
        assert_eq!(
            db.email_for_owner("a").expect("email").as_deref(),
            Some("a@b.co")
        );

        // Почта занята другим владельцем; второй аккаунт у того же владельца.
        assert!(!db.insert_account("b", "a@b.co", "h2", 11).expect("dup"));
        assert!(!db.insert_account("a", "x@y.co", "h2", 11).expect("dup"));
        assert_eq!(db.email_for_owner("b").expect("email"), None);
    }

    #[test]
    fn set_password_hash_changes_hash_and_timestamp() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        db.insert_account("a", "a@b.co", "h1", 10).expect("insert");

        assert!(db.set_password_hash("a", "h2", 20).expect("set"));
        assert!(!db.set_password_hash("b", "h2", 20).expect("no account"));
        assert_eq!(
            db.account_by_email("a@b.co").expect("find"),
            Some(("a".to_string(), "h2".to_string()))
        );
        let conn = db.conn.lock().expect("db mutex poisoned");
        let (created, changed): (i64, i64) = conn
            .query_row(
                "SELECT created_at, password_changed_at FROM accounts WHERE owner_id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("timestamps");
        assert_eq!((created, changed), (10, 20));
    }

    #[test]
    fn account_for_owner_returns_email_and_hash() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        db.insert_account("a", "a@b.co", "h1", 10).expect("insert");
        assert_eq!(
            db.account_for_owner("a").expect("find"),
            Some(("a@b.co".to_string(), "h1".to_string()))
        );
        assert_eq!(db.account_for_owner("b").expect("none"), None);
    }

    #[test]
    fn set_account_email_changes_email_keeps_hash() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_account("a", "a@b.co", "h1", 10).expect("insert");

        assert_eq!(
            db.set_account_email("a", "new@b.co").expect("set"),
            SetEmailOutcome::Updated
        );
        assert_eq!(db.account_by_email("a@b.co").expect("old"), None);
        assert_eq!(
            db.account_by_email("new@b.co").expect("new"),
            Some(("a".to_string(), "h1".to_string()))
        );
        assert_eq!(
            db.email_for_owner("a").expect("email"),
            Some("new@b.co".to_string())
        );
    }

    #[test]
    fn set_account_email_refuses_a_taken_email_and_changes_nothing() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        db.insert_account("a", "a@b.co", "h1", 10).expect("insert");
        db.insert_account("b", "b@b.co", "h2", 10).expect("insert");

        assert_eq!(
            db.set_account_email("a", "b@b.co").expect("set"),
            SetEmailOutcome::EmailTaken
        );
        assert_eq!(
            db.account_by_email("a@b.co").expect("a"),
            Some(("a".to_string(), "h1".to_string()))
        );
        assert_eq!(
            db.account_by_email("b@b.co").expect("b"),
            Some(("b".to_string(), "h2".to_string()))
        );
    }

    #[test]
    fn set_account_email_without_account_reports_no_account() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        assert_eq!(
            db.set_account_email("a", "new@b.co").expect("set"),
            SetEmailOutcome::NoAccount
        );
        assert_eq!(db.account_by_email("new@b.co").expect("none"), None);
    }

    #[test]
    fn delete_all_tokens_removes_only_that_owners_tokens() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        db.insert_token("a", "t1", "h1", "", 1).expect("token");
        db.insert_token("a", "t2", "h2", "", 1).expect("token");
        db.insert_token("b", "t3", "h3", "", 1).expect("token");

        assert_eq!(db.delete_all_tokens("a").expect("delete"), 2);
        assert_eq!(token_count(&db, "a"), 0);
        assert_eq!(token_count(&db, "b"), 1);
    }

    #[test]
    fn merge_anonymous_owner_does_not_merge_owner_with_account() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("a", 1).expect("owner");
        db.insert_owner("b", 1).expect("owner");
        db.insert_token("a", "tok-a", "ha", "", 1).expect("token");
        db.insert_device("dev1", "h", "d", 1).expect("device");
        db.link_device("a", "dev1", 10).expect("link");
        db.insert_account("a", "a@b.co", "h", 1).expect("account");

        assert!(!db.merge_anonymous_owner("a", "b").expect("merge"));
        assert_eq!(db.devices_for_owner("a").expect("a").len(), 1);
        assert!(db.devices_for_owner("b").expect("b").is_empty());
        assert!(db.token_exists("tok-a").expect("token"));
    }

    #[test]
    fn prune_keeps_owner_with_account_and_no_devices() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("old", 100).expect("owner");
        db.insert_account("old", "a@b.co", "h", 100)
            .expect("account");
        let n = db.prune_empty_owners(10_000, 1_000).expect("prune");
        assert_eq!(n, 0);
        assert_eq!(owner_count(&db), 1);
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
    fn owners_of_device_lists_every_linked_owner() {
        let db = Db::in_memory().expect("open in-memory db");
        db.insert_owner("b", 100).expect("owner b");
        db.insert_owner("a", 100).expect("owner a");
        db.insert_device("dev1", "hash1", "Mac", 100)
            .expect("insert device");
        db.insert_device("dev2", "hash2", "Win", 100)
            .expect("insert device");
        db.link_device("b", "dev1", 200).expect("link");
        db.link_device("a", "dev1", 200).expect("link");
        db.link_device("a", "dev2", 200).expect("link");

        assert_eq!(db.owners_of_device("dev1").expect("owners"), ["a", "b"]);
        assert_eq!(db.owners_of_device("dev2").expect("owners"), ["a"]);
        assert!(db.owners_of_device("none").expect("owners").is_empty());
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
