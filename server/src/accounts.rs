//! Учётные записи «почта + пароль» (слайс 3.8a): нормализация почты, правила
//! пароля, argon2-хеширование и одноразовые коды подтверждения почты.
//! Коды живут только в памяти (как незавершённые церемонии passkey) и не
//! переживают рестарт сервера -- для 10-минутного кода это нормально.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngExt;

/// Максимальная длина почты (RFC 5321), в байтах.
pub const MAX_EMAIL_LEN: usize = 254;
/// Допустимая длина пароля, в символах.
pub const MIN_PASSWORD_CHARS: usize = 8;
pub const MAX_PASSWORD_CHARS: usize = 128;

/// Длина кода подтверждения, в цифрах.
pub const CODE_DIGITS: usize = 6;
/// Сколько живёт код.
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);
/// Сколько неверных попыток ввода даётся на один код.
pub const CODE_MAX_ATTEMPTS: u32 = 5;
/// Минимальный интервал между выдачами кода одной почте.
pub const CODE_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Окно и лимит выдач кода одной почте.
pub const CODE_WINDOW: Duration = Duration::from_secs(3600);
pub const CODE_MAX_PER_WINDOW: usize = 5;

/// Приводит почту к виду для хранения и поиска (trim + lowercase) и
/// проверяет её грубо: длина, ровно один `@`, непустые локальная часть и
/// домен, точка в домене, никаких пробельных и управляющих символов.
pub fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    if email.len() > MAX_EMAIL_LEN {
        return None;
    }
    if email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let mut parts = email.split('@');
    let local = parts.next()?;
    let domain = parts.next()?;
    if parts.next().is_some() || local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return None;
    }
    Some(email)
}

/// Подходит ли пароль по длине (в символах).
pub fn password_is_valid(password: &str) -> bool {
    (MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&password.chars().count())
}

/// Хеш пароля argon2 (параметры по умолчанию крейта), PHC-строка со
/// случайной солью.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let mut salt_bytes = [0u8; 16];
    rand::rng().fill(&mut salt_bytes[..]);
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|err| anyhow::anyhow!("failed to encode salt: {err}"))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|err| anyhow::anyhow!("failed to hash password: {err}"))?;
    Ok(hash.to_string())
}

/// Сверяет пароль с PHC-строкой. Нечитаемый хеш -- просто `false`.
pub fn verify_password(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Фиктивный хеш для выравнивания времени ответа, когда аккаунта нет.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("rcdesk-dummy-password").unwrap_or_else(|_| String::from("invalid"))
    })
}

/// Выполняет verify против фиктивного хеша; результат всегда отрицательный.
pub fn verify_dummy(password: &str) {
    let _ = verify_password(dummy_hash(), password);
}

/// `hash_password` в `spawn_blocking`: argon2 -- десятки миллисекунд CPU.
pub async fn hash_password_blocking(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|err| anyhow::anyhow!("hash task failed: {err}"))?
}

/// `verify_password` в `spawn_blocking`. `hash = None` -- аккаунта нет:
/// проверяется фиктивный хеш, результат `false`.
pub async fn verify_password_blocking(
    hash: Option<String>,
    password: String,
) -> anyhow::Result<bool> {
    tokio::task::spawn_blocking(move || match hash {
        Some(hash) => verify_password(&hash, &password),
        None => {
            verify_dummy(&password);
            false
        }
    })
    .await
    .map_err(|err| anyhow::anyhow!("verify task failed: {err}"))
}

struct CodeEntry {
    /// Действующий код и его состояние; `None` -- погашен или не выдавался.
    active: Option<ActiveCode>,
    /// Моменты выдач за последний `CODE_WINDOW` (старые первыми).
    issued: VecDeque<Instant>,
}

struct ActiveCode {
    code: String,
    expires_at: Instant,
    attempts_left: u32,
}

/// Коды подтверждения почты в памяти: `почта -> состояние`.
#[derive(Clone, Default)]
pub struct EmailCodes {
    inner: Arc<Mutex<HashMap<String, CodeEntry>>>,
}

impl EmailCodes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Выдаёт код для `email` (уже нормализованной). `None` -- слишком часто:
    /// прошлый код выдан менее `CODE_MIN_INTERVAL` назад или за
    /// `CODE_WINDOW` уже выдано `CODE_MAX_PER_WINDOW` кодов. Новый код
    /// заменяет старый. Заодно убирает устаревшие записи.
    pub fn issue_at(&self, email: &str, now: Instant) -> Option<String> {
        let mut map = self.inner.lock().expect("email codes mutex poisoned");
        map.retain(|_, entry| {
            while entry
                .issued
                .front()
                .is_some_and(|t| now.saturating_duration_since(*t) >= CODE_WINDOW)
            {
                entry.issued.pop_front();
            }
            if entry.active.as_ref().is_some_and(|a| now >= a.expires_at) {
                entry.active = None;
            }
            entry.active.is_some() || !entry.issued.is_empty()
        });

        if let Some(entry) = map.get(email) {
            let too_soon = entry
                .issued
                .back()
                .is_some_and(|t| now.saturating_duration_since(*t) < CODE_MIN_INTERVAL);
            if too_soon || entry.issued.len() >= CODE_MAX_PER_WINDOW {
                return None;
            }
        }

        let code = random_code();
        let entry = map.entry(email.to_string()).or_insert_with(|| CodeEntry {
            active: None,
            issued: VecDeque::new(),
        });
        entry.issued.push_back(now);
        entry.active = Some(ActiveCode {
            code: code.clone(),
            expires_at: now + CODE_TTL,
            attempts_left: CODE_MAX_ATTEMPTS,
        });
        Some(code)
    }

    pub fn issue(&self, email: &str) -> Option<String> {
        self.issue_at(email, Instant::now())
    }

    /// Проверяет код. Нет кода, истёк, попытки кончились -- `false`.
    /// Неверный -- минус попытка и `false`. Верный -- код гасится, `true`.
    pub fn verify_at(&self, email: &str, code: &str, now: Instant) -> bool {
        let mut map = self.inner.lock().expect("email codes mutex poisoned");
        let Some(entry) = map.get_mut(email) else {
            return false;
        };
        let Some(active) = entry.active.as_mut() else {
            return false;
        };
        if now >= active.expires_at || active.attempts_left == 0 {
            entry.active = None;
            return false;
        }
        if active.code == code.trim() {
            entry.active = None;
            return true;
        }
        active.attempts_left -= 1;
        if active.attempts_left == 0 {
            entry.active = None;
        }
        false
    }

    pub fn verify(&self, email: &str, code: &str) -> bool {
        self.verify_at(email, code, Instant::now())
    }
}

fn random_code() -> String {
    let mut rng = rand::rng();
    (0..CODE_DIGITS)
        .map(|_| char::from_digit(rng.random_range(0..10u32), 10).expect("0..10 is a valid digit"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_email_trims_and_lowercases() {
        assert_eq!(
            normalize_email("  Bob@Example.COM \n").as_deref(),
            Some("bob@example.com")
        );
    }

    #[test]
    fn normalize_email_rejects_bad_addresses() {
        let long_local = "a".repeat(250);
        for bad in [
            "",
            "bob",
            "bob@",
            "@example.com",
            "bob@example",
            "a@b@example.com",
            "bo b@example.com",
            "bob@exa mple.com",
            "bob\u{0}@example.com",
            "bob@example.com\u{7}",
            &format!("{long_local}@example.com"),
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn normalize_email_accepts_254_but_not_255_bytes() {
        let at254 = format!("{}@example.com", "a".repeat(254 - "@example.com".len()));
        assert_eq!(at254.len(), 254);
        assert!(normalize_email(&at254).is_some());
        let at255 = format!("a{at254}");
        assert_eq!(normalize_email(&at255), None);
    }

    #[test]
    fn password_length_bounds_count_chars() {
        assert!(!password_is_valid(&"a".repeat(7)));
        assert!(password_is_valid(&"a".repeat(8)));
        assert!(password_is_valid(&"a".repeat(128)));
        assert!(!password_is_valid(&"a".repeat(129)));
        // 8 многобайтовых символов -- это 8 символов, а не 16 байт.
        assert!(password_is_valid(&"я".repeat(8)));
        assert!(!password_is_valid(&"я".repeat(7)));
    }

    #[test]
    fn hash_and_verify_password() {
        let hash = hash_password("correct horse").expect("hash");
        assert!(hash.starts_with("$argon2"));
        assert!(verify_password(&hash, "correct horse"));
        assert!(!verify_password(&hash, "wrong horse"));
        assert!(!verify_password("not a phc string", "correct horse"));
        // Соль случайная.
        assert_ne!(hash, hash_password("correct horse").expect("hash"));
    }

    #[test]
    fn code_is_six_digits_and_single_use() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        let code = codes.issue_at("a@b.co", now).expect("issue");
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert!(!codes.verify_at("other@b.co", &code, now));
        assert!(codes.verify_at("a@b.co", &code, now));
        assert!(!codes.verify_at("a@b.co", &code, now));
    }

    #[test]
    fn wrong_code_costs_an_attempt_and_five_misses_burn_the_code() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        let code = codes.issue_at("a@b.co", now).expect("issue");
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for _ in 0..CODE_MAX_ATTEMPTS {
            assert!(!codes.verify_at("a@b.co", wrong, now));
        }
        assert!(!codes.verify_at("a@b.co", &code, now));
    }

    #[test]
    fn four_misses_still_allow_the_right_code() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        let code = codes.issue_at("a@b.co", now).expect("issue");
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for _ in 0..CODE_MAX_ATTEMPTS - 1 {
            assert!(!codes.verify_at("a@b.co", wrong, now));
        }
        assert!(codes.verify_at("a@b.co", &code, now));
    }

    #[test]
    fn code_expires_after_ttl() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        let code = codes.issue_at("a@b.co", now).expect("issue");
        assert!(!codes.verify_at("a@b.co", &code, now + CODE_TTL));
        let code = codes
            .issue_at("a@b.co", now + CODE_TTL + CODE_MIN_INTERVAL)
            .expect("issue again");
        assert!(codes.verify_at("a@b.co", &code, now + CODE_TTL + CODE_MIN_INTERVAL));
    }

    #[test]
    fn reissue_within_a_minute_is_refused_then_allowed() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        assert!(codes.issue_at("a@b.co", now).is_some());
        assert!(codes
            .issue_at("a@b.co", now + Duration::from_secs(59))
            .is_none());
        assert!(codes
            .issue_at("a@b.co", now + Duration::from_secs(60))
            .is_some());
        // Другая почта не страдает.
        assert!(codes.issue_at("c@d.co", now).is_some());
    }

    #[test]
    fn sixth_code_within_an_hour_is_refused() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        for i in 0..CODE_MAX_PER_WINDOW as u64 {
            assert!(
                codes
                    .issue_at("a@b.co", now + Duration::from_secs(i * 60))
                    .is_some(),
                "code {i} must be issued"
            );
        }
        assert!(codes
            .issue_at("a@b.co", now + Duration::from_secs(5 * 60))
            .is_none());
        // Первая выдача выпала из окна.
        assert!(codes.issue_at("a@b.co", now + CODE_WINDOW).is_some());
    }

    #[test]
    fn new_code_invalidates_the_old_one() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        let old = codes.issue_at("a@b.co", now).expect("issue");
        let later = now + CODE_MIN_INTERVAL;
        let new = codes.issue_at("a@b.co", later).expect("issue");
        if old != new {
            assert!(!codes.verify_at("a@b.co", &old, later));
        }
        assert!(codes.verify_at("a@b.co", &new, later));
    }

    #[test]
    fn stale_entries_are_dropped_on_issue() {
        let codes = EmailCodes::new();
        let now = Instant::now();
        codes.issue_at("a@b.co", now).expect("issue");
        let later = now + CODE_WINDOW + CODE_TTL;
        codes.issue_at("c@d.co", later).expect("issue");
        let map = codes.inner.lock().expect("lock");
        assert!(!map.contains_key("a@b.co"));
        assert!(map.contains_key("c@d.co"));
    }
}
