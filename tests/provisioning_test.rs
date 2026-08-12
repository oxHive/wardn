use hivemind_gateway::db;
use hivemind_gateway::provisioning::{self, OutboxRow};
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

fn admin_url() -> String {
    std::env::var("SQLD_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".to_string())
}

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

async fn seed_outbox_row(pool: &sqlx::PgPool, owner_type: &str, sqld_namespace: &str) -> OutboxRow {
    let id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO namespace_provisioning_outbox (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(owner_type)
    .bind(owner_id)
    .bind(sqld_namespace)
    .execute(pool)
    .await
    .unwrap();
    OutboxRow {
        id,
        owner_type: owner_type.to_string(),
        owner_id,
        sqld_namespace: sqld_namespace.to_string(),
        attempts: 0,
    }
}

async fn outbox_status(pool: &sqlx::PgPool, id: Uuid) -> (String, i32) {
    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM namespace_provisioning_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    (status, attempts)
}

#[tokio::test]
async fn attempt_provisioning_succeeds_creates_mapping_and_marks_done() {
    let pool = test_pool().await;
    let namespace = format!("provtest-{}", Uuid::new_v4());
    let row = seed_outbox_row(&pool, "user", &namespace).await;

    let result = provisioning::attempt_provisioning(&pool, &admin_url(), &row)
        .await
        .unwrap();
    assert!(result);

    let mapping: Option<(String,)> = sqlx::query_as(
        "SELECT sqld_namespace FROM database_mappings WHERE owner_type = $1 AND owner_id = $2",
    )
    .bind(&row.owner_type)
    .bind(row.owner_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(mapping.unwrap().0, namespace);

    let (status, attempts) = outbox_status(&pool, row.id).await;
    assert_eq!(status, "done");
    assert_eq!(attempts, 0);

    delete_namespace(&namespace).await;
}

#[tokio::test]
async fn attempt_provisioning_records_failure_and_stays_pending() {
    let pool = test_pool().await;
    let namespace = format!("provfail-{}", Uuid::new_v4());
    let row = seed_outbox_row(&pool, "user", &namespace).await;

    // Unreachable admin URL — the sqld call itself fails.
    let result = provisioning::attempt_provisioning(&pool, "http://127.0.0.1:1", &row)
        .await
        .unwrap();
    assert!(!result);

    let (status, attempts) = outbox_status(&pool, row.id).await;
    assert_eq!(status, "pending");
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn attempt_provisioning_gives_up_after_max_attempts() {
    let pool = test_pool().await;
    let namespace = format!("provgiveup-{}", Uuid::new_v4());
    let mut row = seed_outbox_row(&pool, "user", &namespace).await;

    // Drive it to one attempt below the cap directly via the DB, then make
    // one more failing call — this is the call that should flip it to
    // `failed` rather than leaving it `pending` forever.
    sqlx::query("UPDATE namespace_provisioning_outbox SET attempts = 9 WHERE id = $1")
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();
    row.attempts = 9;

    provisioning::attempt_provisioning(&pool, "http://127.0.0.1:1", &row)
        .await
        .unwrap();

    let (status, attempts) = outbox_status(&pool, row.id).await;
    assert_eq!(status, "failed");
    assert_eq!(attempts, 10);
}

#[tokio::test]
async fn fetch_pending_returns_only_pending_rows() {
    let pool = test_pool().await;
    let ns_a = format!("fetchpend-a-{}", Uuid::new_v4());
    let ns_b = format!("fetchpend-b-{}", Uuid::new_v4());
    let row_a = seed_outbox_row(&pool, "user", &ns_a).await;
    let row_b = seed_outbox_row(&pool, "org", &ns_b).await;
    // Mark row_a done so it must not show up.
    sqlx::query("UPDATE namespace_provisioning_outbox SET status = 'done' WHERE id = $1")
        .bind(row_a.id)
        .execute(&pool)
        .await
        .unwrap();

    let pending = provisioning::fetch_pending(&pool, 100).await.unwrap();
    assert!(pending.iter().any(|r| r.id == row_b.id));
    assert!(!pending.iter().any(|r| r.id == row_a.id));
}
