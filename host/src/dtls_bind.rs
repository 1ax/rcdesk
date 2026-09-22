//! Binds an SDP offer/answer to the OPAQUE session key from login (slice
//! 3.2e): defense against a man-in-the-middle at the signaling server, who
//! can see and rewrite `Offer`/`Answer` but (without the password) never
//! learns the OPAQUE `session_key` both sides derived during login.
//!
//! The scheme, identical on the host (here) and the browser client
//! (`web/src/dtlsBind.ts` -- both sides must produce byte-identical tags or
//! every session with a password set would fail to connect):
//!
//! 1. Extract the DTLS certificate fingerprint from the SDP: the first
//!    `a=fingerprint:<alg> <hex>` line (session level or the first
//!    m-section, whichever comes first), normalized to `"<alg> <HEX>"` --
//!    algorithm lowercased, hex uppercased, one space between them.
//! 2. Build the message `"rcdesk-dtls-v1|" + role + "|" + fingerprint`,
//!    where `role` is `"offer"` (the host's own fingerprint, sent in
//!    `Offer.auth`) or `"answer"` (the client's fingerprint, sent in
//!    `Answer.auth`).
//! 3. Tag = `HMAC-SHA256(session_key, message)`, base64 (URL-safe, no
//!    padding -- same alphabet as `access::AccessRecordFile`).
//!
//! The host sends its own fingerprint's tag in `Offer.auth` and checks the
//! client's fingerprint's tag in `Answer.auth`; without a password (no
//! session key) neither side sets or checks the field, so behavior is
//! unchanged from before this slice. See `host/src/signaling/mod.rs`'s
//! `begin_session` and its `Answer` handling for where this plugs in.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The fixed domain-separation prefix for the HMAC message -- see this
/// module's doc comment.
const AUTH_TAG_PREFIX: &str = "rcdesk-dtls-v1";

/// Extracts and normalizes the first `a=fingerprint:<alg> <hex>` line in
/// `sdp` (session level or an m-section, whichever appears first -- SDP
/// lines are read in document order). Returns `None` when there is no such
/// line, or the line after `a=fingerprint:` doesn't have both an algorithm
/// and a hex value.
pub fn fingerprint_from_sdp(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("a=fingerprint:") else {
            continue;
        };
        let mut parts = rest.splitn(2, ' ');
        let (Some(alg), Some(hex)) = (parts.next(), parts.next()) else {
            continue;
        };
        let alg = alg.trim();
        let hex = hex.trim();
        if alg.is_empty() || hex.is_empty() {
            continue;
        }
        return Some(format!("{} {}", alg.to_lowercase(), hex.to_uppercase()));
    }
    None
}

/// `HMAC-SHA256(key, "rcdesk-dtls-v1|" + role + "|" + fingerprint)`, base64
/// (URL-safe, no padding). `key` is the OPAQUE session key's raw bytes
/// (`access::SessionKey::as_bytes`); HMAC accepts any key length, so this
/// never fails.
pub fn auth_tag(key: &[u8], role: &str, fingerprint: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC-SHA256 accepts a key of any length, including a 64-byte session key");
    mac.update(AUTH_TAG_PREFIX.as_bytes());
    mac.update(b"|");
    mac.update(role.as_bytes());
    mac.update(b"|");
    mac.update(fingerprint.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Verifies `tag` against `auth_tag(key, role, fingerprint)` in constant
/// time (`hmac::Mac::verify_slice`). Returns `false` -- never panics or
/// errors out to the caller -- on invalid base64, a tag of the wrong
/// length, or a genuine mismatch: all three just mean "not authenticated".
pub fn verify_auth_tag(key: &[u8], role: &str, fingerprint: &str, tag: &str) -> bool {
    let Ok(tag_bytes) = URL_SAFE_NO_PAD.decode(tag) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return false;
    };
    mac.update(AUTH_TAG_PREFIX.as_bytes());
    mac.update(b"|");
    mac.update(role.as_bytes());
    mac.update(b"|");
    mac.update(fingerprint.as_bytes());
    mac.verify_slice(&tag_bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test vector shared with `web/src/dtlsBind.test.ts` (slice 3.2e) --
    /// both implementations must produce this exact tag for these exact
    /// inputs, or the host and browser client would derive different tags
    /// for the same real login and every password-protected session would
    /// fail `verify_auth_tag`/`verifyAuthTag`.
    const VECTOR_KEY: [u8; 64] = [0x01; 64];
    const VECTOR_ROLE: &str = "offer";
    const VECTOR_FINGERPRINT: &str = "sha-256 AA:BB:CC";
    const VECTOR_TAG: &str = "ePsRNp-0qqZJLVlyAkuOVBJ5aAHjZxfQf27HgyY0Ncc";

    #[test]
    fn auth_tag_matches_the_shared_test_vector() {
        assert_eq!(
            auth_tag(&VECTOR_KEY, VECTOR_ROLE, VECTOR_FINGERPRINT),
            VECTOR_TAG
        );
    }

    #[test]
    fn verify_auth_tag_accepts_the_shared_test_vector() {
        assert!(verify_auth_tag(
            &VECTOR_KEY,
            VECTOR_ROLE,
            VECTOR_FINGERPRINT,
            VECTOR_TAG
        ));
    }

    #[test]
    fn fingerprint_from_session_level_line() {
        let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\na=fingerprint:sha-256 AB:CD:EF\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert_eq!(
            fingerprint_from_sdp(sdp),
            Some("sha-256 AB:CD:EF".to_string())
        );
    }

    #[test]
    fn fingerprint_from_m_section_line() {
        let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=fingerprint:sha-256 11:22:33\r\n";
        assert_eq!(
            fingerprint_from_sdp(sdp),
            Some("sha-256 11:22:33".to_string())
        );
    }

    #[test]
    fn fingerprint_normalizes_algorithm_case_and_hex_case() {
        let sdp = "a=fingerprint:SHA-256 ab:cd:ef\r\n";
        assert_eq!(
            fingerprint_from_sdp(sdp),
            Some("sha-256 AB:CD:EF".to_string())
        );
    }

    #[test]
    fn fingerprint_absent_returns_none() {
        let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert_eq!(fingerprint_from_sdp(sdp), None);
    }

    #[test]
    fn verify_rejects_a_different_role() {
        let key = [0x02u8; 64];
        let tag = auth_tag(&key, "offer", "sha-256 AA:BB");
        assert!(!verify_auth_tag(&key, "answer", "sha-256 AA:BB", &tag));
    }

    #[test]
    fn verify_rejects_a_different_fingerprint() {
        let key = [0x02u8; 64];
        let tag = auth_tag(&key, "offer", "sha-256 AA:BB");
        assert!(!verify_auth_tag(&key, "offer", "sha-256 AA:BC", &tag));
    }

    #[test]
    fn verify_rejects_a_different_key() {
        let key = [0x02u8; 64];
        let other_key = [0x03u8; 64];
        let tag = auth_tag(&key, "offer", "sha-256 AA:BB");
        assert!(!verify_auth_tag(&other_key, "offer", "sha-256 AA:BB", &tag));
    }

    #[test]
    fn verify_rejects_garbage_base64() {
        let key = [0x02u8; 64];
        assert!(!verify_auth_tag(
            &key,
            "offer",
            "sha-256 AA:BB",
            "not valid base64!!"
        ));
    }
}
