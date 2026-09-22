//! Host access password via OPAQUE (slice 3.2, D-series). Anyone who wants
//! to control this host through the browser must know a password the owner
//! sets locally on the host; the host never stores the password itself,
//! only an OPAQUE registration record (`ServerSetup` + `ServerRegistration`,
//! roughly "the password's salted hash", though OPAQUE never sees the
//! plaintext travel further than the browser's own memory even during
//! registration).
//!
//! This module (slice 3.2b) covers everything that runs entirely on the
//! host, with no network protocol yet: the on-disk record (`AccessStore`,
//! `access.json` next to `device.json` -- see `device.rs`'s module doc
//! comment for the directory), local registration (the host plays both
//! OPAQUE roles once, at `password set` time), and the server half of
//! login (`login_start`/`login_finish`), which slice 3.2c will wire up to
//! the signaling `HostLogin*` messages. The client (browser) half of login
//! is `@serenity-kit/opaque` in the web client, added in slice 3.2d.
//!
//! # Cipher suite
//!
//! The cipher suite and key-stretching function below are copied
//! byte-for-byte from `@serenity-kit/opaque`'s own Rust source (it wraps
//! `opaque-ke` 4.0 in WASM) -- see the crate's `src/lib.rs` on
//! <https://github.com/serenity-kit/opaque>. This isn't a matter of taste:
//! the browser client and this host run the *same* OPAQUE instance from two
//! ends of one protocol run, so every primitive, identifier and KSF
//! parameter must match exactly or the two sides derive different keys and
//! every login fails.

use opaque_ke::ciphersuite::CipherSuite;
use opaque_ke::errors::InternalError;
use opaque_ke::generic_array::{ArrayLength, GenericArray};
use opaque_ke::ksf::Ksf;
use opaque_ke::{
    ClientRegistration, ClientRegistrationFinishParameters, CredentialFinalization,
    CredentialRequest, Identifiers, ServerLogin, ServerLoginParameters, ServerRegistration,
    ServerSetup,
};
use std::path::{Path, PathBuf};

// `opaque-ke` 4.0.1 is generic over `rand_core` 0.6's `RngCore + CryptoRng`
// (it pins `rand = "0.8", default-features = false` for those traits,
// itself a major version below this workspace's own `rand = "0.10"`, used
// elsewhere in this crate e.g. `transport` -- their `RngCore`/`CryptoRng`
// are different, incompatible traits). `opaque_ke::rand` re-exports that
// `rand` 0.8, but without its `getrandom` feature there's no `OsRng` on it
// to use (see host/Cargo.toml's comment on the `rand_core` dependency
// below), so this module gets `OsRng` from `rand_core` 0.6 directly --
// the exact same trait `impl`s `rand` 0.8 itself re-exports, just reached
// through an edge this crate controls the features of.
use rand_core::OsRng;

use anyhow::Context as _;

/// The OPAQUE cipher suite this host (and, from slice 3.2d, the browser
/// client through `@serenity-kit/opaque`) speaks: Ristretto255 for both the
/// OPRF and the key exchange group, TripleDh with SHA-512, and the
/// memory-constrained Argon2id KSF below. See this module's doc comment.
pub struct RcdeskCipherSuite;

impl CipherSuite for RcdeskCipherSuite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, sha2::Sha512>;
    type Ksf = CustomKsf;
}

/// `@serenity-kit/opaque`'s "memory-constrained" Argon2id defaults -- must
/// match byte-for-byte (see this module's doc comment). 64 MiB / 3 passes /
/// 4 lanes, deliberately heavier than an interactive login is usually
/// tuned, because it only ever runs once per `password set` (registration)
/// or once per login attempt, both already gated by a human typing a
/// password.
const ARGON2_M_COST_KIB: u32 = 65536;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 4;

/// The `credential_identifier` OPAQUE binds the registration/login record
/// to. rcdesk has exactly one password for exactly one owner account per
/// host, so this is a fixed constant rather than a username -- the browser
/// client (slice 3.2d) calls `@serenity-kit/opaque` without an identifier
/// either, so both sides must agree on *some* fixed value, and this is it.
const CREDENTIAL_ID: &[u8] = b"rcdesk-owner";

/// The key-stretching function OPAQUE runs the password through before
/// deriving key material -- Argon2id with `@serenity-kit/opaque`'s
/// "memory-constrained" parameters (see the constants above). `Default`
/// must produce a `CustomKsf` with those exact parameters: `opaque-ke`'s
/// `Ksf` trait requires `Default`, and both registration and login
/// construct this type through it (`ClientRegistrationFinishParameters`,
/// `ClientLoginFinishParameters`), so a `#[derive(Default)]` here would
/// silently fall back to `argon2`'s own defaults instead and break
/// interop with the browser client.
pub struct CustomKsf {
    argon: argon2::Argon2<'static>,
}

impl CustomKsf {
    fn with_documented_params() -> Self {
        // The three constants above are fixed, in-range Argon2 parameters
        // (8*p_cost <= m_cost <= u32::MAX, 1 <= t_cost, 1 <= p_cost <=
        // 2^24-1): `ParamsBuilder::build` can only fail on out-of-range
        // input, which these never are.
        let params = argon2::ParamsBuilder::new()
            .m_cost(ARGON2_M_COST_KIB)
            .t_cost(ARGON2_T_COST)
            .p_cost(ARGON2_P_COST)
            .build()
            .expect("slice 3.2b: ARGON2_*_COST constants above are always in range");
        Self {
            argon: argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params),
        }
    }
}

impl Default for CustomKsf {
    fn default() -> Self {
        Self::with_documented_params()
    }
}

impl Ksf for CustomKsf {
    fn hash<L: ArrayLength<u8>>(
        &self,
        input: GenericArray<u8, L>,
    ) -> Result<GenericArray<u8, L>, InternalError> {
        let mut output = GenericArray::default();
        self.argon
            .hash_password_into(&input, &[0; argon2::RECOMMENDED_SALT_LEN], &mut output)
            .map_err(|_| InternalError::KsfError)?;
        Ok(output)
    }
}

/// A completed OPAQUE registration: the server setup (the host's own
/// long-term key material) and the server-side registration record (the
/// OPAQUE equivalent of a salted password hash) produced once, at `password
/// set` time. Neither field holds the plaintext password.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessRecord {
    server_setup: Vec<u8>,
    registration: Vec<u8>,
}

// Deliberately no `#[derive(Debug)]`: these bytes are the OPAQUE analog of
// a password hash, and the default derive would print them in full on any
// stray `{:?}` log line.
impl std::fmt::Debug for AccessRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessRecord").finish_non_exhaustive()
    }
}

/// The on-disk shape of `access.json`: the same two fields as
/// `AccessRecord`, but base64 (URL-safe, no padding -- the same alphabet
/// `@serenity-kit/opaque` uses for its own encoded values) instead of raw
/// bytes, since `serde_json` has no native byte-string type.
#[derive(serde::Serialize, serde::Deserialize)]
struct AccessRecordFile {
    server_setup: String,
    registration: String,
}

impl From<&AccessRecord> for AccessRecordFile {
    fn from(record: &AccessRecord) -> Self {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        Self {
            server_setup: engine.encode(&record.server_setup),
            registration: engine.encode(&record.registration),
        }
    }
}

impl TryFrom<AccessRecordFile> for AccessRecord {
    type Error = base64::DecodeError;

    fn try_from(file: AccessRecordFile) -> Result<Self, Self::Error> {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        Ok(Self {
            server_setup: engine.decode(file.server_setup)?,
            registration: engine.decode(file.registration)?,
        })
    }
}

/// The `access.json` file in the agent's data directory (the same
/// directory as `device.json` -- `agent::paths::data_dir()`). Absent means
/// "no password set", the default state.
pub struct AccessStore {
    path: PathBuf,
}

impl AccessStore {
    /// `dir/access.json`.
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("access.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved record, or `None` when there isn't a usable one: the file
    /// doesn't exist, isn't readable, or isn't the JSON this store writes.
    /// A corrupt file is deliberately `None` plus a `tracing::warn!` rather
    /// than an `Err`, matching `device::DeviceStore::load`.
    pub fn load(&self) -> Option<AccessRecord> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "failed to read access record file, treating as no password set"
                );
                return None;
            }
        };
        let file = match serde_json::from_slice::<AccessRecordFile>(&bytes) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "access record file is not valid JSON, treating as no password set"
                );
                return None;
            }
        };
        match AccessRecord::try_from(file) {
            Ok(record) => Some(record),
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "access record file has invalid base64, treating as no password set"
                );
                None
            }
        }
    }

    /// Writes `record` atomically with mode `0600` on Unix -- see
    /// `fsutil::write_private_atomic`.
    pub fn save(&self, record: &AccessRecord) -> anyhow::Result<()> {
        let file = AccessRecordFile::from(record);
        let json = serde_json::to_vec_pretty(&file)?;
        crate::fsutil::write_private_atomic(&self.path, &json)?;
        Ok(())
    }

    /// Removes the access record file. Its absence is not an error.
    pub fn clear(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

/// Runs a full local OPAQUE registration for `password`: the host plays
/// both the client and server roles (there is no network round trip --
/// this is `rcdesk-host password set`, run at the keyboard of the machine
/// being set up), and returns the resulting `AccessRecord` to persist.
/// Runs the ~64 MiB Argon2id KSF once, which is expected to take on the
/// order of a second; acceptable since this only happens when the owner
/// sets or changes the password.
pub fn register(password: &str) -> anyhow::Result<AccessRecord> {
    let mut rng = OsRng;
    let ksf = CustomKsf::default();

    let server_setup = ServerSetup::<RcdeskCipherSuite>::new(&mut rng);

    let client_start =
        ClientRegistration::<RcdeskCipherSuite>::start(&mut rng, password.as_bytes())
            .context("opaque client registration start failed")?;
    let server_start = ServerRegistration::<RcdeskCipherSuite>::start(
        &server_setup,
        client_start.message,
        CREDENTIAL_ID,
    )
    .context("opaque server registration start failed")?;
    let client_finish = client_start
        .state
        .finish(
            &mut rng,
            password.as_bytes(),
            server_start.message,
            ClientRegistrationFinishParameters::new(Identifiers::default(), Some(&ksf)),
        )
        .context("opaque client registration finish failed")?;
    let server_registration =
        ServerRegistration::<RcdeskCipherSuite>::finish(client_finish.message);

    Ok(AccessRecord {
        server_setup: server_setup.serialize().to_vec(),
        registration: server_registration.serialize().to_vec(),
    })
}

/// A 64-byte OPAQUE session key. Deliberately opaque (pun noted): no
/// `Deref`/`AsRef` that would make it easy to log or otherwise leak by
/// accident, and `Debug` never prints its contents. `opaque-ke` doesn't
/// re-export `zeroize` from its public API (see this crate's deviation
/// note for slice 3.2b), so this can't be zeroized on drop without adding
/// a new direct dependency; that's out of this sub-step's scope.
pub struct SessionKey(Vec<u8>);

impl SessionKey {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionKey").field(&"..").finish()
    }
}

/// The server side of one in-progress OPAQUE login, held between
/// `login_start` and `login_finish` (slice 3.2c will keep this in the
/// session's server-side state between the two `HostLogin*` signaling
/// messages).
pub struct LoginServer {
    state: ServerLogin<RcdeskCipherSuite>,
}

/// The server half of OPAQUE login's first step: given the persisted
/// `AccessRecord` and the client's serialized `CredentialRequest`, produces
/// the state to keep for `login_finish` and the serialized
/// `CredentialResponse` to send back to the browser client.
pub fn login_start(
    record: &AccessRecord,
    credential_request: &[u8],
) -> anyhow::Result<(LoginServer, Vec<u8>)> {
    let server_setup = ServerSetup::<RcdeskCipherSuite>::deserialize(&record.server_setup)
        .context("failed to deserialize server setup")?;
    let registration = ServerRegistration::<RcdeskCipherSuite>::deserialize(&record.registration)
        .context("failed to deserialize server registration")?;
    let request = CredentialRequest::<RcdeskCipherSuite>::deserialize(credential_request)
        .context("failed to deserialize credential request")?;

    let mut rng = OsRng;
    let result = ServerLogin::<RcdeskCipherSuite>::start(
        &mut rng,
        &server_setup,
        Some(registration),
        request,
        CREDENTIAL_ID,
        ServerLoginParameters {
            context: None,
            identifiers: Identifiers::default(),
        },
    )
    .context("opaque server login start failed")?;

    let response_bytes = result.message.serialize().to_vec();
    Ok((
        LoginServer {
            state: result.state,
        },
        response_bytes,
    ))
}

impl LoginServer {
    /// The server half of OPAQUE login's second step: given the client's
    /// serialized `CredentialFinalization`, checks the client's proof of
    /// knowledge of the password and returns the session key on success.
    /// Fails (without leaking *why* beyond "invalid login") if the
    /// finalization doesn't check out -- see this module's tests for where
    /// a wrong password is actually caught.
    pub fn login_finish(self, credential_finalization: &[u8]) -> anyhow::Result<SessionKey> {
        let finalization =
            CredentialFinalization::<RcdeskCipherSuite>::deserialize(credential_finalization)
                .context("failed to deserialize credential finalization")?;
        let result = self
            .state
            .finish(finalization, ServerLoginParameters::default())
            .context("opaque server login finish failed")?;
        Ok(SessionKey(result.session_key.to_vec()))
    }
}

/// Minimum length chosen as a basic typo/empty-string guard, not a
/// strength policy -- rcdesk has no account lockout or rate-limit story
/// yet (that's separate debt), so this is deliberately not trying to be a
/// password-strength checker.
const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 128;

/// Validates a candidate access password before it's registered. Returns a
/// Russian-language message describing the problem on failure -- the host
/// UI is Russian throughout (slice 2.6+).
pub fn validate_password(password: &str) -> Result<(), String> {
    if password.trim_start() != password || password.trim_end() != password {
        return Err("Пароль не должен начинаться или заканчиваться пробелом".to_string());
    }
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(format!(
            "Пароль должен содержать не менее {MIN_PASSWORD_LEN} символов"
        ));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(format!(
            "Пароль должен содержать не более {MAX_PASSWORD_LEN} символов"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opaque_ke::{ClientLogin, ClientLoginFinishParameters, CredentialResponse};
    use std::path::PathBuf;

    /// A fresh, per-test directory under the system temp dir, mirroring
    /// `device::tests::scratch_dir`.
    fn scratch_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rcdesk-host-access-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn register_then_login_with_the_same_password_yields_equal_session_keys() {
        let password = "correct horse battery staple";
        let record = register(password).unwrap();

        let mut rng = OsRng;
        let client_start =
            ClientLogin::<RcdeskCipherSuite>::start(&mut rng, password.as_bytes()).unwrap();
        let request_bytes = client_start.message.serialize();

        let (login_server, response_bytes) = login_start(&record, &request_bytes).unwrap();
        let response =
            CredentialResponse::<RcdeskCipherSuite>::deserialize(&response_bytes).unwrap();

        let ksf = CustomKsf::default();
        let client_finish = client_start
            .state
            .finish(
                &mut rng,
                password.as_bytes(),
                response,
                ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
            )
            .unwrap();

        let server_session = login_server
            .login_finish(&client_finish.message.serialize())
            .unwrap();

        assert_eq!(server_session.as_bytes().len(), 64);
        assert_eq!(server_session.as_bytes(), &client_finish.session_key[..]);
    }

    /// OPAQUE catches a wrong password on the *client* side, inside
    /// `ClientLogin::finish`: the OPRF-derived key computed from the wrong
    /// password fails to open the envelope the server returned in its
    /// `CredentialResponse`, so `finish` itself returns
    /// `InvalidLoginError` (see `opaque_ke::errors::ProtocolError`) and no
    /// `CredentialFinalization` is ever produced -- the server-side
    /// `LoginServer` here never even sees a finalization to reject.
    #[test]
    fn login_with_a_wrong_password_fails() {
        let record = register("correct horse battery staple").unwrap();

        let mut rng = OsRng;
        let client_start =
            ClientLogin::<RcdeskCipherSuite>::start(&mut rng, b"wrong password").unwrap();
        let request_bytes = client_start.message.serialize();

        let (_login_server, response_bytes) = login_start(&record, &request_bytes).unwrap();
        let response =
            CredentialResponse::<RcdeskCipherSuite>::deserialize(&response_bytes).unwrap();

        let ksf = CustomKsf::default();
        let result = client_start.state.finish(
            &mut rng,
            b"wrong password",
            response,
            ClientLoginFinishParameters::new(None, Identifiers::default(), Some(&ksf)),
        );

        assert!(result.is_err());
    }

    #[test]
    fn access_store_round_trips_and_clears() {
        let dir = scratch_dir("round-trip");
        let store = AccessStore::new(&dir);
        let record = register("correct horse battery staple").unwrap();

        store.save(&record).unwrap();
        assert_eq!(store.load(), Some(record));

        store.clear().unwrap();
        assert_eq!(store.load(), None);
        store.clear().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_with_garbage_instead_of_json_returns_none() {
        let dir = scratch_dir("garbage");
        std::fs::create_dir_all(&dir).unwrap();
        let store = AccessStore::new(&dir);
        std::fs::write(store.path(), b"not json at all").unwrap();

        assert_eq!(store.load(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_the_file_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("perms");
        let store = AccessStore::new(&dir);
        let record = register("correct horse battery staple").unwrap();
        store.save(&record).unwrap();

        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_password_rules() {
        assert!(validate_password("correct horse").is_ok());
        assert!(validate_password("short7").is_err());
        assert!(validate_password(" leadingspace1").is_err());
        assert!(validate_password("trailingspace1 ").is_err());
        assert!(validate_password(&"a".repeat(128)).is_ok());
        assert!(validate_password(&"a".repeat(129)).is_err());
    }

    /// Fixes `CustomKsf`'s `Default` impl to the documented parameters:
    /// a `#[derive(Default)]` would silently use `argon2`'s own defaults
    /// instead, which don't match `@serenity-kit/opaque` and would break
    /// interop with the browser client (slice 3.2d) without any local
    /// test noticing.
    #[test]
    fn custom_ksf_default_matches_documented_params() {
        use opaque_ke::generic_array::typenum::U64;

        let default_ksf = CustomKsf::default();
        let params = argon2::ParamsBuilder::new()
            .m_cost(ARGON2_M_COST_KIB)
            .t_cost(ARGON2_T_COST)
            .p_cost(ARGON2_P_COST)
            .build()
            .unwrap();
        let explicit_ksf = CustomKsf {
            argon: argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params),
        };

        let input = GenericArray::<u8, U64>::default();
        let a = default_ksf.hash(input).unwrap();
        let b = explicit_ksf.hash(input).unwrap();
        assert_eq!(a, b);
    }
}
