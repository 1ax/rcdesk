use rcdesk_server::app::{app, spawn_pin_rotation, Liveness};
use rcdesk_server::db::{Db, DEVICE_IDLE_TTL_SECS, OWNER_IDLE_TTL_SECS};
use rcdesk_server::ice::{now_unix, IceConfig};
use rcdesk_server::mail::mailer_from_env;
use rcdesk_server::passkeys;
use rcdesk_server::registry::Registry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let port: u16 = std::env::var("RCDESK_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);

    let db_path = std::env::var("RCDESK_DB_PATH").unwrap_or_else(|_| "rcdesk.db".to_string());
    tracing::info!("opening database at {db_path}");
    let db = Db::open(std::path::Path::new(&db_path))?;

    // Долг D53: раз в сутки (и сразу при старте) чистим пустых анонимных
    // владельцев. Короткий запрос под std-мьютексом, как остальной код.
    let prune_db = db.clone();
    let registry = Registry::new();
    let prune_registry = registry.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
        loop {
            interval.tick().await;
            match prune_db.prune_empty_owners(now_unix(), OWNER_IDLE_TTL_SECS) {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} empty owners"),
                Err(err) => tracing::warn!("failed to prune empty owners: {err:#}"),
            }
            match prune_db.prune_unowned_devices(
                now_unix(),
                DEVICE_IDLE_TTL_SECS,
                &prune_registry.online_host_ids(),
            ) {
                Ok(0) => {}
                Ok(n) => tracing::info!("pruned {n} unowned devices"),
                Err(err) => tracing::warn!("failed to prune unowned devices: {err:#}"),
            }
        }
    });

    spawn_pin_rotation(registry.clone(), Liveness::default().pin_ttl);

    let webauthn = passkeys::webauthn_from_env()?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("rcdesk-server listening on {addr}");

    let shutdown_db = db.clone();
    let service = app(
        registry,
        IceConfig::from_env(),
        db,
        webauthn,
        mailer_from_env()?,
    )
    .into_make_service_with_connect_info::<std::net::SocketAddr>();

    // Без with_graceful_shutdown: он ждал бы закрытия долгоживущих WebSocket.
    // Клиенты и хосты переподключаются сами, поэтому по сигналу просто выходим.
    tokio::select! {
        result = axum::serve(listener, service) => result?,
        () = shutdown_signal() => {
            tracing::info!("shutdown signal received");
            match shutdown_db.checkpoint() {
                Ok((0, _, _)) => {}
                Ok((busy, log, done)) => tracing::warn!(
                    "wal checkpoint incomplete: busy={busy} log={log} checkpointed={done}"
                ),
                Err(err) => tracing::warn!("wal checkpoint failed: {err:#}"),
            }
        }
    }

    Ok(())
}

/// Ждёт SIGTERM (docker stop, PID 1) или Ctrl+C; на не-unix — только Ctrl+C.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(err) => {
                tracing::warn!("failed to install SIGTERM handler: {err}");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
