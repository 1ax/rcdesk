use rcdesk_server::app::app;
use rcdesk_server::db::Db;
use rcdesk_server::ice::IceConfig;
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

    let webauthn = passkeys::webauthn_from_env()?;

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("rcdesk-server listening on {addr}");

    axum::serve(
        listener,
        app(Registry::new(), IceConfig::from_env(), db, webauthn)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}
