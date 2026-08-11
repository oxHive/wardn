use hivemind_gateway::{app, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivemind-gateway listening on {}", config.listen_addr);
    axum::serve(listener, app()).await?;
    Ok(())
}
