//! Настройки хоста, которые пишет человек, а не хост (D47-4): файл
//! `settings.json` в каталоге данных. Поля: адрес сигнального сервера
//! (например, LAN-хаба) и признак «служба поднимает LAN-хаб у себя» (D47-5).
//! Хост этот файл никогда не пишет.

use std::path::Path;

use serde::Deserialize;

/// Имя файла настроек в каталоге данных.
pub const SETTINGS_FILE: &str = "settings.json";

/// Адрес по умолчанию для `rcdesk-host serve`: локальный `rcdesk-server`
/// (`docs/dev-run.md`), как было до D47-4.
pub const DEV_SERVER: &str = "ws://127.0.0.1:8080/ws";

/// Содержимое `settings.json`. Незнакомые поля игнорируются.
#[derive(Debug, Deserialize, Default)]
struct Settings {
    server: Option<String>,
    /// `true` -- служба Windows запускает LAN-хаб внутри себя.
    #[serde(default)]
    hub: Option<bool>,
}

/// Адрес сигнального сервера по приоритету: флаг `--server` > поле `server`
/// файла `dir/settings.json` > `default` (у агента -- [`crate::device::DEFAULT_SERVER`], у
/// `serve` -- локальный сервер разработки [`DEV_SERVER`]).
pub fn resolve_server(flag: Option<String>, dir: &Path, default: &str) -> String {
    if let Some(server) = flag {
        tracing::info!(%server, source = "flag", "адрес сигнального сервера");
        return server;
    }
    if let Some(server) = server_from_file(dir) {
        tracing::info!(%server, source = "settings", "адрес сигнального сервера");
        return server;
    }
    tracing::info!(
        server = default,
        source = "default",
        "адрес сигнального сервера"
    );
    default.to_string()
}

/// Проверка адреса сервера (M-4): `wss://` -- всегда; `ws://` -- только для
/// loopback (иначе PIN и секрет устройства идут открытым текстом). Запрет
/// снимает `RCDESK_ALLOW_PLAIN_WS=1` (доверенная dev-сеть).
pub fn check_server_url(url: &str) -> Result<(), String> {
    let allow = std::env::var("RCDESK_ALLOW_PLAIN_WS").is_ok_and(|v| v == "1");
    let result = check_server_url_with(url, allow);
    if allow && url.starts_with("ws://") && check_server_url_with(url, false).is_err() {
        tracing::warn!(%url, "RCDESK_ALLOW_PLAIN_WS=1: незашифрованный ws:// разрешён");
    }
    result
}

/// [`check_server_url`] с явным признаком разрешения plain `ws://`.
pub fn check_server_url_with(url: &str, allow_plain: bool) -> Result<(), String> {
    if url.starts_with("wss://") {
        return Ok(());
    }
    let Some(rest) = url.strip_prefix("ws://") else {
        return Err(format!(
            "unsupported server URL scheme (got {url}); use wss://"
        ));
    };
    if allow_plain {
        return Ok(());
    }
    let authority = rest.split('/').next().unwrap_or("");
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        // `[::1]:8080` -> `::1`
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if matches!(host, "127.0.0.1" | "localhost" | "::1") {
        Ok(())
    } else {
        Err(format!(
            "plain ws:// is only allowed for loopback (got {url}); use wss:// or set RCDESK_ALLOW_PLAIN_WS=1 for a trusted dev network"
        ))
    }
}

/// Читает и разбирает `dir/settings.json`; `None`, если файла нет, он не
/// читается или не разбирается (последние два случая -- с предупреждением).
fn load(dir: &Path) -> Option<Settings> {
    let path = dir.join(SETTINGS_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "не удалось прочитать settings.json, беру значения по умолчанию");
            return None;
        }
    };
    match serde_json::from_slice::<Settings>(&bytes) {
        Ok(settings) => Some(settings),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "settings.json не разобрался, беру значения по умолчанию");
            None
        }
    }
}

/// Поле `server` из файла настроек; `None`, если файла нет, он не читается
/// или не разбирается, либо поле пустое.
fn server_from_file(dir: &Path) -> Option<String> {
    load(dir)?
        .server
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Включён ли режим LAN-хаба (`"hub": true`); любая ошибка чтения или
/// разбора -- `false`.
pub fn hub_enabled(dir: &Path) -> bool {
    load(dir).and_then(|s| s.hub) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DEFAULT_SERVER;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-host-settings-test-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, content: &str) {
        std::fs::write(dir.join(SETTINGS_FILE), content).unwrap();
    }

    #[test]
    fn check_server_url_accepts_wss_and_loopback_ws() {
        for url in [
            "wss://rcdesk.app/ws",
            "wss://10.0.0.5:8443/ws",
            "ws://127.0.0.1:8080/ws",
            "ws://localhost/ws",
            "ws://localhost:1",
            "ws://[::1]:8080/ws",
        ] {
            assert!(check_server_url_with(url, false).is_ok(), "{url}");
        }
    }

    #[test]
    fn check_server_url_rejects_plain_ws_for_lan_host() {
        for url in [
            "ws://1.2.3.4/ws",
            "ws://192.168.1.5:8080/ws",
            "ws://hub.lan/ws",
        ] {
            let err = check_server_url_with(url, false).unwrap_err();
            assert!(err.contains("RCDESK_ALLOW_PLAIN_WS"), "{err}");
            assert!(check_server_url_with(url, true).is_ok(), "{url}");
        }
    }

    #[test]
    fn check_server_url_rejects_other_schemes() {
        for url in [
            "http://127.0.0.1/ws",
            "https://rcdesk.app/ws",
            "rcdesk.app",
            "",
        ] {
            assert!(check_server_url_with(url, false).is_err(), "{url}");
            assert!(check_server_url_with(url, true).is_err(), "{url}");
        }
    }

    #[test]
    fn flag_beats_file() {
        let dir = scratch_dir("flag");
        write(&dir, r#"{"server":"wss://hub.lan:8443/ws"}"#);
        let got = resolve_server(Some("ws://127.0.0.1:1/ws".into()), &dir, DEFAULT_SERVER);
        assert_eq!(got, "ws://127.0.0.1:1/ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_beats_default() {
        let dir = scratch_dir("file");
        write(&dir, r#"{"server":"  wss://hub.lan:8443/ws "}"#);
        assert_eq!(
            resolve_server(None, &dir, DEFAULT_SERVER),
            "wss://hub.lan:8443/ws"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_file_gives_default() {
        let dir = scratch_dir("nofile");
        assert_eq!(resolve_server(None, &dir, DEFAULT_SERVER), DEFAULT_SERVER);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_json_gives_default() {
        let dir = scratch_dir("broken");
        write(&dir, "{ not json");
        assert_eq!(resolve_server(None, &dir, DEFAULT_SERVER), DEFAULT_SERVER);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let dir = scratch_dir("unknown");
        write(&dir, r#"{"future":1,"server":"wss://h:1/ws","x":{"a":2}}"#);
        assert_eq!(resolve_server(None, &dir, DEFAULT_SERVER), "wss://h:1/ws");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_server_gives_default() {
        let dir = scratch_dir("empty");
        write(&dir, r#"{"server":"   "}"#);
        assert_eq!(resolve_server(None, &dir, DEFAULT_SERVER), DEFAULT_SERVER);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hub_true_enables_hub() {
        let dir = scratch_dir("hub-true");
        write(&dir, r#"{"hub":true}"#);
        assert!(hub_enabled(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hub_off_missing_or_broken_is_disabled() {
        let dir = scratch_dir("hub-off");
        assert!(!hub_enabled(&dir), "нет файла");
        write(&dir, r#"{"server":"wss://h:1/ws"}"#);
        assert!(!hub_enabled(&dir), "нет поля");
        write(&dir, r#"{"hub":false}"#);
        assert!(!hub_enabled(&dir), "false");
        write(&dir, "{ not json");
        assert!(!hub_enabled(&dir), "битый");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
