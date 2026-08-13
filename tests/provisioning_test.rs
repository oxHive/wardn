use hivewarden::db;
use hivewarden::provisioning::{self, OutboxRow};
use uuid::Uuid;

mod common;

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
    // Without the lock, the worker test's `run_worker` can create this
    // namespace first — sqld then answers *this* call with a 400 (namespace
    // already exists) and `result` is false.
    let _lock = common::lock_outbox(&pool).await;
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
    common::delete_outbox_row(&pool, row.id).await;
}

#[tokio::test]
async fn attempt_provisioning_records_failure_and_stays_pending() {
    let pool = test_pool().await;
    // This test's whole point is a row that stays `pending`; the worker test
    // would happily provision it out from under the assertion below.
    let _lock = common::lock_outbox(&pool).await;
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

    common::delete_outbox_row(&pool, row.id).await;
}

#[tokio::test]
async fn record_failure_does_not_resurrect_a_row_another_attempt_already_finished() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let namespace = format!("provrace-{}", Uuid::new_v4());
    let row = seed_outbox_row(&pool, "user", &namespace).await;

    // Stand in for the worker having won the race while this attempt was
    // still in flight: the row is already `done` (namespace created, mapping
    // written) by the time the loser's failure comes back.
    sqlx::query("UPDATE namespace_provisioning_outbox SET status = 'done' WHERE id = $1")
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();

    // `row` is the loser's stale in-memory copy, still claiming attempts = 0.
    provisioning::attempt_provisioning(&pool, "http://127.0.0.1:1", &row)
        .await
        .unwrap();

    let (status, attempts) = outbox_status(&pool, row.id).await;
    assert_eq!(status, "done", "a late failure dragged a done row back");
    assert_eq!(attempts, 0);

    common::delete_outbox_row(&pool, row.id).await;
}

#[tokio::test]
async fn attempt_provisioning_gives_up_after_max_attempts() {
    let pool = test_pool().await;
    // Holds `pending` at attempts = 9 between the seed and the final call; a
    // concurrent worker success would flip it to `done` and the guard in
    // `record_failure` would (correctly) refuse to move it to `failed`.
    let _lock = common::lock_outbox(&pool).await;
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

    common::delete_outbox_row(&pool, row.id).await;
}

#[tokio::test]
async fn fetch_pending_returns_only_pending_rows() {
    let pool = test_pool().await;
    // `row_b` has to still be `pending` when the assertion runs.
    let _lock = common::lock_outbox(&pool).await;
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

    common::delete_outbox_row(&pool, row_a.id).await;
    common::delete_outbox_row(&pool, row_b.id).await;
}

use std::time::Duration;

/// The worker's polling loop, driven end to end against the real sqld.
///
/// Holds the outbox lock for its whole life: `run_worker` uses the unscoped
/// `fetch_pending`, so while it runs it will provision *any* `pending` row in
/// the shared dev database, and the lock is what keeps sibling tests (in this
/// binary and in `registration_test`) from having a row seeded underneath it.
///
/// The row is seeded *after* the worker is already running, which is what
/// makes this a genuine multi-tick test: the first ticks find nothing of ours,
/// and only a later one does the work. A worker that only ever ran its first
/// tick would fail here.
#[tokio::test]
async fn run_worker_eventually_provisions_a_row_the_inline_attempt_missed() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let namespace = format!("workerrecover-{}", Uuid::new_v4());

    tokio::spawn(provisioning::run_worker(
        pool.clone(),
        admin_url(),
        Duration::from_millis(200),
    ));

    // Let the first couple of ticks run against a table with no row of ours.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Seed as if an inline attempt already failed once: pending, attempts=1.
    let row = seed_outbox_row(&pool, "user", &namespace).await;
    sqlx::query("UPDATE namespace_provisioning_outbox SET attempts = 1 WHERE id = $1")
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(700)).await;

    let (status, _attempts) = outbox_status(&pool, row.id).await;
    assert_eq!(status, "done");
    let mapping: Option<(String,)> = sqlx::query_as(
        "SELECT sqld_namespace FROM database_mappings WHERE owner_type = 'user' AND owner_id = $1",
    )
    .bind(row.owner_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(mapping.unwrap().0, namespace);

    delete_namespace(&namespace).await;
    common::delete_outbox_row(&pool, row.id).await;
}

#[tokio::test]
async fn attempt_provisioning_records_outcome_metrics() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let handle = common::metrics_handle();
    let namespace = format!("provmetrics-{}", Uuid::new_v4());
    let row = seed_outbox_row(&pool, "user", &namespace).await;

    let before = common::extract_labeled_metric(
        &handle.render(),
        "gateway_provisioning_attempts_total",
        &["outcome=\"success\""],
    )
    .unwrap_or(0.0);

    let result = provisioning::attempt_provisioning(&pool, &admin_url(), &row)
        .await
        .unwrap();
    assert!(result);

    let after = common::extract_labeled_metric(
        &handle.render(),
        "gateway_provisioning_attempts_total",
        &["outcome=\"success\""],
    )
    .unwrap_or(0.0);
    assert_eq!(after, before + 1.0);

    delete_namespace(&namespace).await;
    common::delete_outbox_row(&pool, row.id).await;
}

#[tokio::test]
async fn run_worker_tick_refreshes_outbox_gauges() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let handle = common::metrics_handle();
    let namespace = format!("provgauge-{}", Uuid::new_v4());
    let row = seed_outbox_row(&pool, "user", &namespace).await;

    tokio::spawn(provisioning::run_worker(
        pool.clone(),
        admin_url(),
        Duration::from_millis(200),
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;

    let rendered = handle.render();
    assert!(rendered.contains("gateway_provisioning_outbox_pending "));
    assert!(rendered.contains("gateway_provisioning_outbox_failed "));

    let (status, _attempts) = outbox_status(&pool, row.id).await;
    assert_eq!(status, "done");

    delete_namespace(&namespace).await;
    common::delete_outbox_row(&pool, row.id).await;
}
