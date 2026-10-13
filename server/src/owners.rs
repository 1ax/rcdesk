//! Owner authentication (slice 3.1). In this slice an "owner" is just a
//! browser holding a bearer token -- there's no account system yet. The
//! token is stored only as a hash (`Db::insert_token`/`Db::token_by_hash`,
//! same scheme as a device's secret -- see `devices::hash_secret`, reused
//! here rather than duplicated). Losing the token means losing the owner's
//! list of paired devices: there's nothing else identifying the owner, so a
//! fresh token means a fresh, empty list. The devices themselves aren't
//! affected -- they simply get re-paired the next time their owner joins by
//! PIN. Since slice 3.7a an owner can hold several tokens (one per browser,
//! each revocable); `authenticate` also reports which token matched.

use crate::db::Db;
use crate::devices::hash_secret;
use crate::registry::random_token;

/// Same length as `devices::DEVICE_ID_LEN` -- there's no reason for it to
/// differ, but it's not shared as a constant since the two ids identify
/// unrelated kinds of things.
const OWNER_ID_LEN: usize = 16;
/// Same length/entropy reasoning as `devices::SECRET_LEN`.
const TOKEN_LEN: usize = 43;
/// Идентификатор токена (его видно владельцу в списке браузеров, не секрет).
const TOKEN_ID_LEN: usize = 16;
/// Максимальная длина сохраняемого `User-Agent`, в символах.
const MAX_USER_AGENT_CHARS: usize = 256;

/// Обрезает строку до `max_chars` символов по границе char.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Обрезает `User-Agent` до `MAX_USER_AGENT_CHARS` символов по границе char.
pub fn truncate_user_agent(user_agent: &str) -> &str {
    truncate_chars(user_agent, MAX_USER_AGENT_CHARS)
}

/// Опознаёт владельца по присланному токену или заводит нового.
/// Возвращает `(owner_id, token_id, token)`: токен тот же, если присланный
/// опознан (при этом обновляются `last_used_at` и `user_agent`), иначе
/// свежевыданный — его надо отдать клиенту в `Authenticated`.
pub fn authenticate(
    db: &Db,
    token: Option<&str>,
    user_agent: &str,
    now: i64,
) -> anyhow::Result<(String, String, String)> {
    let user_agent = truncate_user_agent(user_agent);
    if let Some(token) = token {
        if let Some((owner_id, token_id)) = db.token_by_hash(&hash_secret(token))? {
            db.touch_token(&token_id, user_agent, now)?;
            return Ok((owner_id, token_id, token.to_string()));
        }
    }

    let owner_id = random_token(OWNER_ID_LEN);
    let token_id = random_token(TOKEN_ID_LEN);
    let token = random_token(TOKEN_LEN);
    db.insert_owner(&owner_id, now)?;
    db.insert_token(&owner_id, &token_id, &hash_secret(&token), user_agent, now)?;
    Ok((owner_id, token_id, token))
}

/// Выдаёт существующему владельцу новый токен (вход по passkey, слайс
/// 3.7b). Возвращает `(token_id, token)`; токен надо отдать клиенту.
pub fn issue_token(
    db: &Db,
    owner_id: &str,
    user_agent: &str,
    now: i64,
) -> anyhow::Result<(String, String)> {
    let token_id = random_token(TOKEN_ID_LEN);
    let token = random_token(TOKEN_LEN);
    db.insert_token(
        owner_id,
        &token_id,
        &hash_secret(&token),
        truncate_user_agent(user_agent),
        now,
    )?;
    Ok((token_id, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_cuts_on_char_boundary() {
        let s = "привет😀мир";
        assert_eq!(truncate_chars(s, 6), "привет");
        assert_eq!(truncate_chars(s, 7), "привет😀");
        assert_eq!(truncate_chars(s, 100), s);
        assert_eq!(truncate_chars(s, 0), "");
    }

    #[test]
    fn no_token_creates_owner_with_hashed_token_in_db() {
        let db = Db::in_memory().expect("open in-memory db");

        let (owner_id, token_id, token) = authenticate(&db, None, "UA", 100).expect("authenticate");

        assert!(!token.is_empty());
        assert_eq!(
            db.token_by_hash(&hash_secret(&token))
                .expect("query token by hash"),
            Some((owner_id, token_id))
        );
        assert_eq!(
            db.token_by_hash(&token).expect("query token by raw token"),
            None
        );
    }

    #[test]
    fn two_authentications_without_token_give_different_owners() {
        let db = Db::in_memory().expect("open in-memory db");
        let (owner_a, token_id_a, token_a) = authenticate(&db, None, "", 100).expect("auth a");
        let (owner_b, token_id_b, token_b) = authenticate(&db, None, "", 100).expect("auth b");

        assert_ne!(owner_a, owner_b);
        assert_ne!(token_id_a, token_id_b);
        assert_ne!(token_a, token_b);
    }

    #[test]
    fn issued_token_is_recognized_and_usage_is_updated() {
        let db = Db::in_memory().expect("open in-memory db");
        let (owner_id, token_id, token) =
            authenticate(&db, None, "UA-old", 100).expect("authenticate");

        let (owner_id2, token_id2, token2) =
            authenticate(&db, Some(&token), "UA-new", 200).expect("authenticate with issued token");

        assert_eq!(owner_id2, owner_id);
        assert_eq!(token_id2, token_id);
        assert_eq!(token2, token);

        let tokens = db.tokens_for_owner(&owner_id).expect("tokens");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].user_agent, "UA-new");
        assert_eq!(tokens[0].created_at, 100);
        assert_eq!(tokens[0].last_used_at, 200);
    }

    #[test]
    fn unknown_token_issues_a_new_owner_and_a_new_token() {
        let db = Db::in_memory().expect("open in-memory db");
        let (owner_id, _, token) = authenticate(&db, None, "", 100).expect("authenticate");

        let (owner_id2, _, token2) = authenticate(&db, Some("not-a-real-token"), "", 200)
            .expect("authenticate with unknown token");

        assert_ne!(owner_id2, owner_id);
        assert_ne!(token2, token);
    }

    #[test]
    fn user_agent_is_truncated_on_char_boundary() {
        let db = Db::in_memory().expect("open in-memory db");
        let long = "я".repeat(300);
        let (owner_id, _, _) = authenticate(&db, None, &long, 100).expect("authenticate");

        let tokens = db.tokens_for_owner(&owner_id).expect("tokens");
        assert_eq!(tokens[0].user_agent.chars().count(), 256);
    }

    #[test]
    fn issue_token_adds_a_second_working_token_to_the_same_owner() {
        let db = Db::in_memory().expect("open in-memory db");
        let (owner_id, _, first) = authenticate(&db, None, "UA1", 100).expect("authenticate");

        let (token_id, second) = issue_token(&db, &owner_id, "UA2", 200).expect("issue");

        assert_ne!(first, second);
        assert_eq!(
            db.token_by_hash(&hash_secret(&second)).expect("lookup"),
            Some((owner_id.clone(), token_id))
        );
        assert_eq!(db.tokens_for_owner(&owner_id).expect("tokens").len(), 2);
    }
}
