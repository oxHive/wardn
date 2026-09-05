use std::time::Duration;

use hivewarden::{AppState, app, config::Config, db, observability, provisioning, telemetry};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// How often the background provisioning worker retries pending outbox
/// rows. See `docs/superpowers/specs/2026-08-12-database-provisioning-design.md`.
const PROVISIONING_WORKER_INTERVAL: Duration = Duration::from_secs(30);

/// How often the background health-check loop probes sqld for reachability.
/// See `observability::sqld_health_check_loop`.
const SQLD_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `Config::from_env` must run before the subscriber is built — whether
    // the OTel/Loki layers exist at all depends on its output.
    let config = Config::from_env()?;

    // The OTel layer and the Loki layer are each `Option<Layer>` — a blanket
    // impl in tracing_subscriber makes `Option<L>: Layer<S>` a no-op when
    // `None`, so the subscriber degrades cleanly to today's stdout-JSON-only
    // behavior when neither OTEL_EXPORTER_OTLP_ENDPOINT nor LOKI_URL is set.
    let otel_layer = config
        .otel_exporter_otlp_endpoint
        .as_deref()
        .map(telemetry::init_tracer)
        .transpose()?
        .map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer));

    let mut loki_task = None;
    let loki_layer = match config.loki_url.as_deref() {
        Some(loki_url) => {
            let (layer, task) = telemetry::init_loki_layer(loki_url)?;
            loki_task = Some(task);
            Some(layer)
        }
        None => None,
    };

    // JSON output for aggregator-friendly NDJSON, but still honouring
    // `RUST_LOG` — `fmt().json()` alone hard-wires the level floor to INFO
    // with no way to raise or lower verbosity in a deployed environment,
    // which the plain `fmt::init()` this replaced did support.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .with(otel_layer)
        .with(loki_layer)
        .init();

    if let Some(task) = loki_task {
        tokio::spawn(task);
    }

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

    let health_check_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client construction with these settings cannot fail");
    tokio::spawn(observability::sqld_health_check_loop(
        health_check_client,
        config.sqld_url.clone(),
        SQLD_HEALTH_CHECK_INTERVAL,
    ));

    let state = AppState::new(pool, config.sqld_url.clone(), config.api_key_pepper.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone())
        .with_metrics_handle(metrics_handle)
        .with_metrics_token(config.metrics_token.clone())
        .with_cors_origins(config.console_origins.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivewarden listening on {}", config.listen_addr);
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// Resolves on SIGINT (Ctrl+C, works everywhere) or SIGTERM (the signal
/// `podman stop`/Kubernetes send, Unix-only — there is no non-Unix
/// equivalent for `axum::serve` to wait on). Without this, either signal
/// kills the process immediately: in-flight requests get their connection
/// cut mid-response, and the provisioning worker/health-check loop (spawned
/// tasks, not part of `axum::serve`) are aborted wherever they happened to
/// be, mid-outbox-row-update included.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining in-flight requests");
}
