use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPool::connect(database_url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
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

pub async fn find_api_key_by_prefix(
    pool: &PgPool,
    prefix: &str,
) -> Result<Option<ApiKeyRow>, sqlx::Error> {
    sqlx::query_as::<_, ApiKeyRow>(
        "SELECT id, owner_type, owner_id, prefix, key_hash, revoked_at
         FROM api_keys WHERE prefix = $1",
    )
    .bind(prefix)
    .fetch_optional(pool)
    .await
}

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
