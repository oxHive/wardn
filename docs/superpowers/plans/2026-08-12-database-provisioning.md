# Database Provisioning Automation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the manual "curl sqld's admin API, then hand-insert `database_mappings` via psql" workflow with `POST /users` (register — mints the first API key) and `POST /orgs` (authenticated), each provisioning its own sqld namespace automatically via an outbox pattern.

**Architecture:** Account/org creation and an outbox row are inserted in one Postgres transaction (atomic). The sqld admin-API call happens after that commits — attempted inline first for the common case, retried by an in-process `tokio::time::interval` background worker if the inline attempt fails. `database_mappings` only gets a row once provisioning actually succeeds, so the existing "no mapping → 404" behavior already means "not provisioned yet," with no new error path anywhere downstream.

**Tech Stack:** axum 0.8, sqlx 0.8 (Postgres, runtime-checked queries), reqwest (promoted from dev- to a real dependency, for the sqld admin API's simple JSON POST/DELETE calls) — same as the walking skeleton and org roles.

## Global Constraints

- Rust edition 2024, axum 0.8, sqlx 0.8 Postgres. **Runtime-checked queries only** (`sqlx::query_as::<_, T>(...)`) — never the compile-time `query!`/`query_as!` macros.
- **Users and orgs only.** No workspace provisioning — hivemind core's `Layer::Workspace` has no `[workspace_sync]` client config, so gateway's `workspaces` table has nothing real to provision a namespace for.
- **Namespace name = the owner's own UUID, verbatim** (`Uuid::to_string()`), for both users and orgs.
- **`POST /orgs` mints no second API key.** The creator's existing personal key, plus `X-Org-Id: <org_id>`, already reaches the new org's namespace once this makes them a member with a bootstrap `owner` role holding every permission in the org-roles catalog.
- **Outbox pattern, ordering matters.** The account/org row and its `namespace_provisioning_outbox` row are inserted together, in one Postgres transaction. The sqld admin-API call only ever happens *after* that transaction commits — never before. This means a failure can only ever leave behind an unprovisioned account (harmless, retryable), never an orphaned sqld namespace with nothing in Postgres pointing at it.
- **Inline attempt first, in-process background worker as the retry/durability path.** No external cron, no separate worker binary — a `tokio::time::interval` loop spawned alongside `axum::serve` in `main.rs`.
- **Outbox table name is `namespace_provisioning_outbox`**, not `outbox` or `provisioning_outbox` — specific on purpose, so a future unrelated use of the outbox pattern gets its own table.
- **No password/session infrastructure of any kind.** API keys remain the only auth primitive in this system.
- **All test seed values (UUIDs, emails, namespace names, role names) must derive from a fresh `Uuid::new_v4()` per test run** — hardcoded literals collide with `UNIQUE` constraints on repeat runs against the persistent dev Postgres container.
- Migration numbering continues from `0003_org_roles.sql`: this plan's migration is `migrations/0004_namespace_provisioning_outbox.sql`.

---

### Task 1: Schema, config, and `AppState` wiring

**Files:**
- Create: `migrations/0004_namespace_provisioning_outbox.sql`
- Modify: `src/config.rs`
- Modify: `src/auth.rs`
- Modify: `src/main.rs`
- Modify: `podman-compose.yml`
- Modify: `.env.example`

**Interfaces:**
- Consumes: nothing from earlier tasks (this is the foundation).
- Produces (for Tasks 2-5):
  - `namespace_provisioning_outbox` table (columns below).
  - `Config.sqld_admin_url: String` (required env var `SQLD_ADMIN_URL`).
  - `AppState.sqld_admin_url: String` field, defaulting to `String::new()` via `AppState::new`.
  - `AppState::with_sqld_admin_url(self, sqld_admin_url: String) -> Self` builder method.

- [ ] **Step 1: Write the migration**

Create `migrations/0004_namespace_provisioning_outbox.sql`:

```sql
CREATE TABLE namespace_provisioning_outbox (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_type     TEXT NOT NULL CHECK (owner_type IN ('user', 'org')),
    owner_id       UUID NOT NULL,
    sqld_namespace TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('pending', 'done', 'failed')) DEFAULT 'pending',
    attempts       INT NOT NULL DEFAULT 0,
    last_error     TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

- [ ] **Step 2: Add `sqld_admin_url` to `Config`**

Replace the full contents of `src/config.rs` with:

```rust
use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub sqld_url: String,
    pub sqld_admin_url: String,
    pub listen_addr: String,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Ok(Config {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            sqld_url: std::env::var("SQLD_URL").context("SQLD_URL must be set")?,
            sqld_admin_url: std::env::var("SQLD_ADMIN_URL")
                .context("SQLD_ADMIN_URL must be set")?,
            listen_addr: std::env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8787".to_string()),
        })
    }
}
```

(No test file for this — `Config::from_env` is a 4-line env-var mapping with no test call sites anywhere in the existing suite; its correctness is proven by the binary failing to start without `SQLD_ADMIN_URL` set, which Step 6 exercises for real.)

- [ ] **Step 3: Add `sqld_admin_url` to `AppState`**

In `src/auth.rs`, replace the `AppState` struct and its `impl` block with:

```rust
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Base URL of the sqld instance to proxy to, e.g. `http://127.0.0.1:8081`.
    /// Held here rather than read from the environment inside the handler so
    /// that it is configured exactly once, in one place, and tests can point
    /// it wherever they like.
    pub sqld_url: String,
    /// Base URL of sqld's admin API, e.g. `http://127.0.0.1:8090` — used by
    /// database provisioning (`src/provisioning.rs`) to create a namespace.
    /// Defaults to empty via `new`; set it with `with_sqld_admin_url` where
    /// provisioning is actually exercised (`main.rs`, provisioning tests).
    /// Every other existing test/call site is unaffected by its absence.
    pub sqld_admin_url: String,
    /// Pooled outbound HTTP clients, shared by every request.
    pub client: ProxyClient,
}

impl AppState {
    pub fn new(pool: PgPool, sqld_url: String) -> Self {
        Self {
            pool,
            sqld_url,
            sqld_admin_url: String::new(),
            client: ProxyClient::new(),
        }
    }

    pub fn with_sqld_admin_url(mut self, sqld_admin_url: String) -> Self {
        self.sqld_admin_url = sqld_admin_url;
        self
    }
}
```

Leave every other part of `src/auth.rs` (imports, `KEY_MARKER`, `generate_api_key`, `AuthedOwner`, `auth_middleware`, etc.) exactly as it is.

- [ ] **Step 4: Wire `sqld_admin_url` into `main.rs`**

Replace the full contents of `src/main.rs` with:

```rust
use wardn::{AppState, app, config::Config, db};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    let state = AppState::new(pool, config.sqld_url.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("wardn listening on {}", config.listen_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
```

(Task 5 replaces this file again to add the background worker — this is an intermediate state.)

- [ ] **Step 5: Wire `SQLD_ADMIN_URL` into `podman-compose.yml` and `.env.example`**

In `podman-compose.yml`, add `SQLD_ADMIN_URL` to the `gateway` service's `environment` block (alongside the existing `DATABASE_URL`/`SQLD_URL`/`LISTEN_ADDR`):

```yaml
    environment:
      # Container-to-container addresses (compose's internal DNS), not the
      # host-published loopback ports above — postgres/sqld are reachable
      # from the gateway container by service name on their internal ports.
      DATABASE_URL: postgres://gateway:gateway@postgres:5432/gateway
      SQLD_URL: http://sqld:8080
      SQLD_ADMIN_URL: http://sqld:8090
      LISTEN_ADDR: 0.0.0.0:8787
```

In `.env.example`, update the `SQLD_ADMIN_URL` line's comment (it's no longer test-only):

```
# Used by database provisioning (creating a namespace on POST /users and
# POST /orgs) as well as tests that exercise sqld's admin API directly.
SQLD_ADMIN_URL=http://127.0.0.1:8090
```

- [ ] **Step 6: Verify the build and the existing suite**

Run: `cargo build`
Expected: succeeds, no errors.

Run: `cargo test`
Expected: the full existing suite still passes bare (no `SQLD_ADMIN_URL` env var needs to be set for any *existing* test — they don't call `Config::from_env`).

Run (from the repo root, with the dev Postgres/sqld up via `podman-compose up -d`):
```sh
SQLD_ADMIN_URL=http://127.0.0.1:8090 cargo run
```
Expected: starts and logs `wardn listening on 127.0.0.1:8787`. Ctrl-C to stop, then confirm it refuses to start at all without `SQLD_ADMIN_URL` set:
```sh
unset SQLD_ADMIN_URL; cargo run
```
Expected: exits immediately with `SQLD_ADMIN_URL must be set`.

- [ ] **Step 7: Commit**

```bash
git add migrations/0004_namespace_provisioning_outbox.sql src/config.rs src/auth.rs src/main.rs podman-compose.yml .env.example
git commit -m "feat: add namespace provisioning outbox schema and config"
```

---

### Task 2: Core provisioning — `attempt_provisioning` and `fetch_pending`

**Files:**
- Create: `src/provisioning.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Test: `tests/provisioning_test.rs` (new)

**Interfaces:**
- Consumes: `namespace_provisioning_outbox` table from Task 1.
- Produces (for Tasks 3, 4, 5):
  - `pub struct OutboxRow { pub id: Uuid, pub owner_type: String, pub owner_id: Uuid, pub sqld_namespace: String, pub attempts: i32 }` — `Debug + Clone`, `sqlx::FromRow`.
  - `pub async fn attempt_provisioning(pool: &PgPool, sqld_admin_url: &str, row: &OutboxRow) -> Result<bool, sqlx::Error>`
  - `pub async fn fetch_pending(pool: &PgPool, limit: i64) -> Result<Vec<OutboxRow>, sqlx::Error>`

- [ ] **Step 1: Promote `reqwest` to a real dependency**

In `Cargo.toml`, move the `reqwest` line from `[dev-dependencies]` to `[dependencies]`, and update the comment above the remaining dev-dependencies:

```toml
[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tower = { version = "0.5", features = ["util"] }
sqlx = { version = "0.8", features = ["postgres", "runtime-tokio", "migrate", "uuid", "chrono"] }
chrono = "0.4"
argon2 = "0.5"
rand = "0.8"
base64 = "0.22"
hyper = { version = "1", features = ["client", "http1", "http2"] }
hyper-util = { version = "0.1", features = ["client", "client-legacy", "http1", "http2", "tokio"] }
http-body-util = "0.1"
uuid = { version = "1", features = ["v4", "serde"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
serde = { version = "1", features = ["derive"] }
# Simple JSON client for sqld's admin API (database provisioning,
# src/provisioning.rs) — the data-plane proxy itself still uses
# hyper/hyper-util directly (see src/proxy.rs) for streaming and
# protocol-accurate HTTP/1.1-vs-h2c forwarding, which reqwest doesn't give
# fine-grained enough control over.
reqwest = { version = "0.12", features = ["stream"] }

[dev-dependencies]
bytes = "1"
serde_json = "1"
```

- [ ] **Step 2: Write the failing tests**

Create `tests/provisioning_test.rs`:

```rust
use wardn::db;
use wardn::provisioning::{self, OutboxRow};
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
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --test provisioning_test`
Expected: compile failure — `wardn::provisioning` doesn't exist yet.

- [ ] **Step 4: Implement `src/provisioning.rs`**

```rust
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
```

- [ ] **Step 5: Register the module**

In `src/lib.rs`, add `pub mod provisioning;` to the module list (keep alphabetical order: `auth`, `config`, `db`, `org_admin`, `provisioning`, `proxy`, `roles`, `routing`). The full file becomes:

```rust
pub mod auth;
pub mod config;
pub mod db;
pub mod org_admin;
pub mod provisioning;
pub mod proxy;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route(
            "/orgs/{org_id}/roles",
            post(org_admin::create_role).get(org_admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org_admin::update_role).delete(org_admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org_admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .with_state(state)
}
```

(No router changes in this task — just the module declaration.)

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test provisioning_test`
Expected: all 4 tests PASS.

Run: `cargo test`
Expected: full suite still passes, no regressions.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/provisioning.rs src/lib.rs tests/provisioning_test.rs
git commit -m "feat: add core namespace provisioning (attempt_provisioning, fetch_pending)"
```

---

### Task 3: `POST /users` — registration

**Files:**
- Create: `src/registration.rs`
- Modify: `src/lib.rs`
- Test: `tests/registration_test.rs` (new)

**Interfaces:**
- Consumes: `provisioning::{attempt_provisioning, OutboxRow}` from Task 2; `auth::generate_api_key` (existing).
- Produces (for Task 4, which appends to the same file):
  - `pub struct CreateUserRequest { pub email: String }`
  - `pub struct CreateUserResponse { pub user_id: Uuid, pub api_key: String }`
  - `pub async fn create_user(State(state): State<AppState>, Json(req): Json<CreateUserRequest>) -> Response`

- [ ] **Step 1: Write the failing tests**

Create `tests/registration_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use wardn::auth::AppState;
use wardn::db;
use tower::ServiceExt;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

fn test_sqld_url() -> String {
    std::env::var("SQLD_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string())
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

#[tokio::test]
async fn create_user_returns_a_working_key_and_provisions_a_namespace() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let email = format!("register-{}@example.com", uuid::Uuid::new_v4());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id = created["user_id"].as_str().unwrap().to_string();
    let api_key = created["api_key"].as_str().unwrap();
    assert!(api_key.starts_with(wardn::auth::KEY_MARKER));

    // Prove the namespace was actually provisioned: write through it, read
    // it back, via the real router, exactly the way a real client would.
    let secret = format!("REG-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "write through the new namespace failed"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT v FROM kv"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains(&secret),
        "read-back did not contain the written secret: {text}"
    );

    delete_namespace(&user_id).await;
}

#[tokio::test]
async fn create_user_still_succeeds_when_inline_provisioning_fails() {
    let pool = test_pool().await;
    // Unreachable admin URL — the inline attempt inside create_user must
    // fail without failing the request itself.
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url("http://127.0.0.1:1".to_string());
    let app = wardn::app(state);

    let email = format!("register-fail-{}@example.com", uuid::Uuid::new_v4());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id: uuid::Uuid = created["user_id"].as_str().unwrap().parse().unwrap();

    let (status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempts FROM namespace_provisioning_outbox
         WHERE owner_type = 'user' AND owner_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(attempts, 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test registration_test`
Expected: compile failure — `wardn::registration` and `POST /users` don't exist yet.

- [ ] **Step 3: Implement `src/registration.rs`**

```rust
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{self, AppState};
use crate::provisioning::{self, OutboxRow};

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
}

#[derive(Serialize)]
pub struct CreateUserResponse {
    pub user_id: Uuid,
    pub api_key: String,
}

/// Inserts the new user, a fresh personal API key, and its provisioning
/// outbox row in one transaction. Returns the outbox row directly so the
/// caller can make the inline provisioning attempt without a second round
/// trip to read back what was just written.
async fn insert_user(
    pool: &PgPool,
    email: &str,
) -> Result<(Uuid, String, OutboxRow), sqlx::Error> {
    let user_id = Uuid::new_v4();
    let outbox_id = Uuid::new_v4();
    let namespace = user_id.to_string();
    let (full_key, prefix, hash) = auth::generate_api_key();

    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(email)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO namespace_provisioning_outbox (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'user', $2, $3)",
    )
    .bind(outbox_id)
    .bind(user_id)
    .bind(&namespace)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        user_id,
        full_key,
        OutboxRow {
            id: outbox_id,
            owner_type: "user".to_string(),
            owner_id: user_id,
            sqld_namespace: namespace,
            attempts: 0,
        },
    ))
}

/// `POST /users` — public, no `Authorization` header required: this is how a
/// caller gets their first API key at all. Registered after
/// `auth_middleware` in `app()` (`src/lib.rs`), the same place `/healthz`
/// lives, so it bypasses the auth layer entirely.
///
/// The account and key are committed before any sqld call is made, so a
/// failed or slow inline provisioning attempt never costs the caller their
/// account — see
/// `docs/superpowers/specs/2026-08-12-database-provisioning-design.md`.
pub async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Response {
    match insert_user(&state.pool, &req.email).await {
        Ok((user_id, api_key, outbox_row)) => {
            if let Err(e) = provisioning::attempt_provisioning(
                &state.pool,
                &state.sqld_admin_url,
                &outbox_row,
            )
            .await
            {
                tracing::error!("inline provisioning attempt failed: {e:#}");
            }
            (
                StatusCode::CREATED,
                Json(CreateUserResponse { user_id, api_key }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("create user failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 4: Wire the route into `src/lib.rs`**

Replace the full contents of `src/lib.rs` with:

```rust
pub mod auth;
pub mod config;
pub mod db;
pub mod org_admin;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route(
            "/orgs/{org_id}/roles",
            post(org_admin::create_role).get(org_admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org_admin::update_role).delete(org_admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org_admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .route("/users", post(registration::create_user))
        .with_state(state)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test registration_test`
Expected: both tests PASS.

Run: `cargo test`
Expected: full suite still passes.

- [ ] **Step 6: Commit**

```bash
git add src/registration.rs src/lib.rs tests/registration_test.rs
git commit -m "feat: add POST /users registration endpoint"
```

---

### Task 4: `POST /orgs` — authenticated org creation

**Files:**
- Modify: `src/registration.rs` (append)
- Modify: `src/roles.rs`
- Modify: `src/lib.rs`
- Modify: `tests/registration_test.rs` (append)

**Interfaces:**
- Consumes: `roles::{insert_role_permissions, ALL_PERMISSIONS}` (visibility widened this task), `provisioning::{attempt_provisioning, OutboxRow}`, `auth::AuthedOwner`.
- Produces: `pub struct CreateOrgRequest { pub name: String }`, `pub struct CreateOrgResponse { pub org_id: Uuid }`, `pub async fn create_org(...) -> Response` — no later tasks consume these directly.

- [ ] **Step 1: Widen `insert_role_permissions`'s visibility**

In `src/roles.rs`, change:

```rust
async fn insert_role_permissions(
```

to:

```rust
pub(crate) async fn insert_role_permissions(
```

This is the only change to `src/roles.rs` — the function body and its doc comment are unchanged. `pub(crate)` (not `pub`) because it's only ever called from within this crate (`create_role` in this file, and `create_org` in `src/registration.rs`), never from a test crate or external consumer.

- [ ] **Step 2: Write the failing tests**

Append to `tests/registration_test.rs` (`wardn::db` is already imported at the top of this file from Task 3 — no new import needed here):

```rust
async fn seed_registered_user(app: axum::Router) -> (uuid::Uuid, String) {
    let email = format!("orgcreator-{}@example.com", uuid::Uuid::new_v4());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id: uuid::Uuid = created["user_id"].as_str().unwrap().parse().unwrap();
    let api_key = created["api_key"].as_str().unwrap().to_string();
    (user_id, api_key)
}

#[tokio::test]
async fn create_org_provisions_a_namespace_and_makes_the_creator_its_owner() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_user_id, api_key) = seed_registered_user(app.clone()).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Acme"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let org_id = created["org_id"].as_str().unwrap().to_string();

    // The creator's own key, plus X-Org-Id, must already reach the new
    // org's namespace — proving the bootstrap role + membership landed.
    let secret = format!("ORG-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "write into the new org's namespace failed"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
                .body(Body::from(r#"{"statements":["SELECT v FROM kv"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains(&secret),
        "read-back did not contain the written secret: {text}"
    );

    delete_namespace(&org_id).await;
}

#[tokio::test]
async fn create_org_is_forbidden_for_a_non_user_owned_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    // A workspace-owned key: real row, valid hash, but owner_type != "user".
    let workspace_id = uuid::Uuid::new_v4();
    let user_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("wsowner-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces (id, owner_user_id, name) VALUES ($1, $2, 'ws')")
        .bind(workspace_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = wardn::auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'workspace', $3, $4, $5)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_id)
    .bind(workspace_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"Nope"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --test registration_test`
Expected: compile failure — `POST /orgs` doesn't exist yet.

- [ ] **Step 4: Implement `create_org` in `src/registration.rs`**

Replace the import block at the top of `src/registration.rs`:

```rust
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{self, AppState};
use crate::provisioning::{self, OutboxRow};
```

with:

```rust
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{self, AppState, AuthedOwner};
use crate::provisioning::{self, OutboxRow};
use crate::roles;
```

Then append to the end of `src/registration.rs`:

```rust
#[derive(Deserialize)]
pub struct CreateOrgRequest {
    pub name: String,
}

#[derive(Serialize)]
pub struct CreateOrgResponse {
    pub org_id: Uuid,
}

/// Inserts the new org, a bootstrap `owner` role holding every permission in
/// the catalog, the creator's membership in that role, and the org's
/// provisioning outbox row — all in one transaction. Reuses
/// `roles::insert_role_permissions` for the permission-attach loop rather
/// than duplicating it, but can't reuse `roles::create_role` itself since
/// that function opens its own transaction and this one needs everything
/// atomic with the org insert.
async fn insert_org(
    pool: &PgPool,
    name: &str,
    creator_user_id: Uuid,
) -> Result<(Uuid, OutboxRow), sqlx::Error> {
    let org_id = Uuid::new_v4();
    let role_id = Uuid::new_v4();
    let outbox_id = Uuid::new_v4();
    let namespace = org_id.to_string();

    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .bind(org_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO roles (id, org_id, name) VALUES ($1, $2, 'owner')")
        .bind(role_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    roles::insert_role_permissions(&mut tx, role_id, &roles::ALL_PERMISSIONS).await?;
    sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(creator_user_id)
        .bind(role_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO namespace_provisioning_outbox (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'org', $2, $3)",
    )
    .bind(outbox_id)
    .bind(org_id)
    .bind(&namespace)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok((
        org_id,
        OutboxRow {
            id: outbox_id,
            owner_type: "org".to_string(),
            owner_id: org_id,
            sqld_namespace: namespace,
            attempts: 0,
        },
    ))
}

/// `POST /orgs` — authenticated (goes through `auth_middleware` normally,
/// reads the caller via the `AuthedOwner` extension). Only a personal
/// (`owner_type == "user"`) key may create an org — a workspace/org-owned
/// key's `owner_id` is not a user id and could never legitimately become an
/// `org_members.user_id`, the same reasoning `roles::require_permission`
/// already applies to the org-shared-namespace proxy path.
///
/// Mints no second API key: the caller's existing personal key, plus
/// `X-Org-Id: <org_id>`, already reaches the new org's namespace once this
/// makes them its `owner`.
pub async fn create_org(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Json(req): Json<CreateOrgRequest>,
) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    match insert_org(&state.pool, &req.name, owner.owner_id).await {
        Ok((org_id, outbox_row)) => {
            if let Err(e) = provisioning::attempt_provisioning(
                &state.pool,
                &state.sqld_admin_url,
                &outbox_row,
            )
            .await
            {
                tracing::error!("inline provisioning attempt failed: {e:#}");
            }
            (StatusCode::CREATED, Json(CreateOrgResponse { org_id })).into_response()
        }
        Err(e) => {
            tracing::error!("create org failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 5: Wire the route into `src/lib.rs`**

Replace the full contents of `src/lib.rs` with:

```rust
pub mod auth;
pub mod config;
pub mod db;
pub mod org_admin;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/orgs", post(registration::create_org))
        .route(
            "/orgs/{org_id}/roles",
            post(org_admin::create_role).get(org_admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org_admin::update_role).delete(org_admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org_admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .route("/users", post(registration::create_user))
        .with_state(state)
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test registration_test`
Expected: all 4 tests PASS.

Run: `cargo test`
Expected: full suite still passes.

- [ ] **Step 7: Commit**

```bash
git add src/registration.rs src/roles.rs src/lib.rs tests/registration_test.rs
git commit -m "feat: add POST /orgs org creation endpoint"
```

---

### Task 5: Background provisioning worker

**Files:**
- Modify: `src/provisioning.rs`
- Modify: `src/main.rs`
- Modify: `tests/provisioning_test.rs` (append)

**Interfaces:**
- Consumes: `attempt_provisioning`, `fetch_pending` from Task 2.
- Produces: `pub async fn run_worker(pool: PgPool, sqld_admin_url: String, interval: Duration)` — no later tasks consume this; it's wired directly into `main.rs`.

- [ ] **Step 1: Write the failing test**

Append to `tests/provisioning_test.rs`:

```rust
use std::time::Duration;

#[tokio::test]
async fn run_worker_eventually_provisions_a_row_the_inline_attempt_missed() {
    let pool = test_pool().await;
    let namespace = format!("workerrecover-{}", Uuid::new_v4());
    // Seed as if an inline attempt already failed once: pending, attempts=1.
    let row = seed_outbox_row(&pool, "user", &namespace).await;
    sqlx::query("UPDATE namespace_provisioning_outbox SET attempts = 1 WHERE id = $1")
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();

    tokio::spawn(provisioning::run_worker(
        pool.clone(),
        admin_url(),
        Duration::from_millis(200),
    ));

    tokio::time::sleep(Duration::from_millis(800)).await;

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
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test provisioning_test run_worker_eventually_provisions`
Expected: compile failure — `provisioning::run_worker` doesn't exist yet.

- [ ] **Step 3: Implement `run_worker`**

In `src/provisioning.rs`, replace the top of the file:

```rust
use sqlx::PgPool;
use uuid::Uuid;
```

with:

```rust
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;
```

Then append to the end of `src/provisioning.rs`:

```rust
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
```

- [ ] **Step 4: Wire the worker into `main.rs`**

Replace the full contents of `src/main.rs` with:

```rust
use std::time::Duration;

use wardn::{AppState, app, config::Config, db, provisioning};

/// How often the background provisioning worker retries pending outbox
/// rows. See `docs/superpowers/specs/2026-08-12-database-provisioning-design.md`.
const PROVISIONING_WORKER_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;

    let worker_pool = pool.clone();
    let worker_admin_url = config.sqld_admin_url.clone();
    tokio::spawn(provisioning::run_worker(
        worker_pool,
        worker_admin_url,
        PROVISIONING_WORKER_INTERVAL,
    ));

    let state = AppState::new(pool, config.sqld_url.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone());
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("wardn listening on {}", config.listen_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test provisioning_test`
Expected: all 5 tests PASS (the 4 from Task 2 plus this one).

Run: `cargo test`
Expected: full suite still passes.

Run: `cargo build`
Expected: succeeds, no warnings (in particular, no unused-import warning on `Duration` in either file).

- [ ] **Step 6: Commit**

```bash
git add src/provisioning.rs src/main.rs tests/provisioning_test.rs
git commit -m "feat: add background provisioning worker"
```

---

## Self-Review Notes

- **Spec coverage:** namespace provisioning outbox schema (Task 1) — `Config`/`AppState`/compose/env wiring (Task 1) — `attempt_provisioning`/`fetch_pending` core (Task 2) — `reqwest` promotion (Task 2) — `POST /users` public registration (Task 3) — `POST /orgs` authenticated, no second key, bootstrap `owner` role (Task 4) — `owner_type == "user"` guard on org creation (Task 4) — in-process `tokio::time::interval` background worker (Task 5) — inline-attempt-then-still-succeeds behavior (Task 3's second test) — worker recovers what the inline attempt missed (Task 5's test) — write-then-read-back proofs for both `POST /users` and `POST /orgs`, not status-only (Tasks 3 and 4). No gaps found.
- **Placeholder scan:** none found — every step has complete, runnable code.
- **Type consistency:** `OutboxRow`, `attempt_provisioning`, `fetch_pending`, `run_worker`, `CreateUserRequest`/`CreateUserResponse`, `CreateOrgRequest`/`CreateOrgResponse`, `insert_user`, `insert_org` are each defined exactly once and referenced with matching signatures in every later task and test. `AppState::with_sqld_admin_url` (Task 1) is used identically across Tasks 2-5's tests.
