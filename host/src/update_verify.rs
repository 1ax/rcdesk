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

/// Боевой открытый ключ подписи релизов (ed25519, 32 байта). Закрытый -- в секрете
/// `RELEASE_SIGN_KEY` репозитория.
pub const RELEASE_PUBLIC_KEY: [u8; 32] = [
    0x7f, 0x9d, 0xa6, 0x24, 0xd3, 0x68, 0x86, 0xd8, 0x7b, 0x4a, 0x7c, 0xf7, 0xc8, 0x01, 0xf0, 0x6a,
    0xc0, 0x5a, 0x60, 0x5b, 0xc6, 0x29, 0x0f, 0x28, 0x42, 0xe0, 0x7b, 0x83, 0xb1, 0xd6, 0xf8, 0xb3,
];

/// Проверенный манифест релиза.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: Version,
    /// Имя ассета -> sha256 (hex, 64 символа).
    pub assets: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawManifest {
    version: String,
    assets: BTreeMap<String, String>,
}

/// Проверяет подпись боевым ключом и разбирает манифест.
pub fn verify_manifest(json: &[u8], sig: &[u8]) -> anyhow::Result<Manifest> {
    verify_manifest_with(json, sig, &RELEASE_PUBLIC_KEY)
}

pub(crate) fn verify_manifest_with(
    json: &[u8],
    sig: &[u8],
    public_key: &[u8],
) -> anyhow::Result<Manifest> {
    // Сначала подпись: недоверенный JSON до проверки не разбирается.
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(json, sig)
        .map_err(|_| anyhow!("подпись манифеста обновления не сходится"))?;
    let raw: RawManifest = serde_json::from_slice(json).context("манифест не разобрался")?;
    let version = update_check::parse_version(&raw.version)
        .ok_or_else(|| anyhow!("в манифесте некорректная версия '{}'", raw.version))?;
    for (name, hash) in &raw.assets {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("в манифесте некорректный sha256 у ассета '{name}'");
        }
    }
    Ok(Manifest {
        version,
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
            &hex_decode(TEST_PUBLIC_KEY_HEX),
        )
        .unwrap()
    }

    /// Подписывает произвольный JSON фиксированным seed; возвращает (открытый ключ, подпись).
    fn sign(json: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
        (
            pair.public_key().as_ref().to_vec(),
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
            &hex_decode(TEST_PUBLIC_KEY_HEX),
        );
        assert!(res.is_err());
    }

    #[test]
    fn bad_signature_is_rejected() {
        let json = fixture("rcdesk-update.json");
        let key = hex_decode(TEST_PUBLIC_KEY_HEX);
        let sig = fixture("rcdesk-update.json.sig");
        assert!(verify_manifest_with(&json, &sig[..63], &key).is_err());
        assert!(verify_manifest_with(&json, &[sig.as_slice(), &[0]].concat(), &key).is_err());
        assert!(verify_manifest_with(&json, &[], &key).is_err());
        let mut broken = sig.clone();
        broken[0] ^= 1;
        assert!(verify_manifest_with(&json, &broken, &key).is_err());
    }

    #[test]
    fn production_key_rejects_test_fixture() {
        let res = verify_manifest(
            &fixture("rcdesk-update.json"),
            &fixture("rcdesk-update.json.sig"),
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
        let json = format!(r#"{{"version":"1.2","assets":{{"a":"{hash}"}}}}"#);
        let (key, sig) = sign(json.as_bytes());
        assert!(verify_manifest_with(json.as_bytes(), &sig, &key).is_err());
    }

    #[test]
    fn bad_hex_is_rejected() {
        let (short, nonhex) = ("ab".repeat(31), "zz".repeat(32));
        for hash in [short, nonhex] {
            let json = format!(r#"{{"version":"1.2.3","assets":{{"a":"{hash}"}}}}"#);
            let (key, sig) = sign(json.as_bytes());
            assert!(verify_manifest_with(json.as_bytes(), &sig, &key).is_err());
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let hash = "a".repeat(64);
        let json = format!(r#"{{"version":"1.2.3","extra":1,"assets":{{"a":"{hash}"}}}}"#);
        let (key, sig) = sign(json.as_bytes());
        let m = verify_manifest_with(json.as_bytes(), &sig, &key).unwrap();
        assert_eq!(m.version, Version(1, 2, 3));
    }
}
