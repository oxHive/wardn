use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use libsql::Connection;
use serde::{Deserialize, Serialize};

use crate::{api_keys, members, org};

/// The HTTP authorization service `wardn serve` runs — see the spec's "How
/// Wardn and Mynd Actually Interact" and "'CLI Only' Does Not Mean 'No
/// API'" sections. Its scope is deliberately narrow: authorization checks
/// and a health/status readout, nothing that resembles a general admin API.
#[derive(Clone)]
pub struct AppState {
    pub conn: Connection,
    pub started_at: i64,
    /// `Some` only when `WARDN_METRICS_TOKEN` is set — see
    /// `src/observability.rs`. `None` means metrics are toggled off
    /// entirely, not just unauthenticated.
    #[cfg(feature = "observability")]
    pub metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    #[cfg(feature = "observability")]
    pub metrics_token: Option<String>,
}

impl AppState {
    /// Metrics off by default — `main.rs`'s `cmd_serve` turns them on with
    /// `with_metrics` when `WARDN_METRICS_TOKEN` is configured. Every test
    /// and every non-`serve` code path just wants a plain state.
    pub fn new(conn: Connection, started_at: i64) -> Self {
        Self {
            conn,
            started_at,
            #[cfg(feature = "observability")]
            metrics_handle: None,
            #[cfg(feature = "observability")]
            metrics_token: None,
        }
    }

    #[cfg(feature = "observability")]
    pub fn with_metrics(
        mut self,
        handle: metrics_exporter_prometheus::PrometheusHandle,
        token: String,
    ) -> Self {
        self.metrics_handle = Some(handle);
        self.metrics_token = Some(token);
        self
    }
}

pub fn app(state: AppState) -> Router {
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/status", get(status))
        .route("/v1/authorize", post(authorize));

    #[cfg(feature = "observability")]
    let router = router
        .route("/metrics", get(crate::observability::metrics_handler))
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(make_span));

    router.with_state(state)
}

/// Creates the one-per-request span `TraceLayer` attaches below. Stamps a
/// `trace_id` field onto it immediately, read back from this span's own
/// OTel context — this is what makes every JSON log line emitted during the
/// request carry a `trace_id` (the fmt layer's default
/// `with_current_span(true)` includes it), which is what Loki's derived
/// field (`grafana/provisioning/datasources/loki.yml`) keys on to link a
/// log line to its Tempo trace. Works whether or not the OTel layer is
/// actually active (`main.rs`) — with no OTel layer registered,
/// `context().span().span_context()` is a valid-but-empty span context, and
/// `trace_id` renders as all-zeroes rather than failing.
#[cfg(feature = "observability")]
fn make_span(request: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let span = tracing::info_span!(
        "http_request",
        method = %request.method(),
        path = %request.uri().path(),
        trace_id = tracing::field::Empty,
    );
    let trace_id = span.context().span().span_context().trace_id();
    span.record("trace_id", tracing::field::display(trace_id));
    span
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct StatusResponse {
    org_name: Option<String>,
    member_count: i64,
    started_at: i64,
}

async fn status(State(state): State<AppState>) -> Response {
    let org = match org::get(&state.conn).await {
        Ok(org) => org,
        Err(e) => {
            tracing::error!("status: reading org failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let member_count = match members::list(&state.conn).await {
        Ok(members) => members.len() as i64,
        Err(e) => {
            tracing::error!("status: listing members failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Json(StatusResponse {
        org_name: org.map(|o| o.name),
        member_count,
        started_at: state.started_at,
    })
    .into_response()
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Read,
    Write,
}

#[derive(Deserialize)]
struct AuthorizeRequest {
    api_key: String,
    action: Action,
}

#[derive(Serialize)]
struct AuthorizeResponse {
    allowed: bool,
    member_id: Option<String>,
    role: Option<String>,
}

/// `POST /v1/authorize` — this is the one call Mynd makes before touching
/// org-layer memory: "is this member authorized to read/write this org's
/// memory, and at what role level." Wardn never sees or stores the memory
/// content itself.
async fn authorize(State(state): State<AppState>, Json(req): Json<AuthorizeRequest>) -> Response {
    let member = match api_keys::verify(&state.conn, &req.api_key).await {
        Ok(member) => member,
        Err(e) => {
            tracing::error!("authorize: key verification failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(member) = member else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(AuthorizeResponse {
                allowed: false,
                member_id: None,
                role: None,
            }),
        )
            .into_response();
    };
    let allowed = match req.action {
        Action::Read => member.role.can_read(),
        Action::Write => member.role.can_write(),
    };
    #[cfg(feature = "observability")]
    crate::observability::record_authorize(
        match req.action {
            Action::Read => "read",
            Action::Write => "write",
        },
        allowed,
    );
    Json(AuthorizeResponse {
        allowed,
        member_id: Some(member.id),
        role: Some(member.role.as_str().to_string()),
    })
    .into_response()
}
