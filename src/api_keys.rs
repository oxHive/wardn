use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::{self, AppState, AuthedOwner};

#[derive(Serialize, sqlx::FromRow)]
pub struct ApiKeySummary {
    pub id: Uuid,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct CreateKeyResponse {
    pub id: Uuid,
    pub api_key: String,
}

/// `GET /api-keys` — self-service only, `owner_type == "user"` (the same
/// guard `POST /orgs` already uses — a workspace/org-owned key has no
/// notion of "its own" additional keys in this model). The query filters on
/// the target row's `owner_type = 'user'` too, not just the caller's own —
/// `user_id` and `owner_id`/`owner_type` are independent columns, so a
/// future workspace/org-owned key with this caller's `user_id` set must not
/// leak into this listing. Never returns `key_hash` or anything that could
/// reconstruct the full key.
///
/// `ORDER BY created_at, id` — `id` is the tiebreaker so two keys sharing a
/// `created_at` (same transaction timestamp) still order deterministically;
/// tests and clients alike treat position 0 as "the oldest/registration key".
pub async fn list_keys(State(state): State<AppState>, Extension(owner): Extension<AuthedOwner>) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let rows: Result<Vec<ApiKeySummary>, sqlx::Error> = sqlx::query_as::<_, ApiKeySummary>(
        "SELECT id, prefix, created_at, revoked_at FROM api_keys
         WHERE user_id = $1 AND owner_type = 'user' ORDER BY created_at, id",
    )
    .bind(owner.owner_id)
    .fetch_all(&state.pool)
    .await;
    match rows {
        Ok(keys) => Json(keys).into_response(),
        Err(e) => {
            tracing::error!("list api keys failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /api-keys` — mints an additional personal key for the caller. The
/// full key is shown exactly once, exactly like `POST /users`'s
/// registration response.
pub async fn create_key(State(state): State<AppState>, Extension(owner): Extension<AuthedOwner>) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (full_key, prefix, hash) = auth::generate_api_key();
    let id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(id)
    .bind(owner.owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&state.pool)
    .await;
    match result {
        Ok(_) => (
            StatusCode::CREATED,
            Json(CreateKeyResponse {
                id,
                api_key: full_key,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("create api key failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `DELETE /api-keys/:id` — revokes one of the caller's own keys. `404` for
/// a key id that exists but isn't the caller's own — never `403`, so a
/// caller can't use the response to confirm another user's key id exists.
/// The `owner_type = 'user'` guard on the target row (in addition to the
/// caller-side check above) means a workspace/org-owned row can never be
/// revoked through this self-service endpoint even if it happened to share
/// this caller's `user_id`. Idempotent: revoking an already-revoked key
/// still `204`s.
pub async fn revoke_key(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(key_id): Path<Uuid>,
) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let result = sqlx::query(
        "UPDATE api_keys SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND owner_type = 'user'",
    )
    .bind(key_id)
    .bind(owner.owner_id)
    .execute(&state.pool)
    .await;
    match result {
        Ok(res) if res.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("revoke api key failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
