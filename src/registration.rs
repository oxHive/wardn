use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{self, AppState, AuthedOwner};
use crate::org::admin::is_unique_violation_on;
use crate::provisioning::{self, OutboxRow};
use crate::roles;

/// Validates and lowercases an email address before it reaches the
/// database. Deliberately simple (not a full RFC 5321 parser): non-empty,
/// within RFC 5321's 254-character maximum, exactly one `@` with non-empty
/// local and domain parts, no control characters or whitespace. Returns the
/// normalized (lowercased) form on success, or the 400 response to return
/// directly on failure.
// `Response` as the error type is the idiom for "the 4xx to return as-is" in
// an axum handler helper — called once per request, never buffered or moved
// in bulk, so `clippy::result_large_err`'s stack-bloat concern doesn't apply.
#[allow(clippy::result_large_err)]
fn validate_and_normalize_email(email: &str) -> Result<String, Response> {
    let bad_request = || (StatusCode::BAD_REQUEST, "invalid email address").into_response();

    if email.is_empty() || email.len() > 254 {
        return Err(bad_request());
    }
    if email.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(bad_request());
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad_request());
    };
    if local.is_empty() || domain.is_empty() {
        return Err(bad_request());
    }
    Ok(email.to_lowercase())
}

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
#[tracing::instrument(skip(pool, email, api_key_pepper))]
async fn insert_user(
    pool: &PgPool,
    email: &str,
    api_key_pepper: &[u8],
) -> Result<(Uuid, String, OutboxRow), sqlx::Error> {
    let user_id = Uuid::new_v4();
    let outbox_id = Uuid::new_v4();
    let namespace = user_id.to_string();
    let (full_key, prefix, hash) = auth::generate_api_key(api_key_pepper);

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
///
/// Re-registering an already-known address hits `users.email`'s `UNIQUE`, and
/// is by far the most common client error on this endpoint — it gets a 409 so
/// it's distinguishable from a genuine gateway fault both to the caller and in
/// the error logs, exactly as `org_admin::create_role` treats a duplicate role
/// name.
pub async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Response {
    let email = match validate_and_normalize_email(&req.email) {
        Ok(email) => email,
        Err(resp) => return resp,
    };
    match insert_user(&state.pool, &email, state.api_key_pepper.as_bytes()).await {
        Ok((user_id, api_key, outbox_row)) => {
            if let Err(e) =
                provisioning::attempt_provisioning(&state.pool, &state.sqld_admin_url, &outbox_row)
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
        // Two constraints can raise this: `users_email_key` (the original,
        // case-sensitive UNIQUE) and `users_email_lower_idx` (the
        // case-insensitive index added in migration 0005) — either means
        // the same thing to the caller, a taken email.
        Err(e)
            if is_unique_violation_on(&e, "users_email_key")
                || is_unique_violation_on(&e, "users_email_lower_idx") =>
        {
            (StatusCode::CONFLICT, "email already registered").into_response()
        }
        Err(e) => {
            tracing::error!("create user failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CreateOrgRequest {
    pub name: String,
}

#[derive(Serialize)]
pub struct CreateOrgResponse {
    pub org_id: Uuid,
}

/// Maximum orgs a single user may create. This project has no billing/plan
/// tiers yet to derive a real number from — 10 is a conservative ceiling
/// against runaway namespace creation, not a product decision about how many
/// orgs a legitimate customer needs.
const ORG_QUOTA_PER_USER: i64 = 10;

enum CreateOrgError {
    QuotaExceeded,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for CreateOrgError {
    fn from(err: sqlx::Error) -> Self {
        CreateOrgError::Db(err)
    }
}

/// Inserts the new org, a bootstrap `owner` role holding every permission in
/// the catalog, the creator's membership in that role, and the org's
/// provisioning outbox row — all in one transaction. Reuses
/// `roles::insert_role_permissions` for the permission-attach loop rather
/// than duplicating it, but can't reuse `roles::create_role` itself since
/// that function opens its own transaction and this one needs everything
/// atomic with the org insert.
///
/// The `ORG_QUOTA_PER_USER` check also happens inside this transaction, after
/// a transaction-scoped advisory lock keyed on `creator_user_id`: without the
/// lock, the count-then-insert is check-then-act, so concurrent `POST /orgs`
/// calls from the same user could all observe a count under the quota and
/// all insert, exceeding it. The lock serializes concurrent callers with the
/// same `creator_user_id` and is released automatically on commit or
/// rollback, so an early return (e.g. `QuotaExceeded`) can't leak it.
#[tracing::instrument(skip(pool))]
async fn insert_org(
    pool: &PgPool,
    name: &str,
    creator_user_id: Uuid,
) -> Result<(Uuid, OutboxRow), CreateOrgError> {
    let org_id = Uuid::new_v4();
    let role_id = Uuid::new_v4();
    let outbox_id = Uuid::new_v4();
    let namespace = org_id.to_string();

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
        .bind(creator_user_id.to_string())
        .execute(&mut *tx)
        .await?;
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM orgs WHERE created_by = $1")
        .bind(creator_user_id)
        .fetch_one(&mut *tx)
        .await?;
    if count >= ORG_QUOTA_PER_USER {
        return Err(CreateOrgError::QuotaExceeded);
    }
    sqlx::query("INSERT INTO orgs (id, name, created_by) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(name)
        .bind(creator_user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO roles (id, org_id, name) VALUES ($1, $2, 'owner')")
        .bind(role_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    roles::insert_role_permissions(&mut tx, role_id, &roles::ALL_PERMISSIONS).await?;
    sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(creator_user_id)
        .bind(role_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO namespace_provisioning_outbox (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'org', $2, $3)",
    )
    .bind(outbox_id)
    .bind(org_id)
    .bind(&namespace)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        org_id,
        OutboxRow {
            id: outbox_id,
            owner_type: "org".to_string(),
            owner_id: org_id,
            sqld_namespace: namespace,
            attempts: 0,
        },
    ))
}

/// `POST /orgs` — authenticated (goes through `auth_middleware` normally,
/// reads the caller via the `AuthedOwner` extension). Only a personal
/// (`owner_type == "user"`) key may create an org — a workspace/org-owned
/// key's `owner_id` is not a user id and could never legitimately become an
/// `org_members.user_id`, the same reasoning `roles::require_permission`
/// already applies to the org-shared-namespace proxy path.
///
/// Mints no second API key: the caller's existing personal key, plus
/// `X-Org-Id: <org_id>`, already reaches the new org's namespace once this
/// makes them its `owner`.
pub async fn create_org(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Json(req): Json<CreateOrgRequest>,
) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    match insert_org(&state.pool, &req.name, owner.owner_id).await {
        Ok((org_id, outbox_row)) => {
            if let Err(e) =
                provisioning::attempt_provisioning(&state.pool, &state.sqld_admin_url, &outbox_row)
                    .await
            {
                tracing::error!("inline provisioning attempt failed: {e:#}");
            }
            (StatusCode::CREATED, Json(CreateOrgResponse { org_id })).into_response()
        }
        Err(CreateOrgError::QuotaExceeded) => StatusCode::TOO_MANY_REQUESTS.into_response(),
        Err(CreateOrgError::Db(e)) => {
            tracing::error!("create org failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
