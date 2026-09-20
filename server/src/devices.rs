//! Device authentication (slice 3.1): decides, for one `HostRegister`,
//! whether the connecting host is a brand-new device (issue it fresh
//! persistent credentials), a known one presenting the credentials it was
//! issued earlier (let it back in under the same `device_id`), or neither
//! (reject it). Secrets are stored only as a SHA-256 hash
//! (`DeviceRow::secret_hash`, see `db.rs`) -- the database never holds a
//! usable copy of a device's secret, only enough to verify one presented to
//! it, the same reasoning a password store would use except there's no KDF
//! here (see `hash_secret`'s doc comment for why a plain hash is enough for
//! this particular secret).

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::registry::random_token;
use proto::signal::DeviceCredentials;

/// Same alphabet/length as `registry::random_id` for `device_id`; `secret`
/// is longer (see `random_secret`'s doc comment).
const DEVICE_ID_LEN: usize = 16;
/// 43 characters from a 36-character alphabet is close to 222 bits of
/// entropy -- see `authenticate`'s module doc comment for why that's enough
/// on its own, without a per-device salt or a slow KDF.
const SECRET_LEN: usize = 43;

/// The outcome of checking the credentials (if any) a host presented in its
/// `HostRegister` against the device store.
pub enum DeviceAuth {
    /// The host presented no credentials: a new device was inserted into
    /// the store. `credentials` must be sent back to the host in
    /// `Registered.device` so it can save them and present them next time.
    Issued {
        device_id: String,
        credentials: DeviceCredentials,
    },
    /// The host presented credentials that matched a known device.
    /// `Registered.device` should be `None` -- the host already has what it
    /// needs saved.
    Known { device_id: String },
    /// The host presented a `device_id` that isn't in the store at all
    /// (e.g. the server's database was recreated). The host should forget
    /// its saved credentials and register again as a new device.
    Unknown,
    /// The host presented a known `device_id` but the wrong secret.
    BadSecret,
}

/// Authenticates one `HostRegister`'s optional device credentials against
/// `db`, issuing new ones when `requested` is `None`. `name` and `now` are
/// used to record/update the device's row (see `Db::insert_device`/
/// `Db::touch_device`).
pub fn authenticate(
    db: &Db,
    requested: Option<&DeviceCredentials>,
    name: &str,
    now: i64,
) -> anyhow::Result<DeviceAuth> {
    let Some(requested) = requested else {
        let device_id = random_token(DEVICE_ID_LEN);
        let secret = random_secret();
        let secret_hash = hash_secret(&secret);
        db.insert_device(&device_id, &secret_hash, name, now)?;
        return Ok(DeviceAuth::Issued {
            device_id: device_id.clone(),
            credentials: DeviceCredentials { device_id, secret },
        });
    };

    let Some(row) = db.device(&requested.device_id)? else {
        return Ok(DeviceAuth::Unknown);
    };

    if hash_secret(&requested.secret) != row.secret_hash {
        return Ok(DeviceAuth::BadSecret);
    }

    db.touch_device(&row.device_id, name, now)?;
    Ok(DeviceAuth::Known {
        device_id: row.device_id,
    })
}

/// A fresh, full-entropy device secret: `SECRET_LEN` characters from the
/// same `a-z0-9` alphabet as `device_id`/`host_id` (~222 bits). This is not
/// a human-chosen password -- there's nothing to guess by trying common
/// values -- so a single SHA-256 pass over it (see `hash_secret`) is already
/// computationally infeasible to reverse; a salt or a slow KDF (bcrypt/
/// argon2, meant to blunt guessing of a *low*-entropy human password) would
/// add cost without adding real protection here.
fn random_secret() -> String {
    random_token(SECRET_LEN)
}

/// SHA-256 of `secret`, base64-encoded. See this module's doc comment and
/// `random_secret`'s for why a plain hash (no salt, no KDF) is appropriate
/// for this particular secret. `pub(crate)` because `owners::authenticate`
/// (slice 3.1) hashes owner tokens the same way -- same reasoning, so the
/// hashing itself isn't duplicated.
pub(crate) fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    BASE64_STANDARD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_credentials_issues_a_new_device_with_hashed_secret_in_db() {
        let db = Db::in_memory().expect("open in-memory db");

        let auth = authenticate(&db, None, "My Mac", 100).expect("authenticate");
        let (device_id, credentials) = match auth {
            DeviceAuth::Issued {
                device_id,
                credentials,
            } => (device_id, credentials),
            _ => panic!("expected Issued"),
        };

        assert_eq!(device_id, credentials.device_id);
        let row = db
            .device(&device_id)
            .expect("query device")
            .expect("device present in db");
        assert_eq!(row.name, "My Mac");
        assert_ne!(row.secret_hash, credentials.secret);
        assert_eq!(row.secret_hash, hash_secret(&credentials.secret));
    }

    #[test]
    fn presenting_issued_credentials_is_known_with_same_device_id() {
        let db = Db::in_memory().expect("open in-memory db");
        let (device_id, credentials) = match authenticate(&db, None, "My Mac", 100).unwrap() {
            DeviceAuth::Issued {
                device_id,
                credentials,
            } => (device_id, credentials),
            _ => panic!("expected Issued"),
        };

        let auth = authenticate(&db, Some(&credentials), "My Mac", 200).expect("authenticate");
        match auth {
            DeviceAuth::Known {
                device_id: known_id,
            } => assert_eq!(known_id, device_id),
            _ => panic!("expected Known"),
        }
    }

    #[test]
    fn unknown_device_id_is_unknown() {
        let db = Db::in_memory().expect("open in-memory db");
        let creds = DeviceCredentials {
            device_id: "no-such-device".to_string(),
            secret: "whatever".to_string(),
        };

        let auth = authenticate(&db, Some(&creds), "My Mac", 100).expect("authenticate");
        assert!(matches!(auth, DeviceAuth::Unknown));
    }

    #[test]
    fn right_device_id_wrong_secret_is_bad_secret() {
        let db = Db::in_memory().expect("open in-memory db");
        let (device_id, credentials) = match authenticate(&db, None, "My Mac", 100).unwrap() {
            DeviceAuth::Issued {
                device_id,
                credentials,
            } => (device_id, credentials),
            _ => panic!("expected Issued"),
        };

        let wrong = DeviceCredentials {
            device_id,
            secret: format!("{}-wrong", credentials.secret),
        };

        let auth = authenticate(&db, Some(&wrong), "My Mac", 200).expect("authenticate");
        assert!(matches!(auth, DeviceAuth::BadSecret));
    }

    #[test]
    fn known_device_updates_name_and_last_seen_at() {
        let db = Db::in_memory().expect("open in-memory db");
        let (device_id, credentials) = match authenticate(&db, None, "My Mac", 100).unwrap() {
            DeviceAuth::Issued {
                device_id,
                credentials,
            } => (device_id, credentials),
            _ => panic!("expected Issued"),
        };

        authenticate(&db, Some(&credentials), "My Mac (renamed)", 200).expect("authenticate");

        let row = db
            .device(&device_id)
            .expect("query device")
            .expect("device present");
        assert_eq!(row.name, "My Mac (renamed)");
        assert_eq!(row.last_seen_at, 200);
    }
}
