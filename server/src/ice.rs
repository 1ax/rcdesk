//! ICE server configuration for `Registered`/`Joined` replies (see
//! ARCHITECTURE.md §11 and §6): STUN URLs from env, plus optional
//! short-lived TURN credentials generated the way coturn's REST API
//! (`use-auth-secret`) expects: `username = "{expiry}:rcdesk"`,
//! `credential = base64(HMAC-SHA1(secret, username))`.

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::time::{SystemTime, UNIX_EPOCH};

use proto::signal::IceServer;

type HmacSha1 = Hmac<Sha1>;

const DEFAULT_STUN_URLS: &str = "stun:stun.l.google.com:19302";
const DEFAULT_TURN_TTL_SECS: u64 = 86400;

/// Current time as a Unix timestamp, for `IceConfig::ice_servers`'s
/// `now_unix` argument. `unwrap_or_default()` rather than `unwrap()`: a
/// clock set before 1970 would be a broken host, not a reason to panic the
/// signaling connection over a TURN credential.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone)]
pub struct IceConfig {
    stun_urls: Vec<String>,
    turn_urls: Vec<String>,
    turn_secret: String,
    turn_ttl_secs: u64,
}

impl IceConfig {
    pub fn new(
        stun_urls: Vec<String>,
        turn_urls: Vec<String>,
        turn_secret: String,
        turn_ttl_secs: u64,
    ) -> Self {
        Self {
            stun_urls,
            turn_urls,
            turn_secret,
            turn_ttl_secs,
        }
    }

    /// Reads `RCDESK_STUN_URLS`, `RCDESK_TURN_URLS`, `RCDESK_TURN_SECRET`,
    /// `RCDESK_TURN_TTL_SECS` (see this module's doc comment for defaults).
    pub fn from_env() -> Self {
        Self::new(
            parse_csv_env("RCDESK_STUN_URLS", DEFAULT_STUN_URLS),
            parse_csv_env("RCDESK_TURN_URLS", ""),
            std::env::var("RCDESK_TURN_SECRET").unwrap_or_default(),
            std::env::var("RCDESK_TURN_TTL_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_TURN_TTL_SECS),
        )
    }

    /// Builds the ICE server list for one `Registered`/`Joined` reply: one
    /// `IceServer` per STUN URL (no credentials), followed by a single TURN
    /// `IceServer` covering every `RCDESK_TURN_URLS` entry with short-lived
    /// REST-API credentials -- only when both `RCDESK_TURN_URLS` and
    /// `RCDESK_TURN_SECRET` are set; otherwise TURN is omitted entirely.
    pub fn ice_servers(&self, now_unix: u64) -> Vec<IceServer> {
        let mut servers: Vec<IceServer> = self
            .stun_urls
            .iter()
            .map(|url| IceServer {
                urls: vec![url.clone()],
                username: None,
                credential: None,
            })
            .collect();

        if !self.turn_urls.is_empty() && !self.turn_secret.is_empty() {
            let expiry = now_unix + self.turn_ttl_secs;
            let username = format!("{expiry}:rcdesk");
            let credential = turn_credential(&self.turn_secret, &username);
            servers.push(IceServer {
                urls: self.turn_urls.clone(),
                username: Some(username),
                credential: Some(credential),
            });
        }

        servers
    }
}

/// coturn's `use-auth-secret` REST API credential: base64 of
/// `HMAC-SHA1(secret, username)`.
fn turn_credential(secret: &str, username: &str) -> String {
    let mut mac =
        HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC-SHA1 accepts a key of any length");
    mac.update(username.as_bytes());
    BASE64_STANDARD.encode(mac.finalize().into_bytes())
}

fn parse_csv_env(key: &str, default: &str) -> Vec<String> {
    let raw = std::env::var(key).unwrap_or_else(|_| default.to_string());
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 2202 test case 1: key = 0x0b repeated 20 times, data = "Hi
    /// There" -> HMAC-SHA1 =
    /// 0xb617318655057264e28bc0b6fb378c8ef146be00. Verifies `turn_credential`
    /// (base64 of that HMAC) against the base64 encoding of the known
    /// digest bytes, computed independently of `turn_credential` itself.
    #[test]
    fn turn_credential_matches_base64_of_rfc2202_test_case_1_hmac() {
        let key = "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b";
        let key_bytes: Vec<u8> = (0..key.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&key[i..i + 2], 16).expect("valid hex"))
            .collect();
        let key_string = String::from_utf8(key_bytes).expect("key bytes are valid utf-8");

        let expected_digest_hex = "b617318655057264e28bc0b6fb378c8ef146be00";
        let expected_bytes: Vec<u8> = (0..expected_digest_hex.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&expected_digest_hex[i..i + 2], 16).expect("valid hex digit")
            })
            .collect();
        let expected_base64 = BASE64_STANDARD.encode(&expected_bytes);

        assert_eq!(turn_credential(&key_string, "Hi There"), expected_base64);
    }

    #[test]
    fn ice_servers_without_turn_secret_returns_only_stun() {
        let cfg = IceConfig::new(
            vec!["stun:stun.l.google.com:19302".to_string()],
            vec!["turn:turn.rcdesk.app:3478".to_string()],
            String::new(),
            86400,
        );

        let servers = cfg.ice_servers(1_000_000);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec!["stun:stun.l.google.com:19302"]);
        assert!(servers[0].username.is_none());
        assert!(servers[0].credential.is_none());
    }

    #[test]
    fn ice_servers_with_turn_secret_includes_expiry_username_and_nonempty_credential() {
        let cfg = IceConfig::new(
            vec!["stun:stun.l.google.com:19302".to_string()],
            vec!["turn:turn.rcdesk.app:3478".to_string()],
            "s3cret".to_string(),
            86400,
        );

        let now = 1_000_000u64;
        let servers = cfg.ice_servers(now);

        assert_eq!(servers.len(), 2);
        let turn = &servers[1];
        assert_eq!(turn.urls, vec!["turn:turn.rcdesk.app:3478"]);
        let username = turn.username.as_ref().expect("turn server has a username");
        assert_eq!(username, &format!("{}:rcdesk", now + 86400));
        assert!(!turn
            .credential
            .as_ref()
            .expect("turn server has a credential")
            .is_empty());
    }
}
