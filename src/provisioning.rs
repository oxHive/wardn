use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

/// A row from `namespace_provisioning_outbox`. Fetched by [`fetch_pending`],
/// and also produced directly by whichever handler inserted it
/// (`src/registration.rs`), so the inline attempt doesn't need a second
/// round trip to read back what it just wrote.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutboxRow {
    pub id: Uuid,
    pub owner_type: String,
    pub owner_id: Uuid,
    pub sqld_namespace: String,
    pub attempts: i32,
}

/// After this many failed attempts, a row stops being retried automatically
/// and needs manual intervention (same tier as this project's other manual
/// escape hatches, e.g. `scripts/reset-dev-db.sh`).
const MAX_PROVISIONING_ATTEMPTS: i32 = 10;

/// Attempts to provision `row`'s namespace: calls sqld's admin API to create
/// it, then — on success — records the `database_mappings` row and marks the
/// outbox row `done`, both in one transaction. On failure, bumps `attempts`
/// and records `last_error`, flipping to `failed` once
/// [`MAX_PROVISIONING_ATTEMPTS`] is reached.
///
/// The `Result` here is about the *bookkeeping*, not the provisioning
/// outcome: `Ok(true)` means this attempt succeeded, `Ok(false)` means it
/// didn't (and was recorded as such) — both are expected outcomes callers
/// don't need to treat specially. `Err` means Postgres itself failed while
/// recording the outcome, which is the genuinely exceptional case.
pub async fn attempt_provisioning(
    pool: &PgPool,
    sqld_admin_url: &str,
    row: &OutboxRow,
) -> Result<bool, sqlx::Error> {
    let client = reqwest::Client::new();
    let create_result = client
        .post(format!(
            "{}/v1/namespaces/{}/create",
            sqld_admin_url.trim_end_matches('/'),
            row.sqld_namespace
        ))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await;

    match create_result {
        Ok(resp) if resp.status().is_success() => {
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(Uuid::new_v4())
            .bind(&row.owner_type)
            .bind(row.owner_id)
            .bind(&row.sqld_namespace)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE namespace_provisioning_outbox
                 SET status = 'done', updated_at = now() WHERE id = $1",
            )
            .bind(row.id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(true)
        }
        Ok(resp) => {
            record_failure(
                pool,
                row,
                &format!("sqld admin API returned {}", resp.status()),
            )
            .await?;
            Ok(false)
        }
        Err(e) => {
            record_failure(pool, row, &format!("sqld admin API request failed: {e}")).await?;
            Ok(false)
        }
    }
}

async fn record_failure(pool: &PgPool, row: &OutboxRow, error: &str) -> Result<(), sqlx::Error> {
    let attempts = row.attempts + 1;
    let status = if attempts >= MAX_PROVISIONING_ATTEMPTS {
        "failed"
    } else {
        "pending"
    };
    sqlx::query(
        "UPDATE namespace_provisioning_outbox
         SET attempts = $1, last_error = $2, status = $3, updated_at = now()
         WHERE id = $4",
    )
    .bind(attempts)
    .bind(error)
    .bind(status)
    .bind(row.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetches up to `limit` outbox rows still awaiting provisioning, oldest
/// first — used by the background worker (`run_worker`, added in a later
/// task).
pub async fn fetch_pending(pool: &PgPool, limit: i64) -> Result<Vec<OutboxRow>, sqlx::Error> {
    sqlx::query_as::<_, OutboxRow>(
        "SELECT id, owner_type, owner_id, sqld_namespace, attempts
         FROM namespace_provisioning_outbox
         WHERE status = 'pending'
         ORDER BY created_at
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// How many rows the background worker attempts per tick.
const WORKER_BATCH_SIZE: i64 = 20;

/// Runs forever, retrying pending provisioning rows on a fixed interval.
/// Spawned once, in-process, alongside `axum::serve` (see `main.rs`) — no
/// external cron or separate worker binary. `interval` is a parameter
/// (rather than a hardcoded const) so tests can drive it on a much shorter
/// cycle than production's.
pub async fn run_worker(pool: PgPool, sqld_admin_url: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let rows = match fetch_pending(&pool, WORKER_BATCH_SIZE).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!("failed to fetch pending provisioning rows: {e:#}");
                continue;
            }
        };
        for row in rows {
            if let Err(e) = attempt_provisioning(&pool, &sqld_admin_url, &row).await {
                tracing::error!("background provisioning attempt failed: {e:#}");
            }
        }
    }
}
