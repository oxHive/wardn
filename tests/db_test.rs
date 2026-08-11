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

/// sqld resolves a namespace from the label *before the first dot* of the
/// `Host` header, and the proxy formats the mapped namespace as
/// `{sqld_namespace}.local`. A `sqld_namespace` containing a dot would
/// therefore resolve to a *different* row's namespace — one owner reading
/// another owner's database through their own perfectly valid API key.
/// Whitespace is the same class of bug from the other end: it cannot be
/// encoded into an HTTP header value at all.
///
/// `migrations/0002_namespace_format_constraint.sql` makes both impossible to
/// store. This test asserts the constraint is actually live in the database
/// the gateway talks to, i.e. that the malicious value never gets far enough
/// to reach the proxy.
#[tokio::test]
async fn malformed_sqld_namespace_is_rejected_by_the_database() {
    let pool = test_pool().await;
    for bad in [
        "a.b",
        "victimns.attacker",
        "has space",
        "has\nnewline",
        "UPPERCASE",
        "",
    ] {
        let result = sqlx::query(
            "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
             VALUES ($1, 'user', $2, $3)",
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(bad)
        .execute(&pool)
        .await;

        let err = result
            .err()
            .unwrap_or_else(|| panic!("inserting sqld_namespace {bad:?} should have been rejected"));
        assert!(
            err.to_string().contains("sqld_namespace_format"),
            "sqld_namespace {bad:?} was rejected, but not by the format CHECK constraint: {err}"
        );
    }
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
