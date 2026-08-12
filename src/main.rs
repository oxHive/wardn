use std::time::Duration;

use hivemind_gateway::{AppState, app, config::Config, db, provisioning};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::EnvFilter;

/// How often the background provisioning worker retries pending outbox
/// rows. See `docs/superpowers/specs/2026-08-12-database-provisioning-design.md`.
const PROVISIONING_WORKER_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // JSON output for aggregator-friendly NDJSON, but still honouring
    // `RUST_LOG` — `fmt().json()` alone hard-wires the level floor to INFO
    // with no way to raise or lower verbosity in a deployed environment,
    // which the plain `fmt::init()` this replaced did support.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;

    let metrics_handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("install prometheus recorder");

    let worker_pool = pool.clone();
    let worker_admin_url = config.sqld_admin_url.clone();
    tokio::spawn(provisioning::run_worker(
        worker_pool,
        worker_admin_url,
        PROVISIONING_WORKER_INTERVAL,
    ));

    let state = AppState::new(pool, config.sqld_url.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone())
        .with_metrics_handle(metrics_handle);
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivemind-gateway listening on {}", config.listen_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
