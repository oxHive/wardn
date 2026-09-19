//! Prometheus metrics for wardn — one global recorder for the whole
//! process, installed once by `main.rs`'s `cmd_serve`, and only when
//! `WARDN_METRICS_TOKEN` is set (see `src/config.rs`). Unset means metrics
//! collection is toggled off entirely: no recorder is installed and this
//! endpoint always 404s, rather than serving zeroed-out metrics or an
//! unauthenticated internals dump.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::serve::AppState;

/// Renders the process's Prometheus metrics as exposition-format text.
/// Requires a matching `Authorization: Bearer <token>` header against
/// `state.metrics_token` — an unauthenticated `/metrics` would let anyone
/// read live member/request-volume internals for no reason.
pub async fn metrics_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (Some(expected), Some(handle)) = (state.metrics_token.as_deref(), &state.metrics_handle)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // Constant-time comparison — a plain `==`/`!=` here would let a
    // network-adjacent attacker recover the metrics token byte-by-byte via
    // response timing. `ConstantTimeEq` short-circuits on length only,
    // which is fine since the token's length isn't the secret.
    let authorized = token.is_some_and(|t| bool::from(t.as_bytes().ct_eq(expected.as_bytes())));
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Ok(members) = crate::members::list(&state.conn).await {
        metrics::gauge!("wardn_members_total").set(members.len() as f64);
    }
    let body = handle.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

/// Records one `/v1/authorize` call's outcome:
/// `wardn_authorize_requests_total{action,allowed}`.
pub fn record_authorize(action: &'static str, allowed: bool) {
    metrics::counter!(
        "wardn_authorize_requests_total",
        "action" => action,
        "allowed" => if allowed { "true" } else { "false" },
    )
    .increment(1);
}
