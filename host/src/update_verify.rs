//! Проверка подписанного манифеста обновления (D50b).
//!
//! Релиз публикует `rcdesk-update.json` (версия + sha256 установщиков) и
//! `rcdesk-update.json.sig` -- сырую подпись ed25519 над байтами манифеста
//! (`infra/release-manifest.sh`). Хост проверяет подпись зашитым открытым ключом,
//! и только потом разбирает JSON; скачанный установщик сверяется с хэшем из манифеста.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::update_check::{self, Version};

/// Открытые ключи подписи релизов (ed25519, по 32 байта): боевой (секрет
/// `RELEASE_SIGN_KEY` окружения `release`) и резервный (закрытая часть только офлайн у
/// владельца). Подпись должна сойтись с любым из них; при компрометации боевого релиз
/// подписывается резервным, а новая версия хоста меняет список.
pub const RELEASE_PUBLIC_KEYS: [[u8; 32]; 2] = [
    [
        0x7f, 0x9d, 0xa6, 0x24, 0xd3, 0x68, 0x86, 0xd8, 0x7b, 0x4a, 0x7c, 0xf7, 0xc8, 0x01, 0xf0,
        0x6a, 0xc0, 0x5a, 0x60, 0x5b, 0xc6, 0x29, 0x0f, 0x28, 0x42, 0xe0, 0x7b, 0x83, 0xb1, 0xd6,
        0xf8, 0xb3,
    ],
    [
        0xa1, 0x1b, 0x05, 0x34, 0x86, 0x29, 0x49, 0x93, 0xac, 0x53, 0xd5, 0x85, 0xa8, 0x06, 0x05,
        0x0e, 0x12, 0xbd, 0x2d, 0x52, 0x81, 0x2e, 0x45, 0x5e, 0x56, 0x4e, 0x0d, 0x92, 0xf7, 0x74,
        0xb9, 0xfe,
    ],
];

/// Максимальный возраст манифеста. Защита от freeze-атаки: подмены ответа старым, но
/// корректно подписанным манифестом (например, с уязвимой версией). Хост берёт
/// `releases/latest`, поэтому при паузе в релизах дольше этого срока «Обновить» откажет
/// с явным текстом -- обновлять нужно вручную.
pub const MAX_MANIFEST_AGE_SECS: u64 = 180 * 24 * 3600;

/// Допустимое расхождение часов: манифест «из будущего» дальше этого срока отвергается
/// (сбитые часы на компьютере или подмена даты).
pub const MAX_CLOCK_SKEW_SECS: u64 = 24 * 3600;

/// Проверенный манифест релиза.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    /// Время выпуска манифеста, unix-секунды UTC.
    pub issued_at: u64,
    /// Имя ассета -> sha256 (hex, 64 символа).
    pub assets: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawManifest {
    version: String,
    issued_at: u64,
    assets: BTreeMap<String, String>,
}

/// Проверяет подпись боевыми ключами и разбирает манифест; `now_unix` -- текущее время.
pub fn verify_manifest(json: &[u8], sig: &[u8], now_unix: u64) -> anyhow::Result<Manifest> {
    verify_manifest_with(json, sig, &RELEASE_PUBLIC_KEYS, now_unix)
}

pub(crate) fn verify_manifest_with(
    json: &[u8],
    sig: &[u8],
    public_keys: &[[u8; 32]],
    now_unix: u64,
) -> anyhow::Result<Manifest> {
    // Сначала подпись: недоверенный JSON до проверки не разбирается.
    let key_index = public_keys
        .iter()
        .position(|key| {
            UnparsedPublicKey::new(&ED25519, key)
                .verify(json, sig)
                .is_ok()
        })
        .ok_or_else(|| anyhow!("подпись манифеста обновления не сходится ни с одним ключом"))?;
    tracing::debug!(key_index, "подпись манифеста сошлась");
    let raw: RawManifest = serde_json::from_slice(json).context("манифест не разобрался")?;
    let version = update_check::parse_version(&raw.version)
        .ok_or_else(|| anyhow!("в манифесте некорректная версия '{}'", raw.version))?;
    if raw.issued_at > now_unix.saturating_add(MAX_CLOCK_SKEW_SECS) {
        bail!(
            "дата выпуска манифеста обновления ({}) в будущем -- проверьте часы на этом компьютере",
            raw.issued_at
        );
    }
    if now_unix > raw.issued_at.saturating_add(MAX_MANIFEST_AGE_SECS) {
        bail!(
            "манифест обновления выпущен более {} дней назад -- обновите rcdesk вручную",
            MAX_MANIFEST_AGE_SECS / (24 * 3600)
        );
    }
    for (name, hash) in &raw.assets {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("в манифесте некорректный sha256 у ассета '{name}'");
        }
    }
    Ok(Manifest {
        version,
        issued_at: raw.issued_at,
        assets: raw.assets,
    })
}

/// sha256 файла в hex нижнего регистра; читает потоково.
pub fn sha256_file_hex(path: &Path) -> anyhow::Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("не открыть {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("ошибка чтения {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Сверяет файл с хэшем ассета из манифеста.
pub fn check_asset(manifest: &Manifest, asset_name: &str, path: &Path) -> anyhow::Result<()> {
    let expected = manifest
        .assets
        .get(asset_name)
        .ok_or_else(|| anyhow!("ассета '{asset_name}' нет в манифесте"))?;
    let actual = sha256_file_hex(path)?;
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("sha256 ассета '{asset_name}' не совпал с манифестом");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/update");
    /// Открытый ключ `test-key.pem` (только для тестов).
    const TEST_PUBLIC_KEY_HEX: &str =
        "3ccc1ef066e49148611fffdd51d3337022f84c7bf567fe24a9741925fec89e07";

    /// Время «сейчас» для фикстуры: через час после выпуска.
    const NOW: u64 = 1_700_000_000 + 3600;

    fn test_key() -> [u8; 32] {
        hex_decode(TEST_PUBLIC_KEY_HEX).try_into().unwrap()
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(Path::new(FIXTURES).join(name)).unwrap()
    }

    fn fixture_manifest() -> Manifest {
        verify_manifest_with(
            &fixture("rcdesk-update.json"),
            &fixture("rcdesk-update.json.sig"),
            &[test_key()],
            NOW,
        )
        .unwrap()
    }

    /// Подписывает произвольный JSON фиксированным seed; возвращает (открытый ключ, подпись).
    fn sign(json: &[u8]) -> ([u8; 32], Vec<u8>) {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
        (
            pair.public_key().as_ref().try_into().unwrap(),
            pair.sign(json).as_ref().to_vec(),
        )
    }

    #[test]
    fn fixture_parses() {
        let m = fixture_manifest();
        assert_eq!(m.version, Version(1, 2, 3));
        assert_eq!(m.assets.len(), 2);
        assert!(m.assets.contains_key("rcdesk-setup.exe"));
        assert!(m.assets.contains_key("rcdesk-macos.pkg"));
    }

    #[test]
    fn check_asset_accepts_fixture_stubs() {
        let m = fixture_manifest();
        for name in ["rcdesk-setup.exe", "rcdesk-macos.pkg"] {
            check_asset(&m, name, &Path::new(FIXTURES).join(name)).unwrap();
        }
    }

    #[test]
    fn check_asset_is_case_insensitive() {
        let mut m = fixture_manifest();
        let hash = m.assets["rcdesk-setup.exe"].to_uppercase();
        m.assets.insert("rcdesk-setup.exe".into(), hash);
        check_asset(
            &m,
            "rcdesk-setup.exe",
            &Path::new(FIXTURES).join("rcdesk-setup.exe"),
        )
        .unwrap();
    }

    #[test]
    fn tampered_manifest_is_rejected() {
        let mut json = fixture("rcdesk-update.json");
        json[12] ^= 1;
        let res = verify_manifest_with(
            &json,
            &fixture("rcdesk-update.json.sig"),
            &[test_key()],
            NOW,
        );
        assert!(res.is_err());
    }

    #[test]
    fn bad_signature_is_rejected() {
        let json = fixture("rcdesk-update.json");
        let key = [test_key()];
        let sig = fixture("rcdesk-update.json.sig");
        assert!(verify_manifest_with(&json, &sig[..63], &key, NOW).is_err());
        assert!(verify_manifest_with(&json, &[sig.as_slice(), &[0]].concat(), &key, NOW).is_err());
        assert!(verify_manifest_with(&json, &[], &key, NOW).is_err());
        let mut broken = sig.clone();
        broken[0] ^= 1;
        assert!(verify_manifest_with(&json, &broken, &key, NOW).is_err());
    }

    #[test]
    fn production_key_rejects_test_fixture() {
        let res = verify_manifest(
            &fixture("rcdesk-update.json"),
            &fixture("rcdesk-update.json.sig"),
            NOW,
        );
        assert!(res.is_err());
    }

    #[test]
    fn foreign_hash_and_missing_asset_are_rejected() {
        let m = fixture_manifest();
        // Хэш setup.exe не подходит к pkg.
        let pkg = Path::new(FIXTURES).join("rcdesk-macos.pkg");
        assert!(check_asset(&m, "rcdesk-setup.exe", &pkg).is_err());
        assert!(check_asset(&m, "нет-такого.bin", &pkg).is_err());
    }

    #[test]
    fn bad_version_is_rejected() {
        let hash = "0".repeat(64);
        let json =
            format!(r#"{{"version":"1.2","issued_at":1700000000,"assets":{{"a":"{hash}"}}}}"#);
        let (key, sig) = sign(json.as_bytes());
        assert!(verify_manifest_with(json.as_bytes(), &sig, &[key], NOW).is_err());
    }

    #[test]
    fn bad_hex_is_rejected() {
        let (short, nonhex) = ("ab".repeat(31), "zz".repeat(32));
        for hash in [short, nonhex] {
            let json = format!(
                r#"{{"version":"1.2.3","issued_at":1700000000,"assets":{{"a":"{hash}"}}}}"#
            );
            let (key, sig) = sign(json.as_bytes());
            assert!(verify_manifest_with(json.as_bytes(), &sig, &[key], NOW).is_err());
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let hash = "a".repeat(64);
        let json = format!(
            r#"{{"version":"1.2.3","extra":1,"issued_at":1700000000,"assets":{{"a":"{hash}"}}}}"#
        );
        let (key, sig) = sign(json.as_bytes());
        let m = verify_manifest_with(json.as_bytes(), &sig, &[key], NOW).unwrap();
        assert_eq!(m.version, Version(1, 2, 3));
    }

    const ISSUED: u64 = 1_700_000_000;

    /// Подписанный тестовым seed манифест с произвольным значением `issued_at` (JSON-текстом).
    fn signed_with_issued(issued_json: &str) -> (String, [u8; 32], Vec<u8>) {
        let hash = "a".repeat(64);
        let json =
            format!(r#"{{"version":"1.2.3","issued_at":{issued_json},"assets":{{"a":"{hash}"}}}}"#);
        let (key, sig) = sign(json.as_bytes());
        (json, key, sig)
    }

    #[test]
    fn fixture_has_issued_at() {
        assert_eq!(fixture_manifest().issued_at, ISSUED);
    }

    #[test]
    fn second_key_in_list_is_accepted() {
        let json = fixture("rcdesk-update.json");
        let sig = fixture("rcdesk-update.json.sig");
        assert!(verify_manifest_with(&json, &sig, &[[0u8; 32], test_key()], NOW).is_ok());
        assert!(verify_manifest_with(&json, &sig, &[[0u8; 32], [1u8; 32]], NOW).is_err());
    }

    #[test]
    fn missing_issued_at_is_rejected() {
        let hash = "a".repeat(64);
        let json = format!(r#"{{"version":"1.2.3","assets":{{"a":"{hash}"}}}}"#);
        let (key, sig) = sign(json.as_bytes());
        assert!(verify_manifest_with(json.as_bytes(), &sig, &[key], NOW).is_err());
    }

    #[test]
    fn non_integer_issued_at_is_rejected() {
        for value in [r#""1700000000""#, "-1", "1.5"] {
            let (json, key, sig) = signed_with_issued(value);
            assert!(
                verify_manifest_with(json.as_bytes(), &sig, &[key], NOW).is_err(),
                "issued_at={value}"
            );
        }
    }

    #[test]
    fn too_old_manifest_is_rejected() {
        let (json, key, sig) = signed_with_issued("1700000000");
        let now = ISSUED + MAX_MANIFEST_AGE_SECS + 1;
        let err = verify_manifest_with(json.as_bytes(), &sig, &[key], now).unwrap_err();
        assert!(err.to_string().contains("180 дней"));
    }

    #[test]
    fn manifest_exactly_at_age_limit_is_accepted() {
        let (json, key, sig) = signed_with_issued("1700000000");
        let now = ISSUED + MAX_MANIFEST_AGE_SECS;
        assert!(verify_manifest_with(json.as_bytes(), &sig, &[key], now).is_ok());
    }

    #[test]
    fn future_manifest_beyond_skew_is_rejected() {
        let now = 2_000_000_000;
        let (json, key, sig) = signed_with_issued(&(now + MAX_CLOCK_SKEW_SECS).to_string());
        assert!(verify_manifest_with(json.as_bytes(), &sig, &[key], now).is_ok());
        let (json, key, sig) = signed_with_issued(&(now + MAX_CLOCK_SKEW_SECS + 1).to_string());
        assert!(verify_manifest_with(json.as_bytes(), &sig, &[key], now).is_err());
    }

    #[test]
    fn production_keys_are_distinct_and_backup_matches_hex() {
        assert_ne!(RELEASE_PUBLIC_KEYS[0], RELEASE_PUBLIC_KEYS[1]);
        assert_eq!(
            RELEASE_PUBLIC_KEYS[1].to_vec(),
            hex_decode("a11b053486294993ac53d585a806050e12bd2d52812e455e564e0d92f774b9fe")
        );
    }
}
