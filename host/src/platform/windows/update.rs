//! Самообновление на Windows (D50c): Inno Setup в тихом режиме. Установщик сам
//! гасит службу и агента (`PrepareToInstall`); запуск без ожидания.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ELEVATION_REQUIRED};
use windows::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW};

/// Имя ассета в релизе.
pub const ASSET_NAME: &str = "rcdesk-setup.exe";

/// `%SystemRoot%\System32\curl.exe` (запасной `C:\Windows`).
pub fn curl_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    root.join("System32").join("curl.exe")
}

/// Без консольного окна: агент -- GUI-процесс, `curl.exe` иначе мигнул бы окном.
pub fn hide_console(command: &mut tokio::process::Command) {
    command.creation_flags(CREATE_NO_WINDOW.0);
}

fn spawn_setup(installer: &Path, log: &Path, flags: u32) -> std::io::Result<std::process::Child> {
    Command::new(installer)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        // Кавычки Inno ждёт буквально: `arg` экранировал бы их обратной косой.
        .raw_arg(format!("/LOG=\"{}\"", log.display()))
        .creation_flags(flags)
        .spawn()
}

/// Запускает установщик без ожидания. Из Job object помощника службы выходим
/// через `CREATE_BREAKAWAY_FROM_JOB`; если Job этого не разрешает (отказ в
/// доступе) -- повтор без флага.
pub fn launch_installer(installer: &Path, log: &Path) -> anyhow::Result<()> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("не создать {}", dir.display()))?;
    }
    let no_window = CREATE_NO_WINDOW.0;
    let breakaway = CREATE_BREAKAWAY_FROM_JOB.0;
    let result = match spawn_setup(installer, log, no_window | breakaway) {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {
            tracing::warn!(error = %e, "Job не разрешает breakaway, запуск установщика без CREATE_BREAKAWAY_FROM_JOB");
            spawn_setup(installer, log, no_window)
        }
        other => other,
    };
    match result {
        Ok(child) => {
            tracing::info!(pid = child.id(), log = %log.display(), "установщик обновления запущен");
            Ok(())
        }
        Err(e) if e.raw_os_error() == Some(ERROR_ELEVATION_REQUIRED.0 as i32) => {
            tracing::warn!("установщику нужны права администратора (ошибка 740): процесс запущен без повышения");
            Err(e).context("установщик требует прав администратора (ERROR_ELEVATION_REQUIRED, 740)")
        }
        Err(e) => Err(e).with_context(|| format!("не запустить {}", installer.display())),
    }
}
