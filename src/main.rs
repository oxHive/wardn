use hivemind_gateway::{AppState, app, config::Config, db};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    let state = AppState::new(pool, config.sqld_url.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivemind-gateway listening on {}", config.listen_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
