//! Owner authentication (slice 3.1). In this slice an "owner" is just a
//! browser holding a bearer token -- there's no account system yet. The
//! token is stored only as a hash (`Db::insert_owner`/`Db::owner_by_token_hash`,
//! same scheme as a device's secret -- see `devices::hash_secret`, reused
//! here rather than duplicated). Losing the token means losing the owner's
//! list of paired devices: there's nothing else identifying the owner, so a
//! fresh token means a fresh, empty list. The devices themselves aren't
//! affected -- they simply get re-paired the next time their owner joins by
//! PIN.

use crate::db::Db;
use crate::devices::hash_secret;
use crate::registry::random_token;

/// Same length as `devices::DEVICE_ID_LEN` -- there's no reason for it to
/// differ, but it's not shared as a constant since the two ids identify
/// unrelated kinds of things.
const OWNER_ID_LEN: usize = 16;
/// Same length/entropy reasoning as `devices::SECRET_LEN`.
const TOKEN_LEN: usize = 43;

/// Опознаёт владельца по присланному токену или заводит нового.
/// Возвращает `(owner_id, token)`: токен тот же, если присланный опознан,
/// иначе свежевыданный — его надо отдать клиенту в `Authenticated`.
pub fn authenticate(db: &Db, token: Option<&str>, now: i64) -> anyhow::Result<(String, String)> {
    if let Some(token) = token {
        if let Some(owner_id) = db.owner_by_token_hash(&hash_secret(token))? {
            return Ok((owner_id, token.to_string()));
        }
    }

    let owner_id = random_token(OWNER_ID_LEN);
    let token = random_token(TOKEN_LEN);
    db.insert_owner(&owner_id, &hash_secret(&token), now)?;
    Ok((owner_id, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_token_creates_owner_with_hashed_token_in_db() {
        let db = Db::in_memory().expect("open in-memory db");

        let (owner_id, token) = authenticate(&db, None, 100).expect("authenticate");

        assert!(!token.is_empty());
        assert_eq!(
            db.owner_by_token_hash(&hash_secret(&token))
                .expect("query owner by token hash"),
            Some(owner_id)
        );
        assert_eq!(
            db.owner_by_token_hash(&token)
                .expect("query owner by raw token"),
            None
        );
    }

    #[test]
    fn issued_token_is_recognized_with_same_owner_id_and_token() {
        let db = Db::in_memory().expect("open in-memory db");
        let (owner_id, token) = authenticate(&db, None, 100).expect("authenticate");

        let (owner_id2, token2) =
            authenticate(&db, Some(&token), 200).expect("authenticate with issued token");

        assert_eq!(owner_id2, owner_id);
        assert_eq!(token2, token);
    }

    #[test]
    fn unknown_token_issues_a_new_owner_and_a_new_token() {
        let db = Db::in_memory().expect("open in-memory db");
        let (owner_id, token) = authenticate(&db, None, 100).expect("authenticate");

        let (owner_id2, token2) = authenticate(&db, Some("not-a-real-token"), 200)
            .expect("authenticate with unknown token");

        assert_ne!(owner_id2, owner_id);
        assert_ne!(token2, token);
    }
}
