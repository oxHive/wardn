use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{AppState, AuthedOwner};
use crate::org::admin::is_unique_violation;
use crate::roles::{self, Permission};

#[derive(Deserialize)]
pub struct AddMemberRequest {
    pub email: String,
    pub role_id: Uuid,
}

#[derive(Serialize)]
pub struct MemberResponse {
    pub user_id: Uuid,
    pub role_id: Uuid,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct MemberSummary {
    pub user_id: Uuid,
    pub email: String,
    pub role_id: Uuid,
}

/// `GET /orgs/:org_id/members` — lists the org's members, requiring the same
/// `org:manage_members` permission as add/remove. Without this, an admin who
/// knows a departing member's *email* has no HTTP route to the `user_id` that
/// `DELETE /orgs/:org_id/members/:user_id` requires — the offboarding tool the
/// design spec chose over key revocation would be undrivable from this
/// feature's own API surface.
pub async fn list_members(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }

    let rows: Result<Vec<MemberSummary>, sqlx::Error> = sqlx::query_as::<_, MemberSummary>(
        "SELECT om.user_id, u.email, om.role_id
         FROM org_members om
         JOIN users u ON u.id = om.user_id
         WHERE om.org_id = $1
         ORDER BY u.email",
    )
    .bind(org_id)
    .fetch_all(&state.pool)
    .await;
    match rows {
        Ok(members) => Json(members).into_response(),
        Err(e) => {
            tracing::error!("list members failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /orgs/:org_id/members` — adds an *already-registered* user to the
/// org by email. There is no pending-invitation state: the email must
/// already match a `users` row (`404` if not), and the caller must hold
/// `org:manage_members`.
pub async fn add_member(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }

    let user_row: Result<Option<(Uuid,)>, sqlx::Error> =
        // Case-insensitive: an exact match would `404` ("not registered") for
        // `Foo@Example.com` when `foo@example.com` *is* registered, which
        // misleads the admin into thinking the invitee needs to sign up.
        sqlx::query_as("SELECT id FROM users WHERE lower(email) = lower($1)")
            .bind(&req.email)
            .fetch_optional(&state.pool)
            .await;
    let user_id = match user_row {
        Ok(Some((id,))) => id,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("user lookup by email failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let role_row: Result<Option<(Uuid,)>, sqlx::Error> =
        sqlx::query_as("SELECT id FROM roles WHERE id = $1 AND org_id = $2")
            .bind(req.role_id)
            .bind(org_id)
            .fetch_optional(&state.pool)
            .await;
    match role_row {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("role lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    match sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(req.role_id)
        .execute(&state.pool)
        .await
    {
        Ok(_) => (
            StatusCode::CREATED,
            Json(MemberResponse {
                user_id,
                role_id: req.role_id,
            }),
        )
            .into_response(),
        Err(e) if is_unique_violation(&e) => {
            (StatusCode::CONFLICT, "user is already a member of this org").into_response()
        }
        Err(e) => {
            tracing::error!("add member failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `DELETE /orgs/:org_id/members/:user_id` — the offboarding tool. Deletes
/// only the `org_members` row; never touches `users`/`api_keys`, so the
/// removed member's personal key and any other org membership are
/// untouched.
pub async fn remove_member(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }

    let result = sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
        .bind(org_id)
        .bind(user_id)
        .execute(&state.pool)
        .await;
    match result {
        Ok(res) if res.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("remove member failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
