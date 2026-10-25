//! Раздача вшитого веб-клиента LAN-хабом (D47-3).
//!
//! Файлы `web/dist` попадают в бинарник через `build.rs` (`WEB_FILES`).
//! Заголовки повторяют `infra/nginx/rcdesk.app.conf`: CSP (но `connect-src
//! 'self'` -- сигналинг на том же origin), nosniff, X-Frame-Options,
//! Referrer-Policy; кэш: `index.html`/манифест и всё вне `assets/` --
//! `no-cache`, `assets/*` (хешированные ассеты Vite) -- навсегда. HSTS не
//! ставится: сертификат самоподписанный.

use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/web_files.rs"));

/// Как `nginx`, но без `wss://rcdesk.app`.
pub const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; connect-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const CACHE_NO_CACHE: &str = "no-cache";
const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";

const PLACEHOLDER: &str = "<!doctype html><html lang=\"ru\"><head><meta charset=\"utf-8\"><title>rcdesk hub</title></head><body><h1>Веб-клиент не собран</h1><p>Выполните <code>npm run build</code> в каталоге <code>web/</code> и пересоберите хост.</p></body></html>";

/// Файл веб-клиента, готовый к отдаче.
#[derive(Debug, PartialEq, Eq)]
pub struct Asset {
    pub body: &'static [u8],
    pub content_type: &'static str,
    pub cache_control: &'static str,
}

/// Файлы, вшитые в этот бинарник (пусто, если `web/dist` не был собран).
pub fn embedded_files() -> &'static [(&'static str, &'static [u8])] {
    WEB_FILES
}

/// Content-Type по расширению. `wasm` обязателен: OPAQUE грузит `.wasm` с
/// потоковой компиляцией, которой нужен `application/wasm`.
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map_or("", |(_, ext)| ext);
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "webmanifest" => "application/manifest+json",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Путь запроса (с ведущим `/`) -> имя файла в `dist`. `None` для `..`,
/// пустых сегментов, обратных слэшей и т.п.: перехода из каталога нет в
/// принципе (ищем точное совпадение в списке), это лишь явный отказ.
fn normalize(request_path: &str) -> Option<&str> {
    let rel = request_path.strip_prefix('/')?;
    if rel.is_empty() {
        return Some("index.html");
    }
    if rel.contains('\\') || rel.contains('\0') {
        return None;
    }
    if rel
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    Some(rel)
}

/// Чистый поиск: путь запроса -> файл или `None` (404).
pub fn lookup(files: &[(&'static str, &'static [u8])], request_path: &str) -> Option<Asset> {
    let rel = normalize(request_path)?;
    let (_, body) = files.iter().find(|(name, _)| *name == rel)?;
    let cache_control = if rel.starts_with("assets/") {
        CACHE_IMMUTABLE
    } else {
        CACHE_NO_CACHE
    };
    Some(Asset {
        body,
        content_type: content_type(rel),
        cache_control,
    })
}

fn with_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    response
}

/// Ответ на запрос статики из заданного набора файлов (тестируется на
/// синтетическом списке).
pub fn respond(
    files: &[(&'static str, &'static [u8])],
    method: &Method,
    request_path: &str,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return with_security_headers(response);
    }
    let response = match lookup(files, request_path) {
        Some(asset) => (
            [
                (header::CONTENT_TYPE, asset.content_type),
                (header::CACHE_CONTROL, asset.cache_control),
            ],
            asset.body,
        )
            .into_response(),
        None if files.is_empty() && normalize(request_path) == Some("index.html") => (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, CACHE_NO_CACHE),
            ],
            PLACEHOLDER,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    };
    with_security_headers(response)
}

/// Фоллбэк роутера хаба: всё, что не `/ws` и не `/healthz`.
pub async fn handler(method: Method, uri: Uri) -> Response {
    respond(WEB_FILES, &method, uri.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILES: &[(&str, &[u8])] = &[
        ("index.html", b"<html>"),
        ("manifest.webmanifest", b"{}"),
        ("assets/x.js", b"js"),
        ("assets/opaque.wasm", b"\0asm"),
        ("favicon.svg", b"<svg/>"),
    ];

    fn header(response: &Response, name: header::HeaderName) -> &str {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    #[test]
    fn root_serves_index_with_no_cache_and_csp() {
        let response = respond(FILES, &Method::GET, "/");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, header::CACHE_CONTROL), "no-cache");
        assert_eq!(
            header(&response, header::CONTENT_TYPE),
            "text/html; charset=utf-8"
        );
        let csp = header(&response, header::CONTENT_SECURITY_POLICY);
        assert!(csp.contains("connect-src 'self';"));
        assert!(!csp.contains("wss://"));
        assert_eq!(header(&response, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
        assert_eq!(header(&response, header::X_FRAME_OPTIONS), "DENY");
        assert_eq!(
            header(&response, header::REFERRER_POLICY),
            "strict-origin-when-cross-origin"
        );
        assert!(response
            .headers()
            .get(header::STRICT_TRANSPORT_SECURITY)
            .is_none());
    }

    #[test]
    fn assets_are_immutable_with_proper_mime() {
        let response = respond(FILES, &Method::GET, "/assets/x.js");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header(&response, header::CACHE_CONTROL),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            header(&response, header::CONTENT_TYPE),
            "text/javascript; charset=utf-8"
        );
        let wasm = respond(FILES, &Method::GET, "/assets/opaque.wasm");
        assert_eq!(header(&wasm, header::CONTENT_TYPE), "application/wasm");
    }

    #[test]
    fn manifest_and_other_files_are_not_cached_forever() {
        let manifest = lookup(FILES, "/manifest.webmanifest").expect("manifest");
        assert_eq!(manifest.content_type, "application/manifest+json");
        assert_eq!(manifest.cache_control, "no-cache");
        let icon = lookup(FILES, "/favicon.svg").expect("icon");
        assert_eq!(icon.content_type, "image/svg+xml");
    }

    #[test]
    fn traversal_and_unknown_paths_are_404() {
        for path in [
            "/../etc/passwd",
            "/assets/../index.html",
            "/assets/",
            "//index.html",
            "/assets//x.js",
            "/nope.js",
            "/assets\\x.js",
            "/%2e%2e/etc",
            "",
        ] {
            let response = respond(FILES, &Method::GET, path);
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "path {path:?}");
            assert!(
                !header(&response, header::CONTENT_SECURITY_POLICY).is_empty(),
                "заголовки есть и на 404"
            );
        }
    }

    #[test]
    fn empty_file_list_serves_503_placeholder_on_root_only() {
        let root = respond(&[], &Method::GET, "/");
        assert_eq!(root.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            header(&root, header::CONTENT_TYPE),
            "text/html; charset=utf-8"
        );
        let other = respond(&[], &Method::GET, "/assets/x.js");
        assert_eq!(other.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_get_and_head_are_allowed() {
        assert_eq!(respond(FILES, &Method::HEAD, "/").status(), StatusCode::OK);
        let post = respond(FILES, &Method::POST, "/");
        assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header(&post, header::ALLOW), "GET, HEAD");
    }
}
