//! Самообновление на macOS (D50c): `.pkg` ставится через `installer` с запросом
//! прав администратора (osascript). Запуск без ожидания: preinstall пакета
//! останавливает агента, то есть и этот процесс.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;

/// Имя ассета в релизе.
pub const ASSET_NAME: &str = "rcdesk-macos.pkg";

pub fn curl_path() -> PathBuf {
    PathBuf::from("/usr/bin/curl")
}

/// На macOS консольных окон у дочерних процессов нет.
pub fn hide_console(_command: &mut tokio::process::Command) {}

/// Запускает `installer -pkg` через osascript с правами администратора. Путь к
/// пакету идёт аргументом `argv`, а не вклеивается в текст скрипта.
pub fn launch_installer(installer: &Path, log: &Path) -> anyhow::Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("не создать {}", dir.display()))?;
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("не открыть лог установщика {}", log.display()))?;
    let log_err = log_file.try_clone().context("не дублировать хендл лога")?;
    let child = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg("on run argv")
        .arg("-e")
        .arg(
            "do shell script \"/usr/sbin/installer -pkg \" & quoted form of (item 1 of argv) \
             & \" -target /\" with prompt \"rcdesk устанавливает обновление\" \
             with administrator privileges",
        )
        .arg("-e")
        .arg("end run")
        .arg(installer)
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .spawn()
        .context("не запустить osascript")?;
    tracing::info!(pid = child.id(), log = %log.display(), "установщик обновления запущен (osascript)");
    // Не ждём: preinstall убьёт агента. Дескриптор отпускаем.
    drop(child);
    Ok(())
}
