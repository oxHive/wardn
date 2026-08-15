use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPool::connect(database_url).await?;
    if let Err(e) = sqlx::migrate!("./migrations").run(&pool).await {
        tracing::error!(
            "database migration failed: {e}. If this is migration 0002's \
             sqld_namespace format check, a pre-existing database_mappings row \
             violates it — fix or delete that row (see scripts/reset-dev-db.sh \
             for a full dev-DB reset), then retry."
        );
        return Err(e.into());
    }
    Ok(pool)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub owner_type: String,
    pub owner_id: Uuid,
    pub prefix: String,
    pub key_hash: String,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Only matches a non-revoked key — `idx_api_keys_prefix`'s partial index
/// (`WHERE revoked_at IS NULL`) covers exactly this predicate, so a revoked
/// key's row is invisible here rather than filtered out by the caller. A
/// revoked key therefore looks identical to an unknown prefix to
/// `auth_middleware`, which is fine: both already return the same 401.
#[tracing::instrument(skip(pool, prefix))]
pub async fn find_api_key_by_prefix(
    pool: &PgPool,
    prefix: &str,
) -> Result<Option<ApiKeyRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeyRow>(
        "SELECT id, owner_type, owner_id, prefix, key_hash, revoked_at
         FROM api_keys WHERE prefix = $1 AND revoked_at IS NULL",
    )
    .bind(prefix)
    .fetch_optional(pool)
    .await
}

#[tracing::instrument(skip(pool))]
pub async fn find_database_mapping(
    pool: &PgPool,
    owner_type: &str,
    owner_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT sqld_namespace FROM database_mappings WHERE owner_type = $1 AND owner_id = $2",
    )
    .bind(owner_type)
    .bind(owner_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(ns,)| ns))
}
