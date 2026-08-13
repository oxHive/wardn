# hivewarden Walking Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A gateway that authenticates an API key, resolves which sqld namespace it's scoped to, and transparently reverse-proxies the request there — nothing else.

**Architecture:** axum service with three layers: an auth extractor (`AuthedOwner`) that validates a bearer API key against an argon2 hash in Postgres and resolves an `(owner_type, owner_id)` pair; a routing function that looks up that owner's sqld namespace in `database_mappings`; a catch-all proxy handler that forwards the request verbatim (method, headers, body) to the shared sqld instance with a namespace header set, and streams the response back untouched.

**Tech Stack:** Rust/axum 0.8 (HTTP), sqlx 0.8/Postgres (control plane, runtime-checked queries — no compile-time query macros, so `cargo build` never needs a live database), argon2 (key hashing), reqwest (outbound proxy leg), podman (local Postgres + sqld for dev/test).

## Global Constraints

- The gateway never parses libsql/Hrana payloads or memory content — it authenticates, resolves a namespace, and forwards bytes. If any task's code needs to understand what's inside the proxied body, that's a design defect, not a detail to work around.
- No admin/creation endpoints in this plan. All test/dev data (`users`, `api_keys`, `orgs`, `database_mappings` rows) goes in via SQL migration seed files or direct `psql`/`sqlx` inserts in tests — never via an HTTP endpoint.
- Org role enforcement, rate limiting, billing, usage metering, and provisioning automation are explicitly out of scope — `org_members.role` exists in the schema but nothing reads it yet.
- `sqlx` queries in this plan use the runtime-checked API (`sqlx::query`/`query_as` with `.bind(...)`), not the `query!`/`query_as!` compile-time macros — this plan's tests are the only thing that needs a live Postgres, never `cargo build`.
- Integration tests require real local services (Postgres + sqld), started via `podman-compose` per Task 1 — no mocking the proxy path.
- **The exact sqld namespace header name (`x-libsql-namespace`, used throughout this plan) is a best-guess from libsql-server's documented convention, not independently verified against a running instance on this machine (no sqld binary or network access was available while writing this plan).** Task 1's dev-services setup is the first point where a real sqld instance exists — verify the header name there (check the container's startup logs, `--help` output, or the libsql-server source under `/usr/local/bin` or wherever the image places it) before Task 5 depends on it. If it's wrong, every later task's tests still work using the wrong-but-consistent header name until Task 5, where the end-to-end proxy test will fail against real sqld and reveal the correct name to swap in.

---

### Task 1: Project scaffold, config, dev services, health check

**Files:**
- Modify: `Cargo.toml`
- Create: `src/main.rs` (replace the existing `Hello, world!` scaffold)
- Create: `src/config.rs`
- Create: `podman-compose.yml`
- Create: `tests/health_test.rs`

**Interfaces:**
- Produces: `config::Config { database_url: String, sqld_url: String, listen_addr: String }` with `Config::from_env() -> Result<Config, anyhow::Error>`, reading `DATABASE_URL`, `SQLD_URL`, `LISTEN_ADDR` (default `127.0.0.1:8787` if unset).
- Produces: `fn app() -> axum::Router` in `main.rs` (no state yet — just the health route), consumed directly by Task 1's own test and extended with real state in Task 5.

- [ ] **Step 1: Write the failing test**

Create `tests/health_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn healthz_returns_ok() {
    let app = hivewarden::app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test health_test 2>&1 | tail -30`
Expected: FAIL to compile — `hivewarden::app` doesn't exist yet, and this is a binary-only crate today with no library target for the test to link against.

- [ ] **Step 3: Add dependencies**

In `Cargo.toml`, replace the `[dependencies]` section:

```toml
[package]
name = "hivewarden"
version = "0.1.0"
edition = "2024"

[lib]
name = "hivewarden"
path = "src/lib.rs"

[[bin]]
name = "hivewarden"
path = "src/main.rs"

[dependencies]
axum = "0.8"
tokio = { version = "1", features = ["full"] }
tower = { version = "0.5", features = ["util"] }
sqlx = { version = "0.8", features = ["postgres", "runtime-tokio", "migrate", "uuid", "chrono"] }
argon2 = "0.5"
rand = "0.8"
reqwest = { version = "0.12", features = ["stream"] }
uuid = { version = "1", features = ["v4", "serde"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
```

(Splitting into a `[lib]` + `[[bin]]` is what lets `tests/*.rs` integration tests import `hivewarden::app()` — this is the standard axum project shape.)

- [ ] **Step 4: Write `config.rs`**

Create `src/config.rs`:

```rust
use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub sqld_url: String,
    pub listen_addr: String,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Ok(Config {
            database_url: std::env::var("DATABASE_URL")
                .context("DATABASE_URL must be set")?,
            sqld_url: std::env::var("SQLD_URL").context("SQLD_URL must be set")?,
            listen_addr: std::env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8787".to_string()),
        })
    }
}
```

- [ ] **Step 5: Write `src/lib.rs` and `src/main.rs`**

Create `src/lib.rs`:

```rust
pub mod config;

use axum::{Router, routing::get};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app() -> Router {
    Router::new().route("/healthz", get(healthz))
}
```

Replace `src/main.rs`:

```rust
use hivewarden::{app, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivewarden listening on {}", config.listen_addr);
    axum::serve(listener, app()).await?;
    Ok(())
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test --test health_test 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 7: Add dev services (Postgres + sqld) for later tasks' integration tests**

Create `podman-compose.yml`:

```yaml
version: "3"
services:
  postgres:
    image: postgres:16
    environment:
      POSTGRES_USER: gateway
      POSTGRES_PASSWORD: gateway
      POSTGRES_DB: gateway
    ports:
      - "5433:5432"
  sqld:
    image: ghcr.io/tursodatabase/libsql-server:latest
    command: ["sqld", "--http-listen-addr=0.0.0.0:8080", "--enable-namespaces"]
    ports:
      - "8081:8080"
```

Run: `podman-compose -f podman-compose.yml up -d`
Expected: both containers start. Confirm Postgres is reachable: `PGPASSWORD=gateway psql -h 127.0.0.1 -p 5433 -U gateway -d gateway -c 'select 1;'` (if `psql` isn't installed locally, `podman exec` into the postgres container and run `psql` there instead: `podman exec -it <container> psql -U gateway -d gateway -c 'select 1;'`).

**Verify the namespace header name here** (per this plan's Global Constraints note): run `podman logs <sqld-container-name>` after startup and/or `podman exec <sqld-container-name> sqld --help` to confirm the real namespace-selection header/mechanism `--enable-namespaces` mode exposes. If it differs from `x-libsql-namespace`, note the real name — Task 5 depends on it.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/main.rs src/config.rs podman-compose.yml tests/health_test.rs
git commit -m "feat: project scaffold, config, health check, dev services"
```

---

### Task 2: Postgres schema + control-plane accessors

**Files:**
- Create: `migrations/0001_initial_schema.sql`
- Create: `src/db.rs`
- Modify: `src/lib.rs` (add `pub mod db;`)
- Test: `tests/db_test.rs`

**Interfaces:**
- Consumes: `config::Config.database_url` (Task 1).
- Produces: `db::connect(database_url: &str) -> Result<sqlx::PgPool, sqlx::Error>` (runs `sqlx::migrate!()` on connect), `db::ApiKeyRow { id: Uuid, owner_type: String, owner_id: Uuid, prefix: String, key_hash: String, revoked_at: Option<chrono::DateTime<chrono::Utc>> }`, `db::find_api_key_by_prefix(pool: &PgPool, prefix: &str) -> Result<Option<ApiKeyRow>, sqlx::Error>`, `db::find_database_mapping(pool: &PgPool, owner_type: &str, owner_id: Uuid) -> Result<Option<String>, sqlx::Error>` (returns the `sqld_namespace` string).

- [ ] **Step 1: Write the failing test**

Create `tests/db_test.rs`:

```rust
use hivewarden::db;
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
    .bind("testpfx")
    .bind("dummy-hash")
    .execute(&pool)
    .await
    .unwrap();

    let row = db::find_api_key_by_prefix(&pool, "testpfx")
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `podman-compose -f podman-compose.yml up -d && cargo test --test db_test 2>&1 | tail -30`
Expected: FAIL to compile — `hivewarden::db` doesn't exist yet.

- [ ] **Step 3: Write the migration**

Create `migrations/0001_initial_schema.sql`:

```sql
CREATE TABLE users (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email      TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE orgs (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE org_members (
    org_id     UUID NOT NULL REFERENCES orgs(id),
    user_id    UUID NOT NULL REFERENCES users(id),
    role       TEXT NOT NULL CHECK (role IN ('admin', 'member', 'read_only')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, user_id)
);

CREATE TABLE workspaces (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_user_id  UUID NOT NULL REFERENCES users(id),
    name           TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE database_mappings (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_type     TEXT NOT NULL CHECK (owner_type IN ('user', 'workspace', 'org')),
    owner_id       UUID NOT NULL,
    sqld_namespace TEXT NOT NULL UNIQUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_type, owner_id)
);

CREATE TABLE api_keys (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id),
    owner_type  TEXT NOT NULL CHECK (owner_type IN ('user', 'workspace', 'org')),
    owner_id    UUID NOT NULL,
    prefix      TEXT NOT NULL UNIQUE,
    key_hash    TEXT NOT NULL,
    revoked_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_api_keys_prefix ON api_keys (prefix) WHERE revoked_at IS NULL;
```

(`user_id` on `api_keys` is the creator/audit trail — `owner_type`/`owner_id` is what the key actually grants access to, which may be the same user, or a workspace/org they belong to. This lets one user hold separate keys for their personal layer and for an org they're a member of, matching how `hivemind`'s client already keeps `[sync]` and `[org_sync]` as separate connections/keys.)

- [ ] **Step 4: Write `db.rs`**

Create `src/db.rs`:

```rust
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
```

In `src/lib.rs`, add `pub mod db;` alongside `pub mod config;`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test db_test 2>&1 | tail -30`
Expected: all 3 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add migrations/0001_initial_schema.sql src/db.rs src/lib.rs tests/db_test.rs Cargo.lock
git commit -m "feat: control-plane schema and Postgres accessors"
```

---

### Task 3: API key generation + auth extractor

**Files:**
- Create: `src/auth.rs`
- Modify: `src/lib.rs` (add `pub mod auth;`)
- Test: `tests/auth_test.rs`

**Interfaces:**
- Consumes: `db::find_api_key_by_prefix` (Task 2).
- Produces: `auth::generate_api_key() -> (String, String, String)` returning `(full_key, prefix, hash)` — `full_key` is what's shown to the user once (`hm_live_<32 random alphanumeric chars>`), `prefix` is its first 12 characters (stored unhashed for lookup), `hash` is the argon2 hash of the full key (stored, never the raw key). Produces `auth::verify_key(full_key: &str, hash: &str) -> bool`. Produces `#[derive(Debug, Clone)] struct AuthedOwner { pub owner_type: String, pub owner_id: uuid::Uuid }`, inserted into request extensions by `auth_middleware` on a successful auth and read downstream via axum's built-in `axum::Extension<AuthedOwner>` extractor — `AuthedOwner` does NOT implement its own `FromRequestParts`; `Extension<T>`'s blanket impl already covers "read a `T` a middleware stashed in request extensions," so a second, custom extraction mechanism for the same value would be redundant. Also produces `AppState` (a new `#[derive(Clone)] struct AppState { pub pool: sqlx::PgPool }` — this task introduces it, Task 4 and Task 5 both extend its usage, not its shape).

- [ ] **Step 1: Write the failing test**

Create `tests/auth_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use axum::{Extension, Router};
use hivewarden::auth::{self, AppState, AuthedOwner};
use hivewarden::db;
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

async fn whoami(Extension(owner): Extension<AuthedOwner>) -> String {
    format!("{}:{}", owner.owner_type, owner.owner_id)
}

fn test_app(pool: sqlx::PgPool) -> Router {
    Router::new()
        .route("/whoami", get(whoami))
        .layer(axum::middleware::from_fn_with_state(
            AppState { pool: pool.clone() },
            hivewarden::auth::auth_middleware,
        ))
        .with_state(AppState { pool })
}

#[tokio::test]
async fn valid_key_resolves_owner() {
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("valid-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        format!("user:{user_id}")
    );
}

#[tokio::test]
async fn missing_key_returns_401() {
    let pool = test_pool().await;
    let app = test_app(pool);
    let resp = app
        .oneshot(Request::builder().uri("/whoami").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_key_returns_401() {
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("wrong-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (_full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {prefix}wrongsuffixwrongsuffix"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revoked_key_returns_401() {
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("revoked-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash, revoked_at)
         VALUES ($1, $2, 'user', $2, $3, $4, now())",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test auth_test 2>&1 | tail -30`
Expected: FAIL to compile — `hivewarden::auth` doesn't exist yet.

- [ ] **Step 3: Write `auth.rs`**

Create `src/auth.rs`:

```rust
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{SaltString, rand_core::OsRng};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::Rng;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

/// Generates a new API key. Returns (full_key, prefix, hash) — the caller
/// shows full_key to the user exactly once and stores only prefix+hash.
pub fn generate_api_key() -> (String, String, String) {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let random_part: String = (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let full_key = format!("hm_live_{random_part}");
    let prefix = full_key.chars().take(12).collect::<String>();
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(full_key.as_bytes(), &salt)
        .expect("argon2 hashing does not fail for well-formed input")
        .to_string();
    (full_key, prefix, hash)
}

pub fn verify_key(full_key: &str, hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(full_key.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Inserted into request extensions by `auth_middleware` on a successful
/// auth. Read downstream via axum's built-in `axum::Extension<AuthedOwner>`
/// extractor — no custom `FromRequestParts` impl needed, `Extension<T>`
/// already does exactly this for any `T: Clone + Send + Sync + 'static`
/// present in the request's extensions.
#[derive(Debug, Clone)]
pub struct AuthedOwner {
    pub owner_type: String,
    pub owner_id: Uuid,
}

/// Middleware: validates the Authorization bearer token against Postgres,
/// and — on success — inserts an AuthedOwner extension for downstream
/// extractors/handlers to read. Rejects with 401 on any failure (missing
/// header, unknown prefix, hash mismatch, or revoked key).
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let Some(full_key) = auth_header else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let prefix: String = full_key.chars().take(12).collect();

    let row = match db::find_api_key_by_prefix(&state.pool, &prefix).await {
        Ok(row) => row,
        Err(e) => {
            tracing::error!("api key lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(row) = row else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if row.revoked_at.is_some() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !verify_key(full_key, &row.key_hash) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    request.extensions_mut().insert(AuthedOwner {
        owner_type: row.owner_type,
        owner_id: row.owner_id,
    });
    next.run(request).await
}
```

In `src/lib.rs`, add `pub mod auth;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test auth_test 2>&1 | tail -30`
Expected: all 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/auth.rs src/lib.rs tests/auth_test.rs Cargo.lock
git commit -m "feat: API key generation and auth middleware/extractor"
```

---

### Task 4: Namespace routing

**Files:**
- Create: `src/routing.rs`
- Modify: `src/lib.rs` (add `pub mod routing;`)
- Test: `tests/routing_test.rs`

**Interfaces:**
- Consumes: `AppState` (Task 3), `db::find_database_mapping` (Task 2), `AuthedOwner` (Task 3).
- Produces: `routing::resolve_namespace(pool: &PgPool, owner: &AuthedOwner) -> Result<String, StatusCode>` — `Ok(namespace)` on a mapping hit, `Err(StatusCode::NOT_FOUND)` when the authenticated owner has no `database_mappings` row.

- [ ] **Step 1: Write the failing test**

Create `tests/routing_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use axum::{Extension, Router};
use hivewarden::auth::{self, AppState, AuthedOwner};
use hivewarden::{db, routing};
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

async fn whereami(
    axum::extract::State(state): axum::extract::State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
) -> Result<String, StatusCode> {
    routing::resolve_namespace(&state.pool, &owner).await
}

fn test_app(pool: sqlx::PgPool) -> Router {
    Router::new()
        .route("/whereami", get(whereami))
        .layer(axum::middleware::from_fn_with_state(
            AppState { pool: pool.clone() },
            auth::auth_middleware,
        ))
        .with_state(AppState { pool })
}

async fn seed_valid_key(pool: &sqlx::PgPool, owner_type: &str, owner_id: Uuid) -> String {
    let user_id = if owner_type == "user" { owner_id } else { Uuid::new_v4() };
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(user_id)
        .bind(format!("route-{user_id}@example.com"))
        .execute(pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(owner_type)
    .bind(owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(pool)
    .await
    .unwrap();
    full_key
}

#[tokio::test]
async fn resolves_namespace_for_mapped_owner() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'user', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(format!("ns-{owner_id}"))
    .execute(&pool)
    .await
    .unwrap();
    let full_key = seed_valid_key(&pool, "user", owner_id).await;

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whereami")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        format!("ns-{owner_id}")
    );
}

#[tokio::test]
async fn returns_404_when_owner_has_no_mapping() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    let full_key = seed_valid_key(&pool, "user", owner_id).await;

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whereami")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test routing_test 2>&1 | tail -30`
Expected: FAIL to compile — `hivewarden::routing` doesn't exist yet.

- [ ] **Step 3: Write `routing.rs`**

Create `src/routing.rs`:

```rust
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
```

In `src/lib.rs`, add `pub mod routing;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test routing_test 2>&1 | tail -30`
Expected: both tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/routing.rs src/lib.rs tests/routing_test.rs Cargo.lock
git commit -m "feat: resolve authenticated owner to sqld namespace"
```

---

### Task 5: Reverse proxy handler + full wiring

**Files:**
- Create: `src/proxy.rs`
- Modify: `src/lib.rs` (add `pub mod proxy;`, change `app()` to take `AppState` and wire the full router)
- Modify: `src/main.rs` (build real `AppState` from `Config`, pass to `app()`)
- Modify: `tests/health_test.rs` (update for the new `app(state)` signature)
- Test: `tests/proxy_test.rs`

**Interfaces:**
- Consumes: `AppState`, `AuthedOwner`, `auth::auth_middleware` (Task 3), `routing::resolve_namespace` (Task 4).
- Produces: `pub fn app(state: AppState) -> axum::Router` (replaces Task 1's no-arg `app()` — this is the final, complete router for the whole plan).

- [ ] **Step 1: Write the failing test**

**Before writing the test, verify the namespace header name against the real sqld instance** (per this plan's Global Constraints note) — run `curl -i http://127.0.0.1:8081/` or check `podman logs` on the sqld container from Task 1's `podman-compose.yml` for any documented header name, and adjust `x-libsql-namespace` below if it differs.

Create `tests/proxy_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivewarden::auth::{self, AppState};
use hivewarden::db;
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

fn sqld_url() -> String {
    std::env::var("SQLD_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string())
}

#[tokio::test]
async fn valid_key_reaches_sqld_and_gets_a_real_response() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    let namespace = format!("proxytest-{owner_id}");
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'user', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&namespace)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("proxy-{owner_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let state = AppState { pool };
    let app = hivewarden::app(state);

    // sqld exposes a version endpoint at GET /version on its default HTTP
    // listener — proxying it through confirms the request actually reached
    // sqld (not a gateway-side stub) and that the namespace header was set.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/version")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_key_never_reaches_sqld() {
    let pool = test_pool().await;
    let app = hivewarden::app(AppState { pool });
    let resp = app
        .oneshot(Request::builder().uri("/version").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn valid_key_with_no_mapping_returns_404() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("nomap-{owner_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = hivewarden::app(AppState { pool });
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/version")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
```

(`SQLD_URL` needs to be set to `http://127.0.0.1:8081` — Task 1's `podman-compose.yml` port mapping — when running this test, since `proxy.rs`'s handler reads it from `AppState`/env; see Step 3 below for exactly where it's threaded through.)

- [ ] **Step 2: Run test to verify it fails**

Run: `SQLD_URL=http://127.0.0.1:8081 DATABASE_URL=postgres://gateway:gateway@127.0.0.1:5433/gateway cargo test --test proxy_test 2>&1 | tail -30`
Expected: FAIL to compile — `hivewarden::app` currently takes no arguments (Task 1's signature), and `proxy.rs` doesn't exist.

- [ ] **Step 3: Write `proxy.rs`**

Create `src/proxy.rs`:

```rust
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::Extension;

use crate::auth::{AppState, AuthedOwner};
use crate::routing;

const NAMESPACE_HEADER: &str = "x-libsql-namespace";

pub async fn proxy_handler(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    request: Request,
) -> Response {
    let namespace = match routing::resolve_namespace(&state.pool, &owner).await {
        Ok(ns) => ns,
        Err(status) => return status.into_response(),
    };

    let sqld_url = match std::env::var("SQLD_URL") {
        Ok(url) => url,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let (parts, body) = request.into_parts();
    let target = format!(
        "{}{}",
        sqld_url.trim_end_matches('/'),
        parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
    );

    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let client = reqwest::Client::new();
    let mut req_builder = client.request(
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap(),
        &target,
    );

    let mut forwarded_headers = HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        if name == header::AUTHORIZATION || name == header::HOST {
            continue;
        }
        forwarded_headers.insert(name.clone(), value.clone());
    }
    forwarded_headers.insert(
        HeaderName::from_static(NAMESPACE_HEADER),
        HeaderValue::from_str(&namespace).unwrap(),
    );

    for (name, value) in forwarded_headers.iter() {
        req_builder = req_builder.header(name, value);
    }
    req_builder = req_builder.body(body_bytes);

    let upstream_resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("proxy request to sqld failed: {e:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = upstream_resp.status();
    let headers = upstream_resp.headers().clone();
    let bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("reading sqld response failed: {e:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let mut response = Response::builder()
        .status(status.as_u16())
        .body(Body::from(bytes))
        .unwrap();
    *response.headers_mut() = headers
        .iter()
        .filter_map(|(k, v)| {
            Some((
                HeaderName::from_bytes(k.as_str().as_bytes()).ok()?,
                HeaderValue::from_bytes(v.as_bytes()).ok()?,
            ))
        })
        .collect();
    response
}
```

- [ ] **Step 4: Wire the full router in `lib.rs` and update `main.rs`**

Replace `src/lib.rs`:

```rust
pub mod auth;
pub mod config;
pub mod db;
pub mod proxy;
pub mod routing;

use axum::{Router, routing::{any, get}};
pub use auth::AppState;

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
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

(`.layer()` in axum only wraps routes that were already registered on the router *before* that call in the builder chain — routes added via `.route()` afterward are not wrapped. That's why `/{*path}`/`/` (registered before `.layer(auth_middleware)`) end up authenticated while `/healthz` (registered after) stays public, in one router with no `.merge()` needed. **Confirm this in Step 5's test run anyway** — if `/healthz` unexpectedly returns 401, split into two routers and `.merge()` them instead, with the auth layer applied only to the proxy router before merging.)

Replace `src/main.rs`:

```rust
use hivewarden::{app, config::Config, db, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    let state = AppState { pool };
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivewarden listening on {}", config.listen_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
```

Update `tests/health_test.rs`'s `healthz_returns_ok` test to build a real `AppState` (same `test_pool()` helper pattern as the other test files) and call `hivewarden::app(state)` instead of the old no-arg `app()`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `SQLD_URL=http://127.0.0.1:8081 DATABASE_URL=postgres://gateway:gateway@127.0.0.1:5433/gateway cargo test 2>&1 | tail -60`
Expected: every test file (health, db, auth, routing, proxy) passes. If `/healthz` returns 401 instead of 200, apply the `.merge()` fix noted in Step 4's comment and re-run.

- [ ] **Step 6: Commit**

```bash
git add src/proxy.rs src/lib.rs src/main.rs tests/health_test.rs tests/proxy_test.rs Cargo.lock
git commit -m "feat: reverse proxy to sqld, complete walking skeleton"
```
