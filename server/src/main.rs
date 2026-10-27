use rcdesk_server::admin::admin_emails_from_env;
use rcdesk_server::app::{app_with_admins, spawn_pin_rotation, Liveness};
use rcdesk_server::db::Db;
use rcdesk_server::ice::IceConfig;
use rcdesk_server::mail::mailer_from_env;
use rcdesk_server::maintenance::spawn_maintenance;
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

    // Долг D53: суточная уборка (см. `maintenance`).
    let registry = Registry::new();
    spawn_maintenance(db.clone(), registry.clone());

    spawn_pin_rotation(registry.clone(), Liveness::default().pin_ttl);

    let webauthn = passkeys::webauthn_from_env()?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("rcdesk-server listening on {addr}");

    let shutdown_db = db.clone();
    let admin_emails = admin_emails_from_env();
    tracing::info!("admins configured: {}", admin_emails.len());
    let service = app_with_admins(
        registry,
        IceConfig::from_env(),
        db,
        webauthn,
        mailer_from_env()?,
        admin_emails,
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
