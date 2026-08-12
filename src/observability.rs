//! Prometheus metrics for hivemind-gateway. One global recorder for the
//! whole process, installed once in `main.rs` — see `AppState::metrics_handle`
//! for why every other call site builds a local, uninstalled handle instead.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

use crate::auth::AppState;

/// Renders the process's Prometheus metrics as exposition-format text.
/// Unauthenticated, registered outside `auth_middleware` in `src/lib.rs`
/// alongside `/healthz`.
pub async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics_handle.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}
