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
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/status", get(status))
        .route("/v1/authorize", post(authorize))
        .with_state(state)
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

#[derive(Deserialize, PartialEq, Eq)]
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
    Json(AuthorizeResponse {
        allowed,
        member_id: Some(member.id),
        role: Some(member.role.as_str().to_string()),
    })
    .into_response()
}
