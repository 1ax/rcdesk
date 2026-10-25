//! Настройки хоста, которые пишет человек, а не хост (D47-4): файл
//! `settings.json` в каталоге данных. Сейчас в нём одно поле -- адрес
//! сигнального сервера (например, LAN-хаба). Хост этот файл никогда не пишет.

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

/// Поле `server` из файла настроек; `None`, если файла нет, он не читается
/// или не разбирается (с предупреждением), либо поле пустое.
fn server_from_file(dir: &Path) -> Option<String> {
    let path = dir.join(SETTINGS_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "не удалось прочитать settings.json, беру адрес по умолчанию");
            return None;
        }
    };
    match serde_json::from_slice::<Settings>(&bytes) {
        Ok(settings) => settings
            .server
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "settings.json не разобрался, беру адрес по умолчанию");
            None
        }
    }
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
}
