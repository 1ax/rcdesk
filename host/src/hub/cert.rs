//! Самоподписанный сертификат LAN-хаба (D47-3).
//!
//! Ключ ECDSA P-256 живёт в `<data_dir>/hub-key.pem`, сертификат на нём -- в
//! `<data_dir>/hub-cert.der`; оба создаются один раз. Сертификат тоже хранится,
//! а не выпускается на каждом старте: Chrome помнит «Перейти на сайт» для
//! конкретного сертификата, и после перевыпуска предупреждение вернулось бы у
//! всех клиентов (проверено вживую 2026-10-25). Хосты закрепляют не
//! сертификат, а отпечаток открытого ключа (SHA-256 от SubjectPublicKeyInfo):
//! если сертификат перевыпустить (файл удалён), закрепление не сломается.

use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{
    date_time_ymd, CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose,
    PublicKeyData,
};
use sha2::{Digest, Sha256};

/// Имя файла ключа в каталоге данных хаба.
pub const KEY_FILE: &str = "hub-key.pem";
/// Имя файла сертификата (DER) в каталоге данных хаба.
pub const CERT_FILE: &str = "hub-cert.der";
/// Срок сертификата: десять лет, чтобы исключение в браузере не сгорало.
const CERT_VALIDITY_DAYS: i64 = 3650;

/// Всё, что нужно TLS-слушателю: сертификат и закрытый ключ (PKCS#8 DER) и
/// отпечаток открытого ключа.
pub struct HubIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// SHA-256 от SubjectPublicKeyInfo DER, hex в нижнем регистре.
    pub fingerprint: String,
}

/// Читает ключ и сертификат из каталога данных либо создаёт их. Сохранённый
/// сертификат берётся, только если он выпущен на этот ключ; иначе (нет файла,
/// мусор, ключ заменён) выпускается и сохраняется новый.
pub fn load_identity(data_dir: &Path, computer_name: &str) -> Result<HubIdentity> {
    let key = load_or_create_key(&data_dir.join(KEY_FILE))?;
    let fingerprint = key_fingerprint(&key);
    let cert_path = data_dir.join(CERT_FILE);
    let cert_der = match std::fs::read(&cert_path) {
        Ok(der) if cert_spki_sha256_hex(&der).as_deref() == Some(fingerprint.as_str()) => der,
        _ => {
            let der = generate_cert(&key, computer_name)?;
            std::fs::write(&cert_path, &der).with_context(|| {
                format!(
                    "не удалось записать сертификат хаба {}",
                    cert_path.display()
                )
            })?;
            der
        }
    };
    Ok(HubIdentity {
        cert_der,
        key_der: key.serialize_der(),
        fingerprint,
    })
}

/// Ключ из файла; если файла нет -- создаёт (на unix с правами 0600).
/// DACL для Windows появится в под-шаге службы.
pub fn load_or_create_key(path: &Path) -> Result<KeyPair> {
    match std::fs::read_to_string(path) {
        Ok(pem) => {
            return KeyPair::from_pem(&pem)
                .with_context(|| format!("не удалось разобрать ключ хаба {}", path.display()));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("не удалось прочитать {}", path.display()))
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("не удалось создать каталог {}", dir.display()))?;
    }
    let key = KeyPair::generate().context("не удалось сгенерировать ключ хаба")?;
    write_private(path, key.serialize_pem().as_bytes())
        .with_context(|| format!("не удалось записать ключ хаба {}", path.display()))?;
    Ok(key)
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

/// Самоподписанный сертификат на `key`: CN `rcdesk hub`, SAN DNS `localhost`
/// и имя компьютера (если оно годится для DNS-имени сертификата), срок от
/// «вчера» до «сегодня + 10 лет». IP-адреса в SAN не нужны: браузер всё
/// равно просит подтвердить исключение, а `crypto.subtle` доступен после него.
pub fn generate_cert(key: &KeyPair, computer_name: &str) -> Result<Vec<u8>> {
    let mut names = vec!["localhost".to_string()];
    let extra = computer_name.trim();
    if !extra.is_empty()
        && !extra.eq_ignore_ascii_case("localhost")
        && CertificateParams::new(vec![extra.to_string()]).is_ok()
    {
        names.push(extra.to_string());
    }
    let mut params = CertificateParams::new(names).context("некорректные SAN сертификата")?;
    params
        .distinguished_name
        .push(DnType::CommonName, "rcdesk hub");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let today = unix_days(std::time::SystemTime::now());
    let (y, m, d) = civil_from_days(today - 1);
    params.not_before = date_time_ymd(y, m, d);
    let (y, m, d) = civil_from_days(today + CERT_VALIDITY_DAYS);
    params.not_after = date_time_ymd(y, m, d);
    let cert = params
        .self_signed(key)
        .context("не удалось выпустить сертификат хаба")?;
    Ok(cert.der().to_vec())
}

/// Дни с 1970-01-01 (UTC) на момент `time`.
fn unix_days(time: std::time::SystemTime) -> i64 {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    (secs / 86400) as i64
}

/// Григорианская дата по числу дней с 1970-01-01 (алгоритм Хиннанта). Нужна,
/// чтобы не тянуть крейт `time` ради `OffsetDateTime::now_utc`.
fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

/// Отпечаток открытого ключа `key`: SHA-256 от SubjectPublicKeyInfo DER.
pub fn key_fingerprint(key: &KeyPair) -> String {
    spki_sha256_hex(&key.subject_public_key_info())
}

/// SHA-256 от готового SubjectPublicKeyInfo DER, hex в нижнем регистре.
pub fn spki_sha256_hex(spki_der: &[u8]) -> String {
    let digest = Sha256::digest(spki_der);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Отпечаток открытого ключа из DER сертификата (то, что предъявлено в
/// рукопожатии); `None`, если DER не разобрался.
pub fn cert_spki_sha256_hex(cert_der: &[u8]) -> Option<String> {
    spki_from_cert_der(cert_der).map(spki_sha256_hex)
}

/// Один разобранный элемент DER.
struct Tlv<'a> {
    tag: u8,
    /// Весь элемент вместе с заголовком.
    whole: &'a [u8],
    /// Содержимое без заголовка.
    body: &'a [u8],
    /// Всё после элемента.
    rest: &'a [u8],
}

/// Читает один TLV DER с начала `data`.
fn read_tlv(data: &[u8]) -> Option<Tlv<'_>> {
    let tag = *data.first()?;
    let first = *data.get(1)?;
    let (len, header) = if first < 0x80 {
        (usize::from(first), 2)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for byte in data.get(2..2 + n)? {
            len = (len << 8) | usize::from(*byte);
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    Some(Tlv {
        tag,
        whole: data.get(..end)?,
        body: data.get(header..end)?,
        rest: data.get(end..)?,
    })
}

/// Вырезает SubjectPublicKeyInfo из `Certificate ::= SEQUENCE { tbs, ... }`:
/// TBSCertificate = [0] version (необязательно), serial, signature, issuer,
/// validity, subject, subjectPublicKeyInfo.
fn spki_from_cert_der(cert_der: &[u8]) -> Option<&[u8]> {
    let cert = read_tlv(cert_der)?;
    let tbs = read_tlv(cert.body)?;
    let mut rest = tbs.body;
    let first = read_tlv(rest)?;
    if first.tag == 0xa0 {
        rest = first.rest;
    }
    // serial, signature, issuer, validity, subject
    for _ in 0..5 {
        rest = read_tlv(rest)?.rest;
    }
    let spki = read_tlv(rest)?;
    (spki.tag == 0x30).then_some(spki.whole)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rcdesk-hubcert-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn key_is_created_and_reused() {
        let dir = temp_dir("reuse");
        let first = load_identity(&dir, "my-pc").expect("first");
        assert!(dir.join(KEY_FILE).is_file());
        let second = load_identity(&dir, "my-pc").expect("second");
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.key_der, second.key_der);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn certificate_is_reused_and_reissued_for_a_new_key() {
        let dir = temp_dir("cert-reuse");
        std::fs::create_dir_all(&dir).unwrap();
        let first = load_identity(&dir, "hub-pc").unwrap();
        let second = load_identity(&dir, "hub-pc").unwrap();
        assert_eq!(first.cert_der, second.cert_der);

        std::fs::remove_file(dir.join(KEY_FILE)).unwrap();
        let third = load_identity(&dir, "hub-pc").unwrap();
        assert_ne!(third.fingerprint, first.fingerprint);
        assert_ne!(third.cert_der, first.cert_der);
        assert_eq!(
            cert_spki_sha256_hex(&third.cert_der).as_deref(),
            Some(third.fingerprint.as_str())
        );

        std::fs::write(dir.join(CERT_FILE), b"garbage").unwrap();
        let fourth = load_identity(&dir, "hub-pc").unwrap();
        assert_eq!(
            cert_spki_sha256_hex(&fourth.cert_der).as_deref(),
            Some(third.fingerprint.as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("mode");
        load_identity(&dir, "my-pc").expect("identity");
        let mode = std::fs::metadata(dir.join(KEY_FILE))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_is_stable_across_certificates_and_differs_per_key() {
        let key = KeyPair::generate().expect("key");
        let cert_a = generate_cert(&key, "pc").expect("cert a");
        let cert_b = generate_cert(&key, "other-name").expect("cert b");
        let fp = key_fingerprint(&key);
        assert_eq!(fp.len(), 64);
        assert_eq!(cert_spki_sha256_hex(&cert_a).as_deref(), Some(fp.as_str()));
        assert_eq!(cert_spki_sha256_hex(&cert_b).as_deref(), Some(fp.as_str()));

        let other = KeyPair::generate().expect("other key");
        assert_ne!(key_fingerprint(&other), fp);
    }

    #[test]
    fn san_contains_localhost_and_computer_name() {
        let key = KeyPair::generate().expect("key");
        let der = generate_cert(&key, "my-pc").expect("cert");
        let contains = |needle: &[u8]| der.windows(needle.len()).any(|w| w == needle);
        assert!(contains(b"localhost"));
        assert!(contains(b"my-pc"));
        assert!(contains(b"rcdesk hub"));
    }

    #[test]
    fn unusable_computer_name_falls_back_to_localhost_only() {
        let key = KeyPair::generate().expect("key");
        let der = generate_cert(&key, "Компьютер Андрея").expect("cert");
        assert!(der.windows(9).any(|w| w == b"localhost"));
    }

    #[test]
    fn civil_dates_match_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_089), (2025, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
