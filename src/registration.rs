use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{self, AppState};
use crate::provisioning::{self, OutboxRow};

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
}

#[derive(Serialize)]
pub struct CreateUserResponse {
    pub user_id: Uuid,
    pub api_key: String,
}

/// Inserts the new user, a fresh personal API key, and its provisioning
/// outbox row in one transaction. Returns the outbox row directly so the
/// caller can make the inline provisioning attempt without a second round
/// trip to read back what was just written.
async fn insert_user(
    pool: &PgPool,
    email: &str,
) -> Result<(Uuid, String, OutboxRow), sqlx::Error> {
    let user_id = Uuid::new_v4();
    let outbox_id = Uuid::new_v4();
    let namespace = user_id.to_string();
    let (full_key, prefix, hash) = auth::generate_api_key();

    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(email)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO namespace_provisioning_outbox (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'user', $2, $3)",
    )
    .bind(outbox_id)
    .bind(user_id)
    .bind(&namespace)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        user_id,
        full_key,
        OutboxRow {
            id: outbox_id,
            owner_type: "user".to_string(),
            owner_id: user_id,
            sqld_namespace: namespace,
            attempts: 0,
        },
    ))
}

/// `POST /users` — public, no `Authorization` header required: this is how a
/// caller gets their first API key at all. Registered after
/// `auth_middleware` in `app()` (`src/lib.rs`), the same place `/healthz`
/// lives, so it bypasses the auth layer entirely.
///
/// The account and key are committed before any sqld call is made, so a
/// failed or slow inline provisioning attempt never costs the caller their
/// account — see
/// `docs/superpowers/specs/2026-08-12-database-provisioning-design.md`.
pub async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Response {
    match insert_user(&state.pool, &req.email).await {
        Ok((user_id, api_key, outbox_row)) => {
            if let Err(e) = provisioning::attempt_provisioning(
                &state.pool,
                &state.sqld_admin_url,
                &outbox_row,
            )
            .await
            {
                tracing::error!("inline provisioning attempt failed: {e:#}");
            }
            (
                StatusCode::CREATED,
                Json(CreateUserResponse { user_id, api_key }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("create user failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
