use axum::http::StatusCode;
use sqlx::PgPool;

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
