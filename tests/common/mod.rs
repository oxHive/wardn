//! Helpers shared by the integration-test binaries.
//!
//! Cargo runs the test functions inside one binary concurrently, **and** runs
//! separate integration-test binaries in parallel — while every one of them
//! points at the same dev Postgres and the same sqld.
//!
//! That matters because `provisioning_test`'s worker test spawns the real,
//! global `provisioning::run_worker`, and `run_worker` calls the *unscoped*
//! `fetch_pending`: for as long as it runs it picks up and genuinely
//! provisions **any** `pending` outbox row in the shared table, including one
//! a sibling test seeded and is about to assert is still `pending`.
//!
//! A `std::sync::Mutex` would only serialize tests within a single binary. A
//! Postgres advisory lock serializes across binaries and processes too, since
//! the shared database is the thing they actually contend over.
//!
//! Transaction-scoped (`pg_advisory_xact_lock`) rather than session-scoped
//! (`pg_advisory_lock`) on purpose: a panicking test drops the guard, the
//! transaction rolls back, and the lock is released. A session-level lock
//! would ride the connection back into the pool still held and wedge every
//! test that came after it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sqlx::{PgPool, Postgres, Transaction};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// Arbitrary fixed key ("HIVEMIND" in ASCII), shared by every test binary in
/// this crate — it only works as mutual exclusion if all of them use the same
/// number, which is the reason this lives in one module instead of being
/// copy-pasted per file like the older `test_pool()` helpers.
const OUTBOX_TEST_LOCK_KEY: i64 = 0x4849_5645_4d49_4e44;

/// Acquires the exclusive outbox lock, blocking until it's free.
///
/// Bind the returned guard (`let _lock = ...`, never `let _ = ...`, which
/// would drop it immediately) for as long as the test needs exclusivity.
///
/// Every test that seeds a `pending` outbox row and then asserts on that row's
/// state must hold this, and so must the worker test that would otherwise
/// sweep those rows up.
pub async fn lock_outbox(pool: &PgPool) -> Transaction<'static, Postgres> {
    let mut guard = pool.begin().await.expect("begin outbox lock transaction");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(OUTBOX_TEST_LOCK_KEY)
        .execute(&mut *guard)
        .await
        .expect("acquire outbox advisory lock");
    guard
}

/// Removes a test's own outbox rows once it's done asserting on them.
///
/// Rows left `pending` in the persistent dev Postgres are picked up by the
/// *next* run's worker test, which then creates real sqld namespaces for
/// owners that test run never had — so each test cleans up after itself
/// rather than letting that backlog grow. (`scripts/reset-dev-db.sh` now
/// truncates this table too, for the backlog that already exists or that a
/// panicking test leaves behind.)
pub async fn delete_outbox_row(pool: &PgPool, id: uuid::Uuid) {
    sqlx::query("DELETE FROM namespace_provisioning_outbox WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("delete outbox row");
}

/// Captures every `tracing` event with `target: "usage"` emitted while a
/// closure runs, as a list of field-name → stringified-value maps — used to
/// assert on `proxy_handler`'s usage-event fields (`src/proxy.rs`) directly,
/// rather than parsing formatted log output.
#[derive(Clone, Default)]
struct UsageEventCapture {
    events: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl UsageEventCapture {
    fn events(&self) -> Vec<HashMap<String, String>> {
        self.events.lock().unwrap().clone()
    }
}

struct FieldVisitor(HashMap<String, String>);

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S> Layer<S> for UsageEventCapture
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "usage" {
            return;
        }
        let mut visitor = FieldVisitor(HashMap::new());
        event.record(&mut visitor);
        self.events.lock().unwrap().push(visitor.0);
    }
}

/// Runs `f` with a tracing subscriber that captures `target: "usage"`
/// events, returning both `f`'s result and whatever events fired while it
/// ran. Uses `tracing::subscriber::set_default` and holds the returned
/// guard across the `.await` — this only works because `#[tokio::test]`
/// defaults to a single-threaded runtime (every test file in this crate
/// uses the bare attribute, no `flavor = "multi_thread"`), so the task
/// never moves to a different OS thread mid-poll and the thread-local
/// dispatcher stays in effect for the whole call, including inside any
/// `tokio::spawn`ed task also polled on that same thread.
pub async fn capture_usage_events<F, Fut, T>(f: F) -> (T, Vec<HashMap<String, String>>)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let capture = UsageEventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let result = f().await;
    (result, capture.events())
}
