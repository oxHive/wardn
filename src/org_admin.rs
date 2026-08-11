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

fn parse_permissions(raw: &[String]) -> Result<Vec<Permission>, Response> {
    raw.iter()
        .map(|s| {
            Permission::from_db_str(s)
                .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("unknown permission: {s}")).into_response())
        })
        .collect()
}

pub async fn create_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
    Json(req): Json<CreateRoleRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, owner.owner_id, Permission::OrgManageRoles)
            .await
    {
        return status.into_response();
    }
    let permissions = match parse_permissions(&req.permissions) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    match roles::create_role(&state.pool, org_id, &req.name, &permissions).await {
        Ok(role_id) => (
            StatusCode::CREATED,
            Json(RoleResponse {
                id: role_id,
                name: req.name,
                permissions: req.permissions,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("create role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn list_roles(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, owner.owner_id, Permission::OrgManageRoles)
            .await
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

pub async fn update_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, role_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateRoleRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, owner.owner_id, Permission::OrgManageRoles)
            .await
    {
        return status.into_response();
    }
    let permissions = match parse_permissions(&req.permissions) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    match roles::set_role_permissions(&state.pool, role_id, &permissions).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("update role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn delete_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, role_id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, owner.owner_id, Permission::OrgManageRoles)
            .await
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

pub async fn assign_member_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<AssignRoleRequest>,
) -> Response {
    if let Err(status) = roles::require_permission(
        &state.pool,
        org_id,
        owner.owner_id,
        Permission::OrgManageMembers,
    )
    .await
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
