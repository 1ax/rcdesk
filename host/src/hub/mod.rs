//! LAN-хаб (D47-3): хост в одном процессе поднимает сигнальный сервер
//! (`rcdesk-server` библиотекой) и раздаёт вшитый веб-клиент по HTTPS с
//! самоподписанным сертификатом. Для изолированной сети без интернета и
//! `rcdesk.app`.
//!
//! Слушатели (один и тот же `Router`, общее состояние):
//! - TLS на `0.0.0.0:<port>` (по умолчанию 8443) -- для браузеров и других хостов;
//! - без TLS на `127.0.0.1:<local_port>` (по умолчанию 8444) -- для хоста на
//!   самом хабе (`serve --server ws://127.0.0.1:8444/ws`). Порт фиксированный
//!   намеренно: ключи устройства хоста хранятся по URL сервера
//!   (`DeviceStore::for_server`).
//!
//! Платформенного кода здесь нет.

pub mod cert;
pub mod tls;
pub mod web;

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::serve::ListenerExt as _;
use rcdesk_server::app::{lan_hub_app, spawn_pin_rotation, Liveness};
use rcdesk_server::db::Db;
use rcdesk_server::ice::IceConfig;
use rcdesk_server::mail::DisabledMailer;
use rcdesk_server::maintenance::spawn_maintenance;
use rcdesk_server::passkeys;
use rcdesk_server::registry::Registry;
use tokio::task::JoinHandle;

/// Порт HTTPS по умолчанию.
pub const DEFAULT_PORT: u16 = 8443;
/// Порт локального слушателя без TLS по умолчанию.
pub const DEFAULT_LOCAL_PORT: u16 = 8444;
/// Файл БД хаба в каталоге данных.
pub const DB_FILE: &str = "hub.db";

/// Параметры хаба. Порт 0 допустим (тесты): фактические адреса возвращает
/// [`start`].
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub data_dir: PathBuf,
    pub port: u16,
    pub local_port: u16,
}

/// Каталог данных хаба по умолчанию: `<пользовательский data_dir хоста>/hub`.
pub fn default_data_dir() -> PathBuf {
    crate::agent::paths::data_dir().join("hub")
}

/// Запущенный хаб: фактические адреса, отпечаток и фоновые задачи.
pub struct Hub {
    pub tls_addr: SocketAddr,
    pub local_addr: SocketAddr,
    /// SHA-256 от SubjectPublicKeyInfo сертификата, hex.
    pub fingerprint: String,
    db: Db,
    servers: Vec<JoinHandle<()>>,
    background: Vec<JoinHandle<()>>,
}

/// Открывает БД, готовит сертификат, занимает порты и запускает обслуживание.
pub async fn start(cfg: HubConfig) -> Result<Hub> {
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("не удалось создать каталог {}", cfg.data_dir.display()))?;
    let db = Db::open(&cfg.data_dir.join(DB_FILE)).context("не удалось открыть БД хаба")?;
    let registry = Registry::new();

    let computer_name = crate::app::resolve_host_name(None);
    let identity = cert::load_identity(&cfg.data_dir, &computer_name)?;
    let tls_config = tls::server_config(identity.cert_der, identity.key_der)?;

    // Изолированная сеть: ни STUN, ни TURN. Аккаунтов, passkey и почты нет
    // (клиенту об этом говорит `lan_hub`).
    let ice = IceConfig::new(Vec::new(), Vec::new(), String::new(), 0);
    let webauthn = passkeys::build_webauthn("localhost", "https://localhost")
        .context("не удалось собрать заглушку webauthn")?;
    let router = lan_hub_app(
        registry.clone(),
        ice,
        db.clone(),
        webauthn,
        Arc::new(DisabledMailer),
    )
    .fallback(web::handler);

    let tls_listener = tls::TlsListener::bind(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.port),
        tls_config,
    )
    .await?;
    let local_listener = tokio::net::TcpListener::bind(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        cfg.local_port,
    ))
    .await
    .with_context(|| format!("не удалось занять 127.0.0.1:{}", cfg.local_port))?;
    let tls_addr = axum::serve::Listener::local_addr(&tls_listener)?;
    let local_addr = local_listener.local_addr()?;

    // `ws_handler` берёт `ConnectInfo<SocketAddr>`. axum 0.8 реализует
    // `Connected<IncomingStream<..>>` для `SocketAddr` только у `TcpListener` и
    // у `TapIo<L, F>` (для любого `L: Listener`), поэтому свой TLS-слушатель
    // оборачивается в `tap_io` с пустым замыканием.
    let tls_service = router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    let local_service = router.into_make_service_with_connect_info::<SocketAddr>();
    let tls_server = tokio::spawn(async move {
        if let Err(err) = axum::serve(tls_listener.tap_io(|_| {}), tls_service).await {
            tracing::error!("hub: https server stopped: {err}");
        }
    });
    let local_server = tokio::spawn(async move {
        if let Err(err) = axum::serve(local_listener, local_service).await {
            tracing::error!("hub: local server stopped: {err}");
        }
    });

    let background = vec![
        spawn_maintenance(db.clone(), registry.clone()),
        spawn_pin_rotation(registry, Liveness::default().pin_ttl),
    ];

    tracing::info!("hub listening: https {tls_addr}, local http {local_addr}");
    tracing::info!("hub key fingerprint sha256:{}", identity.fingerprint);
    Ok(Hub {
        tls_addr,
        local_addr,
        fingerprint: identity.fingerprint,
        db,
        servers: vec![tls_server, local_server],
        background,
    })
}

impl Hub {
    /// Ждёт `shutdown` (или падения серверной задачи), затем останавливает всё
    /// и делает checkpoint WAL. Без graceful shutdown axum: он ждал бы закрытия
    /// долгоживущих WebSocket, клиенты и хосты переподключаются сами.
    pub async fn run_until(mut self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let mut crashed = false;
        {
            let (tls, local) = self.servers.split_at_mut(1);
            tokio::select! {
                () = shutdown => tracing::info!("hub: shutdown signal received"),
                _ = &mut tls[0] => crashed = true,
                _ = &mut local[0] => crashed = true,
            }
        }
        self.stop();
        if crashed {
            anyhow::bail!("сервер хаба остановился неожиданно");
        }
        Ok(())
    }

    /// Останавливает задачи и сбрасывает WAL в основной файл БД.
    pub fn stop(&self) {
        for task in self.servers.iter().chain(&self.background) {
            task.abort();
        }
        match self.db.checkpoint() {
            Ok((0, _, _)) => {}
            Ok((busy, log, done)) => {
                tracing::warn!(
                    "wal checkpoint incomplete: busy={busy} log={log} checkpointed={done}"
                )
            }
            Err(err) => tracing::warn!("wal checkpoint failed: {err:#}"),
        }
    }
}

/// Запуск и ожидание `shutdown` одним вызовом (для службы).
pub async fn run(cfg: HubConfig, shutdown: impl Future<Output = ()>) -> Result<()> {
    start(cfg).await?.run_until(shutdown).await
}

/// Ctrl+C (SIGINT) или, на unix, SIGTERM (остановка под systemd/launchd).
/// Под службой Windows остановку даёт [`stop_flag_signal`].
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    res = tokio::signal::ctrl_c() => {
                        if let Err(err) = res {
                            tracing::warn!("hub: не удалось подписаться на Ctrl+C: {err}");
                            std::future::pending::<()>().await;
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!("hub: не удалось подписаться на SIGTERM: {err}");
                if let Err(err) = tokio::signal::ctrl_c().await {
                    tracing::warn!("hub: не удалось подписаться на Ctrl+C: {err}");
                    std::future::pending::<()>().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!("hub: не удалось подписаться на Ctrl+C: {err}");
            std::future::pending::<()>().await;
        }
    }
}

/// Завершается, когда `flag` становится `true` (опрос раз в 200 мс). Для
/// службы Windows: флаг -- `STOP_REQUESTED`, выставляемый обработчиком SCM.
pub async fn stop_flag_signal(flag: &'static AtomicBool) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod signal_tests {
    use super::*;

    #[tokio::test]
    async fn stop_flag_signal_completes_after_the_flag_is_set() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let setter = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            FLAG.store(true, Ordering::SeqCst);
        });
        tokio::time::timeout(Duration::from_secs(2), stop_flag_signal(&FLAG))
            .await
            .expect("future должна завершиться после установки флага");
        setter.await.unwrap();
    }
}
