//! Passkey (WebAuthn) support (slice 3.7b): registering a passkey for the
//! current owner and signing in with one without any login. The wire side
//! (`SignalMessage::Passkey*`) only ferries JSON strings; everything that
//! knows about `webauthn-rs` lives here, and `ws.rs` just routes.
//!
//! Ceremony state (`PasskeyRegistration`, `DiscoverableAuthentication`) is
//! never persisted: the connection that started a ceremony holds it in
//! memory until the matching `Finish` arrives (or the socket closes).
//!
//! Passkeys are registered as discoverable ("resident") credentials -- that
//! is what lets the login start without the user naming an account. The
//! `webauthn-rs` 0.5.5 `start_passkey_registration` hardwires
//! `require_resident_key(false)` (it only *discourages* residency), so
//! `register_start` flips the two `authenticator_selection` fields on the
//! challenge before it goes to the browser. `finish_passkey_registration`
//! does not look at them again: `WebauthnCore::register_credential`
//! destructures the stored `RegistrationState` with
//! `require_resident_key: _` (webauthn-rs-core 0.5.5, `src/core.rs`), and a
//! resident credential is a normal attestation as far as verification goes.

use proto::signal::PasskeyEntry;

use crate::db::Db;

/// Why a passkey operation failed. `Rejected` is anything the client or the
/// authenticator got wrong (answered with `passkey failed`); `Internal` is a
/// server-side failure (database, serialization -- answered with
/// `internal error`).
#[derive(Debug)]
pub enum PasskeyError {
    Rejected(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for PasskeyError {
    fn from(err: anyhow::Error) -> Self {
        PasskeyError::Internal(err)
    }
}

/// Без фичи `passkeys` (сборка для LAN-хаба, без OpenSSL) церемоний нет:
/// все четыре вызова отвечают `Rejected("passkeys disabled")`, список passkey
/// из SQLite читается как обычно.
#[cfg(feature = "passkeys")]
mod imp {
    use std::sync::Arc;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use webauthn_rs::prelude::{
        CredentialID, DiscoverableKey, Passkey, PublicKeyCredential, RegisterPublicKeyCredential,
        Url, Uuid,
    };
    pub use webauthn_rs::prelude::{DiscoverableAuthentication, PasskeyRegistration};
    use webauthn_rs::{Webauthn, WebauthnBuilder};

    use super::PasskeyError;
    use crate::db::Db;
    use crate::owners::truncate_user_agent;
    use crate::registry::random_token;

    /// The relying party is built once at startup and shared by all connections.
    pub type SharedWebauthn = Arc<Webauthn>;

    const DEFAULT_RP_ID: &str = "rcdesk.app";
    const DEFAULT_ORIGIN: &str = "https://rcdesk.app";
    const RP_NAME: &str = "rcdesk";
    /// WebAuthn `user.name` / `user.displayName`: the account has no name, the
    /// passkey picker just shows this.
    const USER_NAME: &str = "rcdesk";
    const PASSKEY_ID_LEN: usize = 16;

    fn rejected(reason: impl std::fmt::Display) -> PasskeyError {
        PasskeyError::Rejected(reason.to_string())
    }

    /// Builds the relying party. `allow_any_port` is on only for `localhost`
    /// (local development: the dev server's port is not fixed), never in
    /// production.
    pub fn build_webauthn(rp_id: &str, origin: &str) -> anyhow::Result<SharedWebauthn> {
        let origin = Url::parse(origin)?;
        let webauthn = WebauthnBuilder::new(rp_id, &origin)?
            .rp_name(RP_NAME)
            .allow_any_port(rp_id == "localhost")
            .build()?;
        Ok(Arc::new(webauthn))
    }

    /// Reads `RCDESK_WEBAUTHN_RP_ID` (default `rcdesk.app`) and
    /// `RCDESK_WEBAUTHN_ORIGIN` (default `https://rcdesk.app`).
    pub fn webauthn_from_env() -> anyhow::Result<SharedWebauthn> {
        let rp_id = std::env::var("RCDESK_WEBAUTHN_RP_ID").unwrap_or_else(|_| DEFAULT_RP_ID.into());
        let origin =
            std::env::var("RCDESK_WEBAUTHN_ORIGIN").unwrap_or_else(|_| DEFAULT_ORIGIN.into());
        build_webauthn(&rp_id, &origin)
    }

    fn credential_id_string(id: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(id)
    }

    /// The owner's WebAuthn user id, issued on their first registration.
    fn webauthn_user_id(db: &Db, owner_id: &str) -> anyhow::Result<Uuid> {
        if let Some(existing) = db.webauthn_user_id(owner_id)? {
            return Ok(Uuid::parse_str(&existing)?);
        }
        db.set_webauthn_user_id(owner_id, &Uuid::new_v4().to_string())?;
        // Re-read: if two registrations raced, the stored value wins.
        let stored = db
            .webauthn_user_id(owner_id)?
            .ok_or_else(|| anyhow::anyhow!("owner {owner_id} vanished while issuing a user id"))?;
        Ok(Uuid::parse_str(&stored)?)
    }

    /// Starts registering a passkey for `owner_id`. Returns the JSON for
    /// `navigator.credentials.create()` and the state to keep for the finish.
    pub fn register_start(
        webauthn: &Webauthn,
        db: &Db,
        owner_id: &str,
    ) -> Result<(String, PasskeyRegistration), PasskeyError> {
        let user_id = webauthn_user_id(db, owner_id)?;
        let exclude: Vec<CredentialID> = db
            .credential_ids_for_owner(owner_id)?
            .iter()
            .filter_map(|id| URL_SAFE_NO_PAD.decode(id).ok())
            .map(CredentialID::from)
            .collect();
        let exclude = if exclude.is_empty() {
            None
        } else {
            Some(exclude)
        };

        let (challenge, state) = webauthn
            .start_passkey_registration(user_id, USER_NAME, USER_NAME, exclude)
            .map_err(rejected)?;
        // `ResidentKeyRequirement` is not re-exported by `webauthn_rs::prelude`
        // (and `webauthn-rs-proto` is not a direct dependency), so the two
        // fields are set on the serialized challenge instead of the struct.
        let mut options = serde_json::to_value(&challenge)
            .map_err(|e| PasskeyError::Internal(anyhow::Error::new(e)))?;
        let selection = options
            .pointer_mut("/publicKey/authenticatorSelection")
            .and_then(|v| v.as_object_mut())
            .ok_or_else(|| {
                PasskeyError::Internal(anyhow::anyhow!("challenge has no authenticatorSelection"))
            })?;
        selection.insert("residentKey".into(), "required".into());
        selection.insert("requireResidentKey".into(), true.into());
        let options = options.to_string();
        Ok((options, state))
    }

    /// Finishes the registration and stores the passkey for `owner_id`.
    pub fn register_finish(
        webauthn: &Webauthn,
        db: &Db,
        owner_id: &str,
        state: &PasskeyRegistration,
        credential: &str,
        user_agent: &str,
        now: i64,
    ) -> Result<(), PasskeyError> {
        let credential: RegisterPublicKeyCredential =
            serde_json::from_str(credential).map_err(rejected)?;
        let passkey = webauthn
            .finish_passkey_registration(&credential, state)
            .map_err(rejected)?;

        let credential_id = credential_id_string(passkey.cred_id().as_ref());
        if db.passkey_by_credential_id(&credential_id)?.is_some() {
            return Err(rejected("credential is already registered"));
        }
        let passkey_json =
            serde_json::to_string(&passkey).map_err(|e| PasskeyError::Internal(e.into()))?;
        db.insert_passkey(
            &random_token(PASSKEY_ID_LEN),
            owner_id,
            &credential_id,
            &passkey_json,
            truncate_user_agent(user_agent),
            now,
        )?;
        Ok(())
    }

    /// Starts a login without a login name. Returns the JSON for
    /// `navigator.credentials.get()` and the state to keep for the finish.
    pub fn login_start(
        webauthn: &Webauthn,
    ) -> Result<(String, DiscoverableAuthentication), PasskeyError> {
        let (challenge, state) = webauthn
            .start_discoverable_authentication()
            .map_err(rejected)?;
        let options =
            serde_json::to_string(&challenge).map_err(|e| PasskeyError::Internal(e.into()))?;
        Ok((options, state))
    }

    /// Finishes a passkey login and returns the `owner_id` the passkey belongs
    /// to. The user handle the authenticator reports must be the WebAuthn user
    /// id of that very owner, not just any id.
    pub fn login_finish(
        webauthn: &Webauthn,
        db: &Db,
        state: DiscoverableAuthentication,
        credential: &str,
        now: i64,
    ) -> Result<String, PasskeyError> {
        let credential: PublicKeyCredential = serde_json::from_str(credential).map_err(rejected)?;
        let (user_id, credential_id) = webauthn
            .identify_discoverable_authentication(&credential)
            .map_err(rejected)?;

        let (passkey_id, owner_id, passkey_json) = db
            .passkey_by_credential_id(&credential_id_string(credential_id))?
            .ok_or_else(|| rejected("unknown credential"))?;
        if db.webauthn_user_id(&owner_id)?.as_deref() != Some(user_id.to_string().as_str()) {
            return Err(rejected(
                "user handle does not match the credential's owner",
            ));
        }

        let mut passkey: Passkey =
            serde_json::from_str(&passkey_json).map_err(|e| PasskeyError::Internal(e.into()))?;
        let result = webauthn
            .finish_discoverable_authentication(
                &credential,
                state,
                &[DiscoverableKey::from(&passkey)],
            )
            .map_err(rejected)?;

        let updated_json = if passkey.update_credential(&result) == Some(true) {
            Some(serde_json::to_string(&passkey).map_err(|e| PasskeyError::Internal(e.into()))?)
        } else {
            None
        };
        db.touch_passkey(&passkey_id, updated_json.as_deref(), now)?;
        Ok(owner_id)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn webauthn() -> SharedWebauthn {
            build_webauthn("localhost", "http://localhost:5173").expect("build webauthn")
        }

        fn db_with_owner() -> Db {
            let db = Db::in_memory().expect("open in-memory db");
            db.insert_owner("o1", 1).expect("owner");
            db
        }

        #[test]
        fn build_accepts_defaults_and_localhost_and_rejects_mismatch() {
            assert!(build_webauthn(DEFAULT_RP_ID, DEFAULT_ORIGIN).is_ok());
            assert!(build_webauthn("localhost", "http://localhost:5173").is_ok());
            assert!(build_webauthn("rcdesk.app", "https://example.com").is_err());
            assert!(build_webauthn("rcdesk.app", "not a url").is_err());
        }

        #[test]
        fn register_start_requires_resident_key_and_user_verification() {
            let db = db_with_owner();
            let (options, _state) = register_start(&webauthn(), &db, "o1").expect("start");

            let json: serde_json::Value = serde_json::from_str(&options).expect("options are json");
            let public_key = &json["publicKey"];
            let selection = &public_key["authenticatorSelection"];
            assert_eq!(selection["residentKey"], "required");
            assert_eq!(selection["requireResidentKey"], true);
            assert_eq!(selection["userVerification"], "required");
            assert_eq!(public_key["user"]["name"], "rcdesk");
            assert_eq!(public_key["user"]["displayName"], "rcdesk");
            assert_eq!(public_key["rp"]["id"], "localhost");
            assert_eq!(public_key["rp"]["name"], "rcdesk");
            assert!(public_key
                .get("excludeCredentials")
                .is_none_or(|v| v.is_null()));
        }

        #[test]
        fn register_start_excludes_existing_credentials_and_keeps_user_id() {
            let db = db_with_owner();
            let existing = URL_SAFE_NO_PAD.encode([1u8, 2, 3, 4]);
            db.insert_passkey("pk1", "o1", &existing, "{}", "", 1)
                .expect("insert passkey");

            let (first, _) = register_start(&webauthn(), &db, "o1").expect("start");
            let (second, _) = register_start(&webauthn(), &db, "o1").expect("start again");

            let first: serde_json::Value = serde_json::from_str(&first).expect("json");
            let second: serde_json::Value = serde_json::from_str(&second).expect("json");
            let excluded = first["publicKey"]["excludeCredentials"]
                .as_array()
                .expect("excludeCredentials present");
            assert_eq!(excluded.len(), 1);
            assert_eq!(excluded[0]["id"], existing);
            assert_eq!(
                first["publicKey"]["user"]["id"],
                second["publicKey"]["user"]["id"]
            );
            assert_ne!(
                first["publicKey"]["challenge"],
                second["publicKey"]["challenge"]
            );
            assert!(db.webauthn_user_id("o1").expect("uuid").is_some());
        }

        #[test]
        fn register_finish_rejects_garbage_and_wrong_challenge() {
            let db = db_with_owner();
            let webauthn = webauthn();
            let (_, state) = register_start(&webauthn, &db, "o1").expect("start");

            let garbage = register_finish(&webauthn, &db, "o1", &state, "not json", "UA", 5);
            assert!(matches!(garbage, Err(PasskeyError::Rejected(_))));

            let bogus = r#"{"id":"AQID","rawId":"AQID","type":"public-key",
                "response":{"attestationObject":"AQID","clientDataJSON":"AQID"},
                "extensions":{}}"#;
            let wrong = register_finish(&webauthn, &db, "o1", &state, bogus, "UA", 5);
            assert!(matches!(wrong, Err(PasskeyError::Rejected(_))));
            assert!(db.passkeys_for_owner("o1").expect("list").is_empty());
        }

        #[test]
        fn login_start_gives_options_with_user_verification_and_no_owner() {
            let (options, _state) = login_start(&webauthn()).expect("start");

            let json: serde_json::Value = serde_json::from_str(&options).expect("options are json");
            assert_eq!(json["publicKey"]["userVerification"], "required");
            assert_eq!(json["publicKey"]["rpId"], "localhost");
            assert!(json["publicKey"]["challenge"].is_string());
        }

        #[test]
        fn login_finish_rejects_garbage_and_unknown_credential() {
            let db = db_with_owner();
            let webauthn = webauthn();

            let (_, state) = login_start(&webauthn).expect("start");
            let garbage = login_finish(&webauthn, &db, state, "not json", 5);
            assert!(matches!(garbage, Err(PasskeyError::Rejected(_))));

            let (_, state) = login_start(&webauthn).expect("start");
            let unknown = r#"{"id":"AQID","rawId":"AQID","type":"public-key",
                "response":{"authenticatorData":"AQID","clientDataJSON":"AQID",
                            "signature":"AQID","userHandle":"AQIDBAUGBwgJCgsMDQ4PEA"},
                "extensions":{}}"#;
            let unknown = login_finish(&webauthn, &db, state, unknown, 5);
            assert!(matches!(unknown, Err(PasskeyError::Rejected(_))));
        }
    }
}

#[cfg(not(feature = "passkeys"))]
mod imp {
    use std::sync::Arc;

    use super::PasskeyError;
    use crate::db::Db;

    const DISABLED: &str = "passkeys disabled";

    /// Маркер вместо relying party: без фичи `passkeys` WebAuthn нет.
    pub struct DisabledWebauthn;

    /// Пустая заглушка состояния регистрации (церемоний без фичи не бывает).
    pub struct PasskeyRegistration;

    /// Пустая заглушка состояния входа.
    pub struct DiscoverableAuthentication;

    pub type SharedWebauthn = Arc<DisabledWebauthn>;

    /// Без фичи `passkeys` ничего не строит и не читает env.
    pub fn build_webauthn(_rp_id: &str, _origin: &str) -> anyhow::Result<SharedWebauthn> {
        Ok(Arc::new(DisabledWebauthn))
    }

    /// Без фичи `passkeys` ничего не читает из env.
    pub fn webauthn_from_env() -> anyhow::Result<SharedWebauthn> {
        Ok(Arc::new(DisabledWebauthn))
    }

    pub fn register_start(
        _webauthn: &DisabledWebauthn,
        _db: &Db,
        _owner_id: &str,
    ) -> Result<(String, PasskeyRegistration), PasskeyError> {
        Err(PasskeyError::Rejected(DISABLED.into()))
    }

    pub fn register_finish(
        _webauthn: &DisabledWebauthn,
        _db: &Db,
        _owner_id: &str,
        _state: &PasskeyRegistration,
        _credential: &str,
        _user_agent: &str,
        _now: i64,
    ) -> Result<(), PasskeyError> {
        Err(PasskeyError::Rejected(DISABLED.into()))
    }

    pub fn login_start(
        _webauthn: &DisabledWebauthn,
    ) -> Result<(String, DiscoverableAuthentication), PasskeyError> {
        Err(PasskeyError::Rejected(DISABLED.into()))
    }

    pub fn login_finish(
        _webauthn: &DisabledWebauthn,
        _db: &Db,
        _state: DiscoverableAuthentication,
        _credential: &str,
        _now: i64,
    ) -> Result<String, PasskeyError> {
        Err(PasskeyError::Rejected(DISABLED.into()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_ceremony_is_rejected_as_disabled() {
            let webauthn = build_webauthn("localhost", "http://localhost:5173").expect("build");
            let db = Db::in_memory().expect("open in-memory db");
            let rejected = |r: Result<(), PasskeyError>| match r {
                Err(PasskeyError::Rejected(reason)) => assert_eq!(reason, "passkeys disabled"),
                _ => panic!("expected Rejected(passkeys disabled)"),
            };
            rejected(register_start(&webauthn, &db, "o1").map(|_| ()));
            rejected(login_start(&webauthn).map(|_| ()));
            rejected(register_finish(
                &webauthn,
                &db,
                "o1",
                &PasskeyRegistration,
                "{}",
                "UA",
                1,
            ));
            rejected(login_finish(&webauthn, &db, DiscoverableAuthentication, "{}", 1).map(|_| ()));
        }
    }
}

pub use imp::*;

/// The owner's passkeys for the `Passkeys` message.
pub fn passkey_entries(db: &Db, owner_id: &str) -> anyhow::Result<Vec<PasskeyEntry>> {
    Ok(db
        .passkeys_for_owner(owner_id)?
        .into_iter()
        .map(|row| PasskeyEntry {
            id: row.passkey_id,
            user_agent: row.user_agent,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
        })
        .collect())
}
