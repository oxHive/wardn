//! Prometheus metrics for hivemind-gateway. One global recorder for the
//! whole process, installed once in `main.rs` — see `AppState::metrics_handle`
//! for why every other call site builds a local, uninstalled handle instead.

use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::auth::AppState;

/// Renders the process's Prometheus metrics as exposition-format text.
/// Registered outside `auth_middleware` in `src/lib.rs` alongside
/// `/healthz`, so it checks its own bearer token here instead: the
/// per-tenant `namespace` label on proxy metrics (`record_proxy_metrics`)
/// would otherwise let anyone enumerate every tenant's UUID and traffic
/// volume through this endpoint. `state.metrics_token` empty (the default
/// from `AppState::new`) fails closed — every request is rejected until
/// `main.rs` sets a real token via `with_metrics_token`.
pub async fn metrics_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if state.metrics_token.is_empty() || token != Some(state.metrics_token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let body = state.metrics_handle.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
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
/// proxy traffic that actually reached sqld.
pub fn record_proxy_metrics(
    protocol: &'static str,
    namespace: &str,
    status: StatusCode,
    duration: Duration,
) {
    let status_class = format!("{}xx", status.as_u16() / 100);
    metrics::counter!(
        "gateway_proxy_requests_total",
        "protocol" => protocol,
        "status_class" => status_class,
        "namespace" => namespace.to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "gateway_proxy_request_duration_seconds",
        "protocol" => protocol,
        "namespace" => namespace.to_string(),
    )
    .record(duration.as_secs_f64());
}
