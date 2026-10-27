//! Админский кабинет (A3): список администраторов и сборка снимка
//! `AdminSnapshot` из БД и реестра. Не держит мьютекс БД через `await`:
//! всё синхронно, запросы короткие.

use std::collections::{HashMap, HashSet};

use proto::signal::{AdminDay, AdminDevice, AdminSession, AdminSnapshot, AdminSummary, AdminUser};

use crate::db::{Db, SessionRow};
use crate::registry::Registry;

/// Месячный бюджет релея, байт (ARCHITECTURE §11).
pub const RELAY_BUDGET_BYTES: i64 = 5_000_000_000_000;
/// Сколько последних сеансов уходит в снимок.
pub const SESSIONS_LIMIT: usize = 200;
/// Окно «за 30 дней» и длина `daily`: сегодня плюс 29 предыдущих суток UTC.
pub const DAILY_DAYS: i64 = 30;
/// Максимальная длина заметки администратора, символов.
pub const NOTE_MAX_CHARS: usize = 200;
/// Подпись владельца без почты в `AdminDevice.owners`.
pub const ANONYMOUS: &str = "анонимный";

const DAY: i64 = 86_400;

/// Разбирает `RCDESK_ADMIN_EMAILS`: через запятую, trim + lowercase (как
/// хранятся почты в `accounts`), пустые пропускаются.
pub fn parse_admin_emails(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim().to_lowercase())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Список администраторов из переменной окружения `RCDESK_ADMIN_EMAILS`.
pub fn admin_emails_from_env() -> Vec<String> {
    parse_admin_emails(&std::env::var("RCDESK_ADMIN_EMAILS").unwrap_or_default())
}

/// Заметка администратора: trim, не более 200 символов, пустая -- `None`.
pub fn normalize_note(note: Option<&str>) -> Option<String> {
    let trimmed = note?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(NOTE_MAX_CHARS).collect())
}

/// Дней от 1970-01-01 до гражданской даты (алгоритм Ховарда Хиннанта).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Гражданская дата (год, месяц, день) по числу дней от 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn day_label(day_index: i64) -> String {
    let (y, m, d) = civil_from_days(day_index);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Длительность сеанса: `ended_at`, иначе `now` если сеанс активен, иначе
/// `last_seen_at`; минус `started_at`, не меньше 0.
fn duration_secs(row: &SessionRow, active: bool, now: i64) -> i64 {
    let end = match row.ended_at {
        Some(ended) => ended,
        None if active => now,
        None => row.last_seen_at,
    };
    (end - row.started_at).max(0)
}

fn session_bytes(row: &SessionRow) -> i64 {
    row.bytes_sent.saturating_add(row.bytes_received)
}

/// Собирает снимок кабинета на момент `now` (unix, секунды).
pub fn build_snapshot(db: &Db, registry: &Registry, now: i64) -> anyhow::Result<AdminSnapshot> {
    let owners = db.admin_owner_rows()?;
    let device_rows = db.admin_device_rows()?;
    let ownership = db.admin_ownership()?;
    let sessions = db.all_sessions()?;
    let active_ids: HashSet<String> = registry.active_session_ids();
    let hosts_online = registry.online_host_ids().len() as i64;

    let today = now.div_euclid(DAY);
    let window_start_day = today - (DAILY_DAYS - 1);
    let window_start = window_start_day * DAY;
    let (year, month, _) = civil_from_days(today);
    let month_start = days_from_civil(year, month, 1) * DAY;

    // Дни окна без пропусков, по возрастанию.
    let mut daily: Vec<AdminDay> = (window_start_day..=today)
        .map(|day| AdminDay {
            day: day_label(day),
            sessions: 0,
            seconds: 0,
            bytes: 0,
            relay_bytes: 0,
        })
        .collect();

    #[derive(Default)]
    struct PerOwner {
        sessions: i64,
        seconds: i64,
        bytes_30d: i64,
        bytes_total: i64,
    }
    let mut per_owner: HashMap<&str, PerOwner> = HashMap::new();
    let (mut bytes_month, mut relay_month, mut bytes_30d, mut relay_30d) = (0i64, 0i64, 0i64, 0i64);
    let mut sessions_active = 0i64;

    for row in &sessions {
        let active = active_ids.contains(&row.session_id);
        if active {
            sessions_active += 1;
        }
        let seconds = duration_secs(row, active, now);
        let bytes = session_bytes(row);
        let relay_bytes = if row.relay { bytes } else { 0 };
        let in_window = row.started_at >= window_start;

        if row.started_at >= month_start {
            bytes_month = bytes_month.saturating_add(bytes);
            relay_month = relay_month.saturating_add(relay_bytes);
        }
        if in_window {
            bytes_30d = bytes_30d.saturating_add(bytes);
            relay_30d = relay_30d.saturating_add(relay_bytes);
            let idx = row.started_at.div_euclid(DAY) - window_start_day;
            if let Some(day) = usize::try_from(idx).ok().and_then(|i| daily.get_mut(i)) {
                day.sessions += 1;
                day.seconds += seconds;
                day.bytes = day.bytes.saturating_add(bytes);
                day.relay_bytes = day.relay_bytes.saturating_add(relay_bytes);
            }
        }
        if let Some(owner_id) = row.owner_id.as_deref() {
            let entry = per_owner.entry(owner_id).or_default();
            entry.sessions += 1;
            entry.seconds += seconds;
            entry.bytes_total = entry.bytes_total.saturating_add(bytes);
            if in_window {
                entry.bytes_30d = entry.bytes_30d.saturating_add(bytes);
            }
        }
    }

    let email_of: HashMap<&str, &str> = owners
        .iter()
        .filter_map(|o| o.email.as_deref().map(|e| (o.owner_id.as_str(), e)))
        .collect();
    let device_name: HashMap<&str, &str> = device_rows
        .iter()
        .map(|d| (d.device_id.as_str(), d.name.as_str()))
        .collect();

    let summary = AdminSummary {
        users_total: owners.len() as i64,
        users_with_email: owners.iter().filter(|o| o.email.is_some()).count() as i64,
        new_users_7d: owners
            .iter()
            .filter(|o| o.created_at >= now - 7 * DAY)
            .count() as i64,
        new_users_30d: owners
            .iter()
            .filter(|o| o.created_at >= now - 30 * DAY)
            .count() as i64,
        devices_total: device_rows.len() as i64,
        hosts_online,
        sessions_active,
        bytes_month,
        relay_bytes_month: relay_month,
        bytes_30d,
        relay_bytes_30d: relay_30d,
        relay_budget_bytes: RELAY_BUDGET_BYTES,
    };

    let mut users: Vec<AdminUser> = owners
        .iter()
        .map(|o| {
            let stats = per_owner.get(o.owner_id.as_str());
            AdminUser {
                owner_id: o.owner_id.clone(),
                email: o.email.clone(),
                note: o.note.clone(),
                created_at: o.created_at,
                last_active_at: o.last_active_at,
                devices: o.devices,
                browsers: o.browsers,
                passkeys: o.passkeys,
                sessions_total: stats.map_or(0, |s| s.sessions),
                seconds_total: stats.map_or(0, |s| s.seconds),
                bytes_30d: stats.map_or(0, |s| s.bytes_30d),
                bytes_total: stats.map_or(0, |s| s.bytes_total),
            }
        })
        .collect();
    users.sort_by(|a, b| {
        b.last_active_at
            .cmp(&a.last_active_at)
            .then_with(|| a.owner_id.cmp(&b.owner_id))
    });

    let mut owners_of: HashMap<&str, Vec<String>> = HashMap::new();
    for (device_id, owner_id) in &ownership {
        let label = email_of
            .get(owner_id.as_str())
            .map_or_else(|| ANONYMOUS.to_string(), |e| (*e).to_string());
        owners_of.entry(device_id.as_str()).or_default().push(label);
    }
    let mut devices: Vec<AdminDevice> = device_rows
        .iter()
        .map(|d| {
            let presence = registry.host_presence(&d.device_id);
            let mut labels = owners_of.remove(d.device_id.as_str()).unwrap_or_default();
            labels.sort();
            AdminDevice {
                device_id: d.device_id.clone(),
                name: d.name.clone(),
                owners: labels,
                online: presence.is_some(),
                busy: presence == Some(true),
                created_at: d.created_at,
                last_seen_at: d.last_seen_at,
                host_version: d.host_version.clone(),
            }
        })
        .collect();
    devices.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| b.last_seen_at.cmp(&a.last_seen_at))
            .then_with(|| a.device_id.cmp(&b.device_id))
    });

    // `all_sessions` уже отсортирован по `started_at` по убыванию.
    let session_list: Vec<AdminSession> = sessions
        .iter()
        .take(SESSIONS_LIMIT)
        .map(|row| AdminSession {
            session_id: row.session_id.clone(),
            device_id: row.device_id.clone(),
            device_name: device_name
                .get(row.device_id.as_str())
                .map(|n| (*n).to_string()),
            owner_id: row.owner_id.clone(),
            email: row
                .owner_id
                .as_deref()
                .and_then(|id| email_of.get(id))
                .map(|e| (*e).to_string()),
            via: row.via.clone(),
            client_ua: row.client_ua.clone(),
            started_at: row.started_at,
            last_seen_at: row.last_seen_at,
            ended_at: row.ended_at,
            end_reason: row.end_reason.clone(),
            active: active_ids.contains(&row.session_id),
            bytes_sent: row.bytes_sent,
            bytes_received: row.bytes_received,
            relay: row.relay,
        })
        .collect();

    Ok(AdminSnapshot {
        generated_at: now,
        summary,
        users,
        devices,
        sessions: session_list,
        daily,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Tx;

    // 2026-10-27 12:00:00 UTC.
    const NOW: i64 = 1_793_102_400;

    #[test]
    fn parse_admin_emails_trims_lowercases_and_skips_empty() {
        assert_eq!(
            parse_admin_emails(" Boss@Example.com, ,x@y.zz,"),
            vec!["boss@example.com".to_string(), "x@y.zz".to_string()]
        );
        assert!(parse_admin_emails("").is_empty());
    }

    #[test]
    fn normalize_note_trims_truncates_and_clears() {
        assert_eq!(normalize_note(None), None);
        assert_eq!(normalize_note(Some("  \n ")), None);
        assert_eq!(normalize_note(Some(" друг ")), Some("друг".to_string()));
        let long = "ж".repeat(300);
        assert_eq!(normalize_note(Some(&long)).unwrap().chars().count(), 200);
    }

    #[test]
    fn civil_conversions_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(day_label(NOW / DAY), "2026-10-27");
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(day_label(days_from_civil(2026, 3, 1) - 1), "2026-02-28");
    }

    fn seeded() -> (Db, Registry) {
        let db = Db::in_memory().expect("db");
        let reg = Registry::new();
        // Владелец с почтой, свежий, и анонимный старый.
        db.insert_owner("o1", NOW - 100 * DAY).unwrap();
        db.insert_account("o1", "boss@example.com", "h", NOW - 100 * DAY)
            .unwrap();
        db.insert_token("o1", "t1", "th1", "UA", NOW - 100 * DAY)
            .unwrap();
        db.touch_token("t1", "UA", NOW - 3 * DAY).unwrap();
        db.insert_owner("o2", NOW - 2 * DAY).unwrap();
        db.insert_device("d1", "s", "Mac", NOW - 50 * DAY).unwrap();
        db.insert_device("d2", "s", "Win", NOW - 40 * DAY).unwrap();
        db.touch_device("d2", "Win", NOW - 10).unwrap();
        db.link_device("o1", "d1", NOW).unwrap();
        db.link_device("o2", "d1", NOW).unwrap();
        db.set_host_version("d1", "0.2.0").unwrap();
        (db, reg)
    }

    #[test]
    fn snapshot_aggregates_users_devices_and_days() {
        let (db, reg) = seeded();
        // Сегодня: закрытый сеанс 60 с, 100+50 байт, через релей.
        let t0 = NOW - 3600;
        db.insert_session("s1", "d1", Some("o1"), "pin", "UA", t0)
            .unwrap();
        db.record_session_stats("s1", "d1", 100, 50, true, false, t0 + 60)
            .unwrap();
        db.end_session("s1", "bye", t0 + 60).unwrap();
        // 5 дней назад: сеанс без ended_at, неактивный -> last_seen_at.
        let t1 = NOW - 5 * DAY;
        db.insert_session("s2", "d1", Some("o1"), "device", "UA", t1)
            .unwrap();
        db.record_session_stats("s2", "d1", 10, 10, false, false, t1 + 30)
            .unwrap();
        // 40 дней назад: вне окна 30 дней, но в total.
        let t2 = NOW - 40 * DAY;
        db.insert_session("s3", "d2", Some("o1"), "pin", "UA", t2)
            .unwrap();
        db.record_session_stats("s3", "d2", 1000, 0, true, true, t2 + 10)
            .unwrap();
        // Активный сеанс анонима: нет ended_at, есть в реестре.
        let (tx_h, _rx_h, _k) = Tx::channel();
        reg.register_host("d1".into(), "Mac".into(), tx_h, None, false);
        db.insert_session("s4", "d1", Some("o2"), "pin", "UA2", NOW - 20)
            .unwrap();

        let snap = build_snapshot(&db, &reg, NOW).expect("snapshot");

        assert_eq!(snap.generated_at, NOW);
        assert_eq!(snap.summary.users_total, 2);
        assert_eq!(snap.summary.users_with_email, 1);
        assert_eq!(snap.summary.new_users_7d, 1);
        assert_eq!(snap.summary.new_users_30d, 1);
        assert_eq!(snap.summary.devices_total, 2);
        assert_eq!(snap.summary.hosts_online, 1);
        assert_eq!(snap.summary.relay_budget_bytes, RELAY_BUDGET_BYTES);
        // s1 + s2 (+ s4 с нулём) в окне; s3 вне.
        assert_eq!(snap.summary.bytes_30d, 150 + 20);
        assert_eq!(snap.summary.relay_bytes_30d, 150);
        // Октябрь 2026: s1 и s4 (27-го), s2 -- 22 октября, тоже в месяце.
        assert_eq!(snap.summary.bytes_month, 150 + 20);
        assert_eq!(snap.summary.relay_bytes_month, 150);

        // Дни: 30 штук, по возрастанию, без пропусков, последний -- сегодня.
        assert_eq!(snap.daily.len(), 30);
        assert_eq!(snap.daily.last().unwrap().day, "2026-10-27");
        assert_eq!(snap.daily.first().unwrap().day, "2026-09-28");
        for pair in snap.daily.windows(2) {
            assert!(pair[0].day < pair[1].day);
        }
        let today = snap.daily.last().unwrap();
        assert_eq!(today.sessions, 2);
        // s4 неактивен (нет в реестре) и без отчётов: last_seen_at == started_at.
        assert_eq!(today.seconds, 60);
        assert_eq!(today.bytes, 150);
        assert_eq!(today.relay_bytes, 150);
        let d5 = &snap.daily[29 - 5];
        assert_eq!(d5.day, "2026-10-22");
        assert_eq!(d5.sessions, 1);
        assert_eq!(d5.seconds, 30);
        assert_eq!(d5.relay_bytes, 0);
        assert_eq!(snap.daily[0].sessions, 0);

        // Пользователи: o1 активнее (токен 3 дня назад) против o2 (2 дня).
        assert_eq!(snap.users.len(), 2);
        assert_eq!(snap.users[0].owner_id, "o2");
        assert_eq!(snap.users[0].last_active_at, NOW - 2 * DAY);
        let o1 = &snap.users[1];
        assert_eq!(o1.owner_id, "o1");
        assert_eq!(o1.last_active_at, NOW - 3 * DAY);
        assert_eq!(o1.email.as_deref(), Some("boss@example.com"));
        assert_eq!(o1.devices, 1);
        assert_eq!(o1.browsers, 1);
        assert_eq!(o1.sessions_total, 3);
        assert_eq!(o1.seconds_total, 60 + 30 + 10);
        assert_eq!(o1.bytes_30d, 170);
        assert_eq!(o1.bytes_total, 1170);

        // Устройства: онлайн первым, затем по last_seen_at.
        assert_eq!(snap.devices[0].device_id, "d1");
        assert!(snap.devices[0].online);
        assert!(!snap.devices[0].busy);
        assert_eq!(snap.devices[0].host_version.as_deref(), Some("0.2.0"));
        assert_eq!(
            snap.devices[0].owners,
            vec!["boss@example.com".to_string(), ANONYMOUS.to_string()]
        );
        assert_eq!(snap.devices[1].device_id, "d2");
        assert!(snap.devices[1].owners.is_empty());

        // Сеансы: по started_at убыв., email и имя устройства подставлены.
        let ids: Vec<&str> = snap
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(ids, ["s4", "s1", "s2", "s3"]);
        assert_eq!(snap.sessions[1].email.as_deref(), Some("boss@example.com"));
        assert_eq!(snap.sessions[1].device_name.as_deref(), Some("Mac"));
        assert!(!snap.sessions[1].active);
        assert!(snap.sessions[1].relay);
    }

    #[test]
    fn active_session_counts_up_to_now() {
        let (db, reg) = seeded();
        let (host_tx, _hrx, _k) = Tx::channel();
        let (client_tx, _crx, _k2) = Tx::channel();
        reg.register_host("d1".into(), "Mac".into(), host_tx, None, false);
        let (session_id, _name, _tx) = reg.join_by_host_id("d1", client_tx).expect("join");
        db.insert_session(&session_id, "d1", Some("o1"), "pin", "UA", NOW - 600)
            .unwrap();

        let snap = build_snapshot(&db, &reg, NOW).expect("snapshot");
        assert_eq!(snap.summary.sessions_active, 1);
        assert!(snap.sessions[0].active);
        assert!(snap.devices[0].busy);
        let o1 = snap.users.iter().find(|u| u.owner_id == "o1").unwrap();
        assert_eq!(o1.seconds_total, 600);
    }

    #[test]
    fn empty_database_gives_zero_snapshot_with_full_days() {
        let db = Db::in_memory().unwrap();
        let snap = build_snapshot(&db, &Registry::new(), NOW).unwrap();
        assert_eq!(snap.summary.users_total, 0);
        assert_eq!(snap.daily.len(), 30);
        assert!(snap.daily.iter().all(|d| d.sessions == 0 && d.bytes == 0));
        assert!(snap.users.is_empty() && snap.devices.is_empty() && snap.sessions.is_empty());
    }

    #[test]
    fn sessions_are_limited_to_200_newest() {
        let db = Db::in_memory().unwrap();
        for i in 0..250 {
            db.insert_session(&format!("s{i:03}"), "d", None, "pin", "", NOW - 1000 + i)
                .unwrap();
        }
        let snap = build_snapshot(&db, &Registry::new(), NOW).unwrap();
        assert_eq!(snap.sessions.len(), 200);
        assert_eq!(snap.sessions[0].session_id, "s249");
        assert_eq!(snap.sessions[199].session_id, "s050");
    }

    #[test]
    fn negative_duration_is_clamped_to_zero() {
        let row = SessionRow {
            session_id: "x".into(),
            device_id: "d".into(),
            owner_id: None,
            via: "pin".into(),
            client_ua: String::new(),
            started_at: 100,
            last_seen_at: 50,
            ended_at: None,
            end_reason: None,
            bytes_sent: 0,
            bytes_received: 0,
            relay: false,
        };
        assert_eq!(duration_secs(&row, false, 200), 0);
        assert_eq!(duration_secs(&row, true, 200), 100);
    }
}
