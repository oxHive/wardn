use hivemind_gateway::{AppState, app, config::Config, db};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    let state = AppState { pool };
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivemind-gateway listening on {}", config.listen_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
