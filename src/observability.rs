//! Prometheus metrics for hivewarden. One global recorder for the
//! whole process, installed once in `main.rs` — see `AppState::metrics_handle`
//! for why every other call site builds a local, uninstalled handle instead.

use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;

use crate::auth::AppState;

/// Renders the process's Prometheus metrics as exposition-format text.
/// Registered outside `auth_middleware` in `src/lib.rs` alongside
/// `/healthz`, so it checks its own bearer token here instead: an
/// unauthenticated `/metrics` would let anyone read live proxy traffic
/// volume, Postgres pool occupancy, and provisioning-queue depth — internals
/// useful for gauging load and timing an attack, not something this endpoint
/// should hand out for free. `state.metrics_token` empty (the default from
/// `AppState::new`) fails closed — every request is rejected until
/// `main.rs` sets a real token via `with_metrics_token`. On an authorized
/// call, it also refreshes the Postgres pool gauges (`refresh_pg_pool_gauges`)
/// as a side effect before rendering.
pub async fn metrics_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if state.metrics_token.is_empty() || token != Some(state.metrics_token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    refresh_pg_pool_gauges(&state.pool);
    let body = state.metrics_handle.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

/// Sets `gateway_pg_pool_size`/`gateway_pg_pool_idle` from the pool's current
/// state. Called at `/metrics` scrape time (`metrics_handler`) rather than on
/// a timer — `PgPool::size`/`num_idle` are synchronous, in-memory reads, so
/// there is no cost to computing them fresh on every scrape.
pub fn refresh_pg_pool_gauges(pool: &PgPool) {
    metrics::gauge!("gateway_pg_pool_size").set(pool.size() as f64);
    metrics::gauge!("gateway_pg_pool_idle").set(pool.num_idle() as f64);
}

/// Increments `gateway_proxy_requests_in_flight` on construction and
/// decrements it on drop — one guard per `proxy_handler` call, created before
/// any early return, so every exit path (a permission rejection, a namespace
/// lookup failure, a successful proxy, a timeout) decrements it exactly once
/// without each return site needing its own instrumentation.
pub struct InFlightGuard;

impl InFlightGuard {
    pub fn new() -> Self {
        metrics::gauge!("gateway_proxy_requests_in_flight").increment(1.0);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!("gateway_proxy_requests_in_flight").decrement(1.0);
    }
}

impl Default for InFlightGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Records one proxied request's outcome: a `gateway_proxy_requests_total`
/// increment and a `gateway_proxy_request_duration_seconds` observation.
/// Called only from the same two points in `proxy_handler` that already emit
/// a usage event (`src/proxy.rs`) — a request rejected before reaching sqld
/// never calls this, matching `gateway_proxy_requests_total`'s definition as
/// proxy traffic that actually reached sqld. `duration` measures request
/// entry to response *head*, not full transfer — on the `sync` path, where
/// the streaming gRPC body is the long-lived part, it says very little about
/// how long the request really took (same caveat `src/proxy.rs` documents
/// for its usage-event `duration_ms`).
///
/// Deliberately carries **no per-tenant label**. `namespace` was dropped
/// (security-hardening finding #4): every namespace is permanently retained
/// in the Prometheus recorder's memory with no eviction, and every namespace
/// is created by a `POST /users` call — an unbounded, externally-triggerable
/// memory-growth vector. Per-tenant attribution stays available through the
/// `usage`-target tracing events (`src/proxy.rs`'s `emit_usage_event`),
/// which already carry `namespace`/`owner_id`/`org_id` and are designed for
/// exactly this kind of high-cardinality data.
pub fn record_proxy_metrics(protocol: &'static str, status: StatusCode, duration: Duration) {
    let status_class = format!("{}xx", status.as_u16() / 100);
    metrics::counter!(
        "gateway_proxy_requests_total",
        "protocol" => protocol,
        "status_class" => status_class,
    )
    .increment(1);
    metrics::histogram!(
        "gateway_proxy_request_duration_seconds",
        "protocol" => protocol,
    )
    .record(duration.as_secs_f64());
}

/// A single reachability probe against `sqld_url`: any HTTP response at all
/// counts as up (this answers "is the network path and the sqld process
/// alive," not "is every endpoint healthy"). A connection failure or timeout
/// counts as down.
pub async fn check_sqld_up(client: &reqwest::Client, sqld_url: &str) -> bool {
    client.get(sqld_url).send().await.is_ok()
}

/// Increments `gateway_provisioning_attempts_total{outcome}` — called from
/// `attempt_provisioning` (`src/provisioning.rs`) with `"success"` or
/// `"failure"`.
pub fn record_provisioning_outcome(outcome: &'static str) {
    metrics::counter!("gateway_provisioning_attempts_total", "outcome" => outcome).increment(1);
}

/// Sets `gateway_provisioning_outbox_pending`/`_failed` from a fresh count of
/// `namespace_provisioning_outbox`. Called once per `run_worker` tick
/// (`src/provisioning.rs`) — a full `COUNT(*)`, not `fetch_pending`'s
/// batch-limited row count, so the gauge reflects the true queue depth even
/// when it exceeds one tick's batch size.
pub async fn refresh_provisioning_outbox_gauges(pool: &PgPool) -> Result<(), sqlx::Error> {
    let (pending,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM namespace_provisioning_outbox WHERE status = 'pending'",
    )
    .fetch_one(pool)
    .await?;
    let (failed,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM namespace_provisioning_outbox WHERE status = 'failed'",
    )
    .fetch_one(pool)
    .await?;
    metrics::gauge!("gateway_provisioning_outbox_pending").set(pending as f64);
    metrics::gauge!("gateway_provisioning_outbox_failed").set(failed as f64);
    Ok(())
}

/// Runs forever, probing `sqld_url` on `interval` and publishing the result
/// to the `gateway_sqld_up` gauge (`1.0` up, `0.0` down). Spawned once, in
/// production only, alongside the provisioning worker — see `main.rs`. Its
/// own interval, decoupled from `/metrics` scrape cadence, so a scrape never
/// blocks on a network call to sqld.
pub async fn sqld_health_check_loop(client: reqwest::Client, sqld_url: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let up = check_sqld_up(&client, &sqld_url).await;
        metrics::gauge!("gateway_sqld_up").set(if up { 1.0 } else { 0.0 });
    }
}
