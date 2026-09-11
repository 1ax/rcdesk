mod platform;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");

    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("{version}");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let platform = platform::name();
    tracing::info!("rcdesk-host {version} on {platform}");

    Ok(())
}
