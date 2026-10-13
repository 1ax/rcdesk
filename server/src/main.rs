use rcdesk_server::app::app;
use rcdesk_server::db::{Db, DEVICE_IDLE_TTL_SECS, OWNER_IDLE_TTL_SECS};
use rcdesk_server::ice::{now_unix, IceConfig};
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

    let webauthn = passkeys::webauthn_from_env()?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("rcdesk-server listening on {addr}");

    axum::serve(
        listener,
        app(registry, IceConfig::from_env(), db, webauthn)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}
