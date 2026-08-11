use hivemind_gateway::db;
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

#[tokio::test]
async fn find_api_key_by_prefix_returns_seeded_row() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    let prefix = format!("t{}", owner_id.simple())[..12].to_string();
    sqlx::query(
        "INSERT INTO users (id, email) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(owner_id)
    .bind(format!("test-{owner_id}@example.com"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&prefix)
    .bind("dummy-hash")
    .execute(&pool)
    .await
    .unwrap();

    let row = db::find_api_key_by_prefix(&pool, &prefix)
        .await
        .unwrap()
        .expect("row should exist");
    assert_eq!(row.owner_type, "user");
    assert_eq!(row.owner_id, owner_id);
    assert_eq!(row.key_hash, "dummy-hash");
}

#[tokio::test]
async fn find_api_key_by_prefix_returns_none_when_missing() {
    let pool = test_pool().await;
    let row = db::find_api_key_by_prefix(&pool, "no-such-prefix")
        .await
        .unwrap();
    assert!(row.is_none());
}

#[tokio::test]
async fn find_database_mapping_returns_seeded_namespace() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'org', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(format!("ns-{owner_id}"))
    .execute(&pool)
    .await
    .unwrap();

    let ns = db::find_database_mapping(&pool, "org", owner_id)
        .await
        .unwrap()
        .expect("mapping should exist");
    assert_eq!(ns, format!("ns-{owner_id}"));
}
