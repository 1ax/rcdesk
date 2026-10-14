//! Самообновление хоста (D50c): скачать манифест и установщик, проверить подпись
//! и sha256 (`update_verify`), запустить установщик платформы.
//!
//! Общий код без платформенных деталей: загрузка и запуск установщика -- за
//! трейтами `Downloader`/`Launcher` (боевые реализации опираются на
//! `platform::update`), поэтому весь сценарий проверяется тестами на любой ОС.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Context};

use crate::platform;
use crate::update_check::Version;
use crate::update_verify::{self, RELEASE_PUBLIC_KEYS};

/// Манифест последнего релиза (подпись -- рядом, с суффиксом `.sig`).
const MANIFEST_URL: &str =
    "https://github.com/1ax/rcdesk/releases/latest/download/rcdesk-update.json";
const SIGNATURE_URL: &str =
    "https://github.com/1ax/rcdesk/releases/latest/download/rcdesk-update.json.sig";
const MANIFEST_FILE: &str = "rcdesk-update.json";
const SIGNATURE_FILE: &str = "rcdesk-update.json.sig";
/// Лог установщика в каталоге логов.
const INSTALL_LOG_FILE: &str = "update-install.log";

/// Этапы обновления для отображения/логирования.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateProgress {
    Checking,
    Downloading(Version),
    Installing(Version),
}

/// Чем закончилось обновление.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Манифест не новее текущей версии.
    UpToDate(Version),
    /// Установщик запущен (завершение не ждём: он останавливает агента).
    InstallerLaunched(Version),
}

/// Скачивает `url` в файл `out`.
pub trait Downloader: Send + Sync {
    fn download(&self, url: &str, out: &Path) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Запускает установщик без ожидания завершения.
pub trait Launcher: Send + Sync {
    fn launch(&self, installer: &Path, log: &Path) -> anyhow::Result<()>;
}

/// Боевая загрузка: системный `curl`.
struct CurlDownloader;

impl Downloader for CurlDownloader {
    async fn download(&self, url: &str, out: &Path) -> anyhow::Result<()> {
        let curl = platform::update::curl_path();
        let mut command = tokio::process::Command::new(&curl);
        platform::update::hide_console(&mut command);
        let output = command
            .args([
                "-fsSL",
                "--proto",
                "=https",
                "--retry",
                "2",
                "--max-time",
                "900",
                "-o",
            ])
            .arg(out)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("не запустить {}", curl.display()))?;
        if !output.status.success() {
            bail!(
                "curl завершился с кодом {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Боевой запуск: платформенный установщик.
struct PlatformLauncher;

impl Launcher for PlatformLauncher {
    fn launch(&self, installer: &Path, log: &Path) -> anyhow::Result<()> {
        platform::update::launch_installer(installer, log)
    }
}

/// Глобальный флаг «обновление идёт»; снимается при любом исходе (Drop).
static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

struct RunGuard;

impl RunGuard {
    fn acquire() -> anyhow::Result<Self> {
        if UPDATE_RUNNING.swap(true, Ordering::SeqCst) {
            bail!("обновление уже идёт");
        }
        Ok(Self)
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        UPDATE_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Скачивает, проверяет и запускает установщик, если в релизе версия новее `current`.
pub async fn run_update(
    work_dir: &Path,
    log_dir: &Path,
    current: Version,
    progress: impl Fn(UpdateProgress) + Send + Sync,
) -> anyhow::Result<UpdateOutcome> {
    run_update_with(
        &CurlDownloader,
        &PlatformLauncher,
        &RELEASE_PUBLIC_KEYS,
        now_unix(),
        platform::update::ASSET_NAME,
        work_dir,
        log_dir,
        current,
        progress,
    )
    .await
}

/// Текущее время, unix-секунды (при часах до 1970 -- 0: манифест тогда отвергнется как «из будущего»).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn run_update_with(
    downloader: &impl Downloader,
    launcher: &impl Launcher,
    public_keys: &[[u8; 32]],
    now_unix: u64,
    asset_name: &str,
    work_dir: &Path,
    log_dir: &Path,
    current: Version,
    progress: impl Fn(UpdateProgress) + Send + Sync,
) -> anyhow::Result<UpdateOutcome> {
    let _guard = RunGuard::acquire()?;
    tracing::info!(%current, "обновление: проверка релиза");
    progress(UpdateProgress::Checking);

    let dir = work_dir.join("update");
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("не очистить {}", dir.display())),
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("не создать {}", dir.display()))?;

    let manifest_path = dir.join(MANIFEST_FILE);
    let sig_path = dir.join(SIGNATURE_FILE);
    downloader
        .download(MANIFEST_URL, &manifest_path)
        .await
        .context("не скачался манифест обновления")?;
    downloader
        .download(SIGNATURE_URL, &sig_path)
        .await
        .context("не скачалась подпись манифеста")?;
    let json = std::fs::read(&manifest_path).context("не прочитать манифест")?;
    let sig = std::fs::read(&sig_path).context("не прочитать подпись манифеста")?;
    let manifest = update_verify::verify_manifest_with(&json, &sig, public_keys, now_unix)?;

    if manifest.version <= current {
        tracing::info!(%current, latest = %manifest.version, "обновление: уже последняя версия");
        return Ok(UpdateOutcome::UpToDate(manifest.version));
    }

    let version = manifest.version;
    tracing::info!(%version, asset = asset_name, "обновление: скачивание установщика");
    progress(UpdateProgress::Downloading(version));
    let installer: PathBuf = dir.join(asset_name);
    let asset_url =
        format!("https://github.com/1ax/rcdesk/releases/download/v{version}/{asset_name}");
    downloader
        .download(&asset_url, &installer)
        .await
        .context("не скачался установщик обновления")?;
    update_verify::check_asset(&manifest, asset_name, &installer)?;

    let log = log_dir.join(INSTALL_LOG_FILE);
    tracing::info!(%version, installer = %installer.display(), "обновление: запуск установщика");
    progress(UpdateProgress::Installing(version));
    launcher
        .launch(&installer, &log)
        .map_err(|e| anyhow!("не запустился установщик: {e:#}"))?;
    Ok(UpdateOutcome::InstallerLaunched(version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/update");
    /// Открытый ключ фикстуры `test-key.pem` (только для тестов).
    const TEST_PUBLIC_KEY_HEX: &str =
        "3ccc1ef066e49148611fffdd51d3337022f84c7bf567fe24a9741925fec89e07";
    const ASSET: &str = "rcdesk-setup.exe";

    /// Тесты делят глобальный guard -- гоняем по одному.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// «Сейчас» для фикстуры: через час после выпуска (`issued_at` = 1_700_000_000).
    const NOW: u64 = 1_700_000_000 + 3600;

    fn test_key() -> [u8; 32] {
        let bytes: Vec<u8> = (0..TEST_PUBLIC_KEY_HEX.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&TEST_PUBLIC_KEY_HEX[i..i + 2], 16).unwrap())
            .collect();
        bytes.try_into().unwrap()
    }

    /// Копирует файл фикстуры по имени из URL; можно испортить файл и задержать загрузку.
    #[derive(Default)]
    struct FakeDownloader {
        urls: Mutex<Vec<String>>,
        corrupt: Option<&'static str>,
        fail_on: Option<&'static str>,
        delay: Option<Duration>,
    }

    impl Downloader for FakeDownloader {
        async fn download(&self, url: &str, out: &Path) -> anyhow::Result<()> {
            self.urls.lock().unwrap().push(url.to_string());
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            let name = url.rsplit('/').next().unwrap();
            if self.fail_on == Some(name) {
                bail!("сеть недоступна");
            }
            let mut data = std::fs::read(Path::new(FIXTURES).join(name))?;
            if self.corrupt == Some(name) {
                data.push(b'!');
            }
            std::fs::write(out, data)?;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeLauncher {
        calls: Mutex<Vec<(PathBuf, PathBuf)>>,
    }

    impl Launcher for FakeLauncher {
        fn launch(&self, installer: &Path, log: &Path) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((installer.to_path_buf(), log.to_path_buf()));
            Ok(())
        }
    }

    async fn run(
        dl: &FakeDownloader,
        l: &FakeLauncher,
        work: &Path,
        current: Version,
    ) -> anyhow::Result<UpdateOutcome> {
        run_at(dl, l, work, current, NOW).await
    }

    async fn run_at(
        dl: &FakeDownloader,
        l: &FakeLauncher,
        work: &Path,
        current: Version,
        now: u64,
    ) -> anyhow::Result<UpdateOutcome> {
        run_update_with(
            dl,
            l,
            &[test_key()],
            now,
            ASSET,
            work,
            work,
            current,
            |_| {},
        )
        .await
    }

    fn tmp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rcdesk-updater-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn success_launches_installer_from_update_dir() {
        let _s = SERIAL.lock().await;
        let work = tmp("ok");
        let (dl, l) = (FakeDownloader::default(), FakeLauncher::default());
        let out = run(&dl, &l, &work, Version(1, 0, 0)).await.unwrap();
        assert_eq!(out, UpdateOutcome::InstallerLaunched(Version(1, 2, 3)));
        let calls = l.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, work.join("update").join(ASSET));
        assert_eq!(calls[0].1, work.join(INSTALL_LOG_FILE));
        assert!(calls[0].0.exists());
    }

    #[tokio::test]
    async fn asset_url_contains_manifest_version() {
        let _s = SERIAL.lock().await;
        let work = tmp("url");
        let (dl, l) = (FakeDownloader::default(), FakeLauncher::default());
        run(&dl, &l, &work, Version(1, 0, 0)).await.unwrap();
        let urls = dl.urls.lock().unwrap();
        assert!(urls.contains(&format!(
            "https://github.com/1ax/rcdesk/releases/download/v1.2.3/{ASSET}"
        )));
    }

    #[tokio::test]
    async fn current_not_older_is_up_to_date() {
        let _s = SERIAL.lock().await;
        for current in [Version(1, 2, 3), Version(2, 0, 0)] {
            let work = tmp("uptodate");
            let (dl, l) = (FakeDownloader::default(), FakeLauncher::default());
            let out = run(&dl, &l, &work, current).await.unwrap();
            assert_eq!(out, UpdateOutcome::UpToDate(Version(1, 2, 3)));
            assert!(!dl.urls.lock().unwrap().iter().any(|u| u.ends_with(ASSET)));
            assert!(l.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn bad_signature_fails() {
        let _s = SERIAL.lock().await;
        let work = tmp("sig");
        let dl = FakeDownloader {
            corrupt: Some("rcdesk-update.json.sig"),
            ..Default::default()
        };
        let l = FakeLauncher::default();
        assert!(run(&dl, &l, &work, Version(1, 0, 0)).await.is_err());
        assert!(l.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_manifest_fails_and_does_not_launch() {
        let _s = SERIAL.lock().await;
        let work = tmp("stale");
        let (dl, l) = (FakeDownloader::default(), FakeLauncher::default());
        let now = 1_700_000_000 + update_verify::MAX_MANIFEST_AGE_SECS + 1;
        assert!(run_at(&dl, &l, &work, Version(1, 0, 0), now).await.is_err());
        assert!(l.calls.lock().unwrap().is_empty());
        assert!(!dl.urls.lock().unwrap().iter().any(|u| u.ends_with(ASSET)));
    }

    #[tokio::test]
    async fn asset_hash_mismatch_fails() {
        let _s = SERIAL.lock().await;
        let work = tmp("hash");
        let dl = FakeDownloader {
            corrupt: Some(ASSET),
            ..Default::default()
        };
        let l = FakeLauncher::default();
        assert!(run(&dl, &l, &work, Version(1, 0, 0)).await.is_err());
        assert!(l.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn download_error_fails_and_releases_guard() {
        let _s = SERIAL.lock().await;
        let work = tmp("dlerr");
        let dl = FakeDownloader {
            fail_on: Some(ASSET),
            ..Default::default()
        };
        let l = FakeLauncher::default();
        assert!(run(&dl, &l, &work, Version(1, 0, 0)).await.is_err());
        assert!(l.calls.lock().unwrap().is_empty());
        // Guard снят: следующий запуск проходит.
        let dl = FakeDownloader::default();
        assert!(run(&dl, &l, &work, Version(1, 0, 0)).await.is_ok());
    }

    #[tokio::test]
    async fn second_concurrent_call_is_rejected() {
        let _s = SERIAL.lock().await;
        let work = tmp("conc");
        let dl = FakeDownloader {
            delay: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let l = FakeLauncher::default();
        let (first, second) = tokio::join!(run(&dl, &l, &work, Version(1, 0, 0)), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            run(&dl, &l, &work, Version(1, 0, 0)).await
        });
        assert!(first.is_ok());
        let err = second.unwrap_err();
        assert!(err.to_string().contains("обновление уже идёт"));
    }
}
