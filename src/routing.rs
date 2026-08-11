use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::AuthedOwner;
use crate::db;

pub async fn resolve_namespace(pool: &PgPool, owner: &AuthedOwner) -> Result<String, StatusCode> {
    match db::find_database_mapping(pool, &owner.owner_type, owner.owner_id).await {
        Ok(Some(namespace)) => Ok(namespace),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("database mapping lookup failed: {e:#}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Resolves an org's shared namespace directly by `org_id`, bypassing the
/// authenticated owner entirely — used only after `roles::require_permission`
/// has already confirmed the caller is a member of this org with the right
/// permission (see `proxy::proxy_handler`).
pub async fn resolve_org_namespace(pool: &PgPool, org_id: Uuid) -> Result<String, StatusCode> {
    match db::find_database_mapping(pool, "org", org_id).await {
        Ok(Some(namespace)) => Ok(namespace),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("database mapping lookup failed: {e:#}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
