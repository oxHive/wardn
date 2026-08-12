use std::time::Duration;

use hivemind_gateway::{AppState, app, config::Config, db, provisioning};

/// How often the background provisioning worker retries pending outbox
/// rows. See `docs/superpowers/specs/2026-08-12-database-provisioning-design.md`.
const PROVISIONING_WORKER_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().json().init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;

    let worker_pool = pool.clone();
    let worker_admin_url = config.sqld_admin_url.clone();
    tokio::spawn(provisioning::run_worker(
        worker_pool,
        worker_admin_url,
        PROVISIONING_WORKER_INTERVAL,
    ));

    let state = AppState::new(pool, config.sqld_url.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivemind-gateway listening on {}", config.listen_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
