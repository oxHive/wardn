use std::collections::HashSet;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{AppState, AuthedOwner};
use crate::roles::{self, Permission};

#[derive(Deserialize)]
pub struct CreateRoleRequest {
    pub name: String,
    pub permissions: Vec<String>,
}

#[derive(Deserialize)]
pub struct UpdateRoleRequest {
    pub permissions: Vec<String>,
}

#[derive(Deserialize)]
pub struct AssignRoleRequest {
    pub role_id: Uuid,
}

#[derive(Serialize)]
pub struct RoleResponse {
    pub id: Uuid,
    pub name: String,
    pub permissions: Vec<String>,
}

/// Parses the request's permission strings against the fixed catalog, and
/// **deduplicates** them: `role_permissions` is `PRIMARY KEY (role_id,
/// permission)`, so `["db:query","db:query"]` reaching the insert loop is a
/// unique violation and — before this — a 500 for what is a perfectly
/// expressible, harmless request. A repeated permission means exactly the
/// same role as the single one, so it's collapsed rather than rejected.
///
/// First-seen order is preserved so the 201 body echoes the caller's ordering
/// rather than a `HashSet`'s arbitrary one.
fn parse_permissions(raw: &[String]) -> Result<Vec<Permission>, Response> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let permission = Permission::from_db_str(s).ok_or_else(|| {
            (StatusCode::BAD_REQUEST, format!("unknown permission: {s}")).into_response()
        })?;
        if seen.insert(permission) {
            out.push(permission);
        }
    }
    Ok(out)
}

/// A duplicate role name in the same org hits `roles`' `UNIQUE (org_id,
/// name)`. That's a client-correctable conflict, not a gateway fault, so it
/// gets 409 rather than the blanket 500 every other `sqlx::Error` gets.
///
/// `pub(crate)` because `src/registration.rs` applies the identical rule to
/// `users.email`'s uniqueness — same pattern as `roles::insert_role_permissions`
/// being widened for reuse rather than copied.
pub(crate) fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .map(|e| e.is_unique_violation())
        .unwrap_or(false)
}

/// Like [`is_unique_violation`], but only true when the violation is on
/// exactly this named constraint or index. Needed wherever the failing
/// statement runs inside a multi-insert transaction that could violate more
/// than one unique constraint — `is_unique_violation` alone can't tell a
/// targeted conflict (e.g. a duplicate email) apart from an unrelated one
/// raised by the same transaction (e.g. an astronomically rare
/// `api_keys.prefix` collision in `registration::create_user`), which would
/// otherwise get mapped to a misleading 409 message.
pub(crate) fn is_unique_violation_on(err: &sqlx::Error, constraint: &str) -> bool {
    err.as_database_error()
        .is_some_and(|e| e.is_unique_violation() && e.constraint() == Some(constraint))
}

/// Widened the same way `is_unique_violation` is, for `roles::delete_role`
/// and `org::members::add_member`: both have a check-then-act window where
/// the referenced role can be deleted or referenced concurrently, and
/// catching the resulting foreign-key violation turns what would otherwise
/// be a bare 500 into the same outcome a non-racing caller would have seen
/// (`Conflict`/`NotFound`).
pub(crate) fn is_foreign_key_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .map(|e| e.is_foreign_key_violation())
        .unwrap_or(false)
}

#[tracing::instrument(skip(state, req))]
pub async fn create_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
    Json(req): Json<CreateRoleRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageRoles).await
    {
        return status.into_response();
    }
    let permissions = match parse_permissions(&req.permissions) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    match roles::create_role(&state.pool, org_id, &req.name, &permissions).await {
        // Built from the *deduplicated* `permissions`, not `req.permissions`
        // — echoing the request back would claim two permissions were stored
        // for `["db:query","db:query"]` when only one was.
        Ok(role_id) => (
            StatusCode::CREATED,
            Json(RoleResponse {
                id: role_id,
                name: req.name,
                permissions: permissions
                    .into_iter()
                    .map(|p| p.as_db_str().to_string())
                    .collect(),
            }),
        )
            .into_response(),
        Err(e) if is_unique_violation_on(&e, "roles_org_id_name_key") => {
            (StatusCode::CONFLICT, "role name already exists in this org").into_response()
        }
        Err(e) => {
            tracing::error!("create role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[tracing::instrument(skip(state))]
pub async fn list_roles(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageRoles).await
    {
        return status.into_response();
    }
    match roles::list_roles(&state.pool, org_id).await {
        Ok(found) => Json(
            found
                .into_iter()
                .map(|r| RoleResponse {
                    id: r.id,
                    name: r.name,
                    permissions: r.permissions,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!("list roles failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[tracing::instrument(skip_all)]
pub async fn update_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, role_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateRoleRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageRoles).await
    {
        return status.into_response();
    }
    let permissions = match parse_permissions(&req.permissions) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // `set_role_permissions` is org-scoped and does its own existence check
    // in the same transaction as the rewrite, so a nonexistent or wrong-org
    // `role_id` is `NotFound` here rather than a silent 204.
    match roles::set_role_permissions(&state.pool, org_id, role_id, &permissions).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(roles::SetRolePermissionsError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(roles::SetRolePermissionsError::Db(e)) if is_unique_violation(&e) => {
            (StatusCode::CONFLICT, "role name already exists in this org").into_response()
        }
        Err(roles::SetRolePermissionsError::Db(e)) => {
            tracing::error!("update role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[tracing::instrument(skip_all)]
pub async fn delete_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, role_id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageRoles).await
    {
        return status.into_response();
    }
    match roles::delete_role(&state.pool, org_id, role_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(roles::DeleteRoleError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(roles::DeleteRoleError::InUse) => StatusCode::CONFLICT.into_response(),
        Err(roles::DeleteRoleError::Db(e)) => {
            tracing::error!("delete role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[tracing::instrument(skip_all)]
pub async fn assign_member_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<AssignRoleRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }
    match roles::assign_member_role(&state.pool, org_id, user_id, req.role_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("assign member role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
