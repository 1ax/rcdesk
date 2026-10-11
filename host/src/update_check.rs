//! Проверка обновлений (4.3d): последняя версия из GitHub Releases.
//!
//! `HEAD https://github.com/1ax/rcdesk/releases/latest` без следования редиректу:
//! при наличии релизов GitHub отвечает `302` с `Location: .../releases/tag/vX.Y.Z`,
//! версия -- последний сегмент пути. Без релизов `Location` указывает на
//! `.../releases` (без `tag/`) -- это «нет данных». HTTP-клиент ручной (rustls +
//! httparse), чтобы не тащить в граф новый крейт.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// Страница загрузки, которую открывает пункт меню агента (её делает 4.3e).
pub const DOWNLOAD_URL: &str = "https://rcdesk.app/#download";

const RELEASES_HOST: &str = "github.com";
const LATEST_PATH: &str = "/1ax/rcdesk/releases/latest";
/// Общий таймаут «соединение + ответ».
const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
/// Верхняя граница размера заголовков ответа.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Версия `X.Y.Z` (сравнивается лексикографически по полям).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Разбирает `X.Y.Z` или `vX.Y.Z`. Пре-релизы, метаданные и мусор -> `None`.
pub fn parse_version(text: &str) -> Option<Version> {
    let text = text.strip_prefix('v').unwrap_or(text);
    let mut parts = text.split('.');
    let mut next = || {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse::<u64>().ok()
    };
    let version = Version(next()?, next()?, next()?);
    if parts.next().is_some() {
        return None;
    }
    Some(version)
}

/// Версия из значения `Location` (абсолютного или относительного):
/// путь должен оканчиваться на `.../tag/<версия>`.
pub fn parse_location(location: &str) -> Option<Version> {
    let rest = location.split_once("://").map_or(location, |(_, r)| r);
    let path = rest.split(['?', '#']).next()?;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let [.., tag, last] = segments.as_slice() else {
        return None;
    };
    if *tag != "tag" {
        return None;
    }
    parse_version(last)
}

/// `true`, если `latest` строго новее `current` (оба должны разбираться).
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// Разбор сырых заголовков ответа. `Ok(None)` -- ответ корректный, но версии нет
/// (200, редирект не на тег). Ошибка -- обрезанный/непонятный ответ или статус 4xx/5xx.
pub fn parse_response(raw: &[u8]) -> anyhow::Result<Option<Version>> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(raw).context("некорректный HTTP-ответ")? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => bail!("HTTP-ответ оборван до конца заголовков"),
    }
    let code = response.code.unwrap_or(0);
    match code {
        300..=399 => {
            let location = response
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("location"))
                .and_then(|h| std::str::from_utf8(h.value).ok());
            Ok(location.and_then(parse_location))
        }
        200..=299 => Ok(None),
        _ => bail!("HTTP {code}"),
    }
}

/// Последняя опубликованная версия `1ax/rcdesk`; `None` -- релизов нет.
pub async fn fetch_latest() -> anyhow::Result<Option<Version>> {
    fetch_latest_from(RELEASES_HOST, LATEST_PATH).await
}

/// То же для произвольного публичного репозитория GitHub (`path` --
/// `/<owner>/<repo>/releases/latest`).
pub async fn fetch_latest_from(host: &str, path: &str) -> anyhow::Result<Option<Version>> {
    tokio::time::timeout(TOTAL_TIMEOUT, head_headers(host, path))
        .await
        .map_err(|_| anyhow!("таймаут {} с", TOTAL_TIMEOUT.as_secs()))?
        .and_then(|raw| parse_response(&raw))
}

async fn head_headers(host: &str, path: &str) -> anyhow::Result<Vec<u8>> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("версии TLS")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let tcp = TcpStream::connect((host, 443))
        .await
        .with_context(|| format!("соединение с {host}"))?;
    let server_name = ServerName::try_from(host.to_owned()).context("имя сервера")?;
    let mut tls = connector
        .connect(server_name, tcp)
        .await
        .context("TLS-рукопожатие")?;

    let request = format!(
        "HEAD {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: rcdesk/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        crate::build_info::FULL_VERSION
    );
    tls.write_all(request.as_bytes())
        .await
        .context("отправка запроса")?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 2048];
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        if raw.len() > MAX_HEADER_BYTES {
            bail!("заголовки ответа длиннее {MAX_HEADER_BYTES} байт");
        }
        let n = tls.read(&mut buf).await.context("чтение ответа")?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_absolute_and_relative() {
        assert_eq!(
            parse_location("https://github.com/1ax/rcdesk/releases/tag/v0.2.0"),
            Some(Version(0, 2, 0))
        );
        assert_eq!(
            parse_location("/1ax/rcdesk/releases/tag/v1.10.3"),
            Some(Version(1, 10, 3))
        );
        assert_eq!(
            parse_location("https://github.com/rust-lang/rust/releases/tag/1.99.0"),
            Some(Version(1, 99, 0))
        );
    }

    #[test]
    fn location_without_tag_or_prerelease_is_none() {
        assert_eq!(
            parse_location("https://github.com/1ax/rcdesk/releases"),
            None
        );
        assert_eq!(
            parse_location("https://github.com/1ax/rcdesk/releases/tag/v0.2.0-rc1"),
            None
        );
        assert_eq!(parse_location(""), None);
        assert_eq!(parse_location("/tag"), None);
        assert_eq!(parse_location("/a/tag/v1.2"), None);
        assert_eq!(parse_location("/a/tag/v1.2.3.4"), None);
        assert_eq!(parse_location("/a/tag/v1..3"), None);
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("0.2.0-rc1", "0.1.0"));
        assert!(!is_newer("junk", "0.1.0"));
        assert!(!is_newer("0.2.0", "junk"));
    }

    #[test]
    fn response_redirect_to_tag() {
        let raw = b"HTTP/1.1 302 Found\r\ncontent-type: text/html\r\nLocation: https://github.com/1ax/rcdesk/releases/tag/v0.3.1\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), Some(Version(0, 3, 1)));
    }

    #[test]
    fn response_redirect_without_release() {
        let raw = b"HTTP/1.1 302 Found\r\nlocation: https://github.com/1ax/rcdesk/releases\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), None);
    }

    #[test]
    fn response_200_without_location_is_none() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), None);
    }

    #[test]
    fn response_404_and_garbage_are_errors() {
        let raw = b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n";
        assert!(parse_response(raw).is_err());
        assert!(parse_response(b"HTTP/1.1 302 Found\r\nLocation: /a").is_err());
        assert!(parse_response(b"garbage\r\n\r\n").is_err());
        assert!(parse_response(b"").is_err());
    }

    /// Живой запрос: `cargo test -p rcdesk-host update_check -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "ходит в сеть (github.com)"]
    async fn live_github() {
        let ours = fetch_latest().await;
        println!("1ax/rcdesk: {ours:?}");
        ours.unwrap();
        let rust = fetch_latest_from("github.com", "/rust-lang/rust/releases/latest").await;
        println!("rust-lang/rust: {rust:?}");
        assert!(rust.unwrap().is_some());
    }
}
