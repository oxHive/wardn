# hivewarden Security Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the Critical and all ten Important findings from the full-codebase security audit — replace argon2 with HMAC-SHA256 for API keys, close a permission-escalation gap in the proxy, harden `POST /users`/`POST /orgs` against abuse, remove an unbounded-cardinality metrics label, make provisioning idempotent, add defense-in-depth around namespace isolation, guard against org self-lockout, reduce DB round-trips on the hot path, and fix a non-constant-time secret comparison.

**Architecture:** Ten independent-but-sequenced tasks, each touching a small, named set of files. Tasks 1, 3, and 4 all touch `src/registration.rs` and are ordered so each builds on the previous rather than conflicting. No new services or major structural changes — every fix is additive or narrowly corrective within the existing module layout.

**Tech Stack:** Rust/axum 0.8 (edition 2024), sqlx 0.8 (runtime-checked queries only), `hmac`+`sha2`+`hex` (replacing `argon2`), `tower_governor` (new, for rate limiting), `subtle` (new, for constant-time comparison).

## Global Constraints

- Rust edition 2024, axum 0.8, sqlx 0.8 — runtime-checked queries only (`sqlx::query`/`query_as`, never `query!`/`query_as!`).
- `#[tokio::test]` stays the bare, single-threaded-runtime attribute in every test file — no `flavor = "multi_thread"`.
- Integration tests exercise real Postgres and real sqld (`DATABASE_URL`/`SQLD_URL`/`SQLD_ADMIN_URL`, defaulting to the `podman-compose.yml` dev instances) — no mocks.
- New required env vars follow the existing `Config::from_env()` `.context(...)`-required pattern and get documented in `.env.example`.
- No backward-compatibility path for API keys issued before Task 1's HMAC migration — not needed, confirmed by the project owner (no real issued keys exist).
- Any change touching `src/proxy.rs` (Tasks 2, 7) gets the strictest review bar this project applies — that file has a documented history of two prior Critical cross-tenant vulnerabilities.
- Commit after each task's tests pass.

---

### Task 1: Replace argon2 with HMAC-SHA256 API key hashing

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/config.rs`
- Modify: `src/auth.rs`
- Modify: `src/main.rs`
- Modify: `src/registration.rs`
- Modify: `src/api_keys.rs`
- Modify: `.env.example`
- Modify: `tests/common/mod.rs`
- Modify (mechanical, see Step 8): every test file listed in Step 8

**Interfaces:**
- Produces: `pub fn auth::generate_api_key(pepper: &[u8]) -> (String, String, String)` (signature change — was zero-arg).
- Produces: `pub fn auth::verify_key(pepper: &[u8], full_key: &str, hash: &str) -> bool` (signature change — was two-arg).
- Produces: `AppState::new(pool: PgPool, sqld_url: String, api_key_pepper: String) -> Self` (signature change — was two-arg; `api_key_pepper` becomes a **required constructor parameter**, not a builder method, because it's load-bearing for every authenticated request, unlike `metrics_token`/`sqld_admin_url`/`metrics_handle` which stay optional builders for single-endpoint concerns).
- Produces: `Config::api_key_pepper: String`.
- Produces (test-only): `common::TEST_API_KEY_PEPPER: &str`.

- [ ] **Step 1: Update `Cargo.toml`**

Remove:
```toml
argon2 = "0.5"
```

Add, under `[dependencies]`:
```toml
hmac = "0.12"
sha2 = "0.10"
hex = "0.4"
```

Run: `cargo build`
Expected: fails — `src/auth.rs` still references `argon2`. That's expected; Step 3 fixes it.

- [ ] **Step 2: Add `api_key_pepper` to `Config`**

In `src/config.rs`, change to:

```rust
use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub sqld_url: String,
    pub sqld_admin_url: String,
    pub listen_addr: String,
    pub metrics_token: String,
    pub api_key_pepper: String,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let metrics_token =
            std::env::var("METRICS_TOKEN").context("METRICS_TOKEN must be set")?;
        if metrics_token.is_empty() {
            anyhow::bail!(
                "METRICS_TOKEN must not be empty — an empty value fails closed and permanently \
                 401s GET /metrics"
            );
        }
        let api_key_pepper =
            std::env::var("API_KEY_PEPPER").context("API_KEY_PEPPER must be set")?;
        if api_key_pepper.len() < 32 {
            anyhow::bail!(
                "API_KEY_PEPPER must be at least 32 characters — it's the server-side secret \
                 folded into every API key's hash, and a short value defeats the point of a \
                 pepper"
            );
        }
        Ok(Config {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            sqld_url: std::env::var("SQLD_URL").context("SQLD_URL must be set")?,
            sqld_admin_url: std::env::var("SQLD_ADMIN_URL")
                .context("SQLD_ADMIN_URL must be set")?,
            listen_addr: std::env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8787".to_string()),
            metrics_token,
            api_key_pepper,
        })
    }
}
```

- [ ] **Step 3: Rewrite `src/auth.rs`'s hashing to HMAC-SHA256**

Replace the top imports:

```rust
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use rand::Rng;
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::proxy::ProxyClient;

type HmacSha256 = Hmac<Sha256>;
```

(Removes `argon2::*` imports, `std::sync::LazyLock` — no longer needed since the dummy hash is cheap to recompute per call, see Step 3d below.)

Change `AppState` and its constructor to make `api_key_pepper` a required constructor argument:

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
    /// Renders `GET /metrics` (`src/observability.rs`). `new` builds a local,
    /// non-globally-installed handle so every existing call site keeps
    /// compiling unchanged; `main.rs` overrides it via `with_metrics_handle`
    /// with the handle tied to the globally installed recorder — the one the
    /// `metrics::counter!`/`gauge!`/`histogram!` calls elsewhere in this
    /// crate actually feed. A handle not tied to the global recorder still
    /// renders successfully, just always as empty output.
    pub metrics_handle: PrometheusHandle,
    /// Shared secret required as `Authorization: Bearer <token>` on `GET
    /// /metrics` (`src/observability.rs`) — added because the per-tenant
    /// `namespace` label on proxy metrics would otherwise let anyone
    /// enumerate every tenant's UUID and traffic volume through an
    /// unauthenticated endpoint. Defaults to empty via `new`, which makes
    /// `metrics_handler` reject every request (fail closed) until
    /// `with_metrics_token` sets a real value — `main.rs` does this from
    /// `Config::metrics_token`.
    pub metrics_token: String,
    /// Server-side secret folded into every API key's HMAC-SHA256 hash (see
    /// `hash_key`/`generate_api_key`/`verify_key` below). Unlike
    /// `metrics_token`/`sqld_admin_url`, this is a **required constructor
    /// argument**, not an optional builder: it's load-bearing for every
    /// single authenticated request (`auth_middleware`), not one endpoint —
    /// an empty-by-default value here would make every key verification
    /// silently use an empty pepper until someone remembered to set it,
    /// which is exactly the kind of security-relevant omission that should
    /// be a compile error, not a runtime footgun.
    pub api_key_pepper: String,
}

impl AppState {
    pub fn new(pool: PgPool, sqld_url: String, api_key_pepper: String) -> Self {
        let metrics_handle = PrometheusBuilder::new().build_recorder().handle();
        Self {
            pool,
            sqld_url,
            sqld_admin_url: String::new(),
            client: ProxyClient::new(),
            metrics_handle,
            metrics_token: String::new(),
            api_key_pepper,
        }
    }

    pub fn with_sqld_admin_url(mut self, sqld_admin_url: String) -> Self {
        self.sqld_admin_url = sqld_admin_url;
        self
    }

    pub fn with_metrics_handle(mut self, metrics_handle: PrometheusHandle) -> Self {
        self.metrics_handle = metrics_handle;
        self
    }

    pub fn with_metrics_token(mut self, metrics_token: String) -> Self {
        self.metrics_token = metrics_token;
        self
    }
}
```

Replace the key-generation/verification section (everything from `pub const KEY_MARKER` through the old `verify_key` function) with:

```rust
/// Marker every issued key starts with. The DB lookup prefix is derived from
/// the *random* part only — including this constant in the prefix would burn
/// 8 of its characters on a value that is identical for every key.
pub const KEY_MARKER: &str = "hm_live_";

/// Number of characters of the random part used as the DB lookup prefix.
/// 16 characters from a 62-symbol alphabet is ~62^16 ≈ 2^95 possibilities, so
/// the `UNIQUE` constraint on `api_keys.prefix` will never realistically be
/// hit by an issuance collision.
const PREFIX_LEN: usize = 16;

/// Derives the `api_keys.prefix` lookup value from a presented bearer token.
/// Returns `None` for anything that isn't one of our keys, so the caller can
/// reject it without touching the database.
pub fn prefix_of(full_key: &str) -> Option<String> {
    let random_part = full_key.strip_prefix(KEY_MARKER)?;
    let prefix: String = random_part.chars().take(PREFIX_LEN).collect();
    if prefix.chars().count() < PREFIX_LEN {
        return None;
    }
    Some(prefix)
}

/// HMAC-SHA256 of `full_key`, keyed by `pepper`, as a lowercase hex string.
///
/// API keys are 32 random characters from a 62-symbol alphabet (~190 bits of
/// entropy, see `generate_api_key`) — nowhere near brute-forceable, so unlike
/// a human-chosen password there is no reason to slow this down with a
/// memory-hard KDF. A slow hash on every request (this project previously
/// used argon2) is instead a straightforward unauthenticated-DoS amplifier:
/// every request, including ones with a bogus key, paid ~50-100ms of CPU and
/// ~19MiB of memory synchronously on the async runtime. HMAC-SHA256 costs
/// about 1 microsecond. `pepper` is a server-side secret distinct from any
/// individual key, so a leaked `key_hash` column alone doesn't let an
/// attacker forge valid keys without also knowing it.
fn hash_key(pepper: &[u8], full_key: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(pepper).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(full_key.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Generates a new API key. Returns (full_key, prefix, hash) — the caller
/// shows full_key to the user exactly once and stores only prefix+hash.
pub fn generate_api_key(pepper: &[u8]) -> (String, String, String) {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let random_part: String = (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let full_key = format!("{KEY_MARKER}{random_part}");
    let prefix: String = random_part.chars().take(PREFIX_LEN).collect();
    let hash = hash_key(pepper, &full_key);
    (full_key, prefix, hash)
}

/// Verifies `full_key` against a stored `hash` (hex-encoded HMAC-SHA256).
/// `Mac::verify_slice` compares in constant time internally — no separate
/// constant-time-comparison crate needed for this path.
pub fn verify_key(pepper: &[u8], full_key: &str, hash: &str) -> bool {
    let Ok(expected) = hex::decode(hash) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(pepper) else {
        return false;
    };
    mac.update(full_key.as_bytes());
    mac.verify_slice(&expected).is_ok()
}
```

Update `auth_middleware`'s unknown-prefix branch to compute the dummy hash inline (no `LazyLock` needed — HMAC is cheap enough to just recompute per call):

```rust
    let Some(row) = row else {
        // Unknown prefix, or a revoked key — `find_api_key_by_prefix` only
        // matches non-revoked rows, so the two cases are indistinguishable
        // here, and both already get the same 401.
        //
        // Verify against a throwaway hash anyway. Without this, an unknown
        // prefix returns in microseconds while a known-but-wrong key pays
        // the same ~1us HMAC cost as a genuine verification — a timing
        // oracle for "does this prefix exist," same reasoning as before this
        // moved from argon2 to HMAC, just at a much smaller absolute cost.
        let dummy_hash = hash_key(state.api_key_pepper.as_bytes(), "not-a-real-api-key");
        let _ = verify_key(state.api_key_pepper.as_bytes(), full_key, &dummy_hash);
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !verify_key(state.api_key_pepper.as_bytes(), full_key, &row.key_hash) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
```

(This replaces the two lines that called the old zero/two-arg `verify_key` and referenced `DUMMY_HASH`.)

- [ ] **Step 4: Run `cargo build` to confirm `src/auth.rs` compiles**

Run: `cargo build`
Expected: fails — `src/main.rs`, `src/registration.rs`, `src/api_keys.rs` still call the old signatures. Steps 5-6 fix them.

- [ ] **Step 5: Update `src/main.rs`**

Change the `AppState::new(...)` call:

```rust
    let state = AppState::new(pool, config.sqld_url.clone(), config.api_key_pepper.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone())
        .with_metrics_handle(metrics_handle)
        .with_metrics_token(config.metrics_token.clone());
```

- [ ] **Step 6: Update `src/registration.rs` and `src/api_keys.rs`**

In `src/registration.rs`, change `insert_user`'s signature and body to accept and pass the pepper:

```rust
async fn insert_user(
    pool: &PgPool,
    email: &str,
    api_key_pepper: &[u8],
) -> Result<(Uuid, String, OutboxRow), sqlx::Error> {
    let user_id = Uuid::new_v4();
    let outbox_id = Uuid::new_v4();
    let namespace = user_id.to_string();
    let (full_key, prefix, hash) = auth::generate_api_key(api_key_pepper);
```

(Only the function signature and the `generate_api_key()` call change — everything else in `insert_user` stays identical.)

Update `create_user`'s call site:

```rust
    match insert_user(&state.pool, &req.email, state.api_key_pepper.as_bytes()).await {
```

In `src/api_keys.rs`, update `create_key`'s call site:

```rust
    let (full_key, prefix, hash) = auth::generate_api_key(state.api_key_pepper.as_bytes());
```

- [ ] **Step 7: Run `cargo build` to confirm production code compiles**

Run: `cargo build`
Expected: succeeds (production code). Test binaries still fail — Step 8 fixes them.

- [ ] **Step 8: Update every test call site**

Add to `tests/common/mod.rs`, near `TEST_METRICS_TOKEN`:

```rust
/// Fixed pepper every test that calls `auth::generate_api_key`/`verify_key`
/// or constructs an `AppState` should use — `AppState::new` now requires an
/// `api_key_pepper` argument (unlike `metrics_token`, it's not an optional
/// builder), and a key seeded with one pepper only verifies successfully
/// against an `AppState` built with the *same* pepper. Must be at least 32
/// characters (see `config.rs`'s validation) even though tests don't go
/// through `Config::from_env`.
pub const TEST_API_KEY_PEPPER: &str = "test-api-key-pepper-do-not-use-in-prod";
```

Two mechanical, compiler-enforced changes apply across every test file below:

1. **Every `AppState::new(pool, sqld_url)` call becomes `AppState::new(pool, sqld_url, common::TEST_API_KEY_PEPPER.to_string())`** (or the equivalent with whatever variable names that call site already uses for `pool`/`sqld_url` — only the third argument is new). Find every call site with `grep -rn "AppState::new" tests/`.
2. **Every `auth::generate_api_key()` (or `hivewarden::auth::generate_api_key()`) call becomes `auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes())`** (fully-qualified call sites keep their full path, just add the argument). Find every call site with `grep -rn "generate_api_key()" tests/`.

If a test file doesn't already have `mod common;` at its top, add it — every file below needs `common::TEST_API_KEY_PEPPER`.

Files with `AppState::new` and/or `generate_api_key()` call sites to update (confirmed via the greps above at plan-writing time — re-run them yourself in case anything drifted):
`tests/observability_test.rs`, `tests/auth_test.rs`, `tests/routing_test.rs`, `tests/health_test.rs`, `tests/proxy_test.rs`, `tests/org_proxy_test.rs`, `tests/org_admin_test.rs`, `tests/grpc_proxy_test.rs`, `tests/registration_test.rs`, `tests/org_members_test.rs`, `tests/api_keys_test.rs`, `tests/usage_metering_test.rs`.

This is a compiler-enforced change: `cargo build --tests` will not succeed until every site is updated, and there is no way to silently miss one.

- [ ] **Step 9: Update `.env.example`**

Add, after the `METRICS_TOKEN` line:

```
# Server-side secret folded into every API key's HMAC-SHA256 hash
# (src/auth.rs) — must be at least 32 characters. A leaked key_hash column
# alone doesn't let an attacker forge valid keys without also knowing this.
API_KEY_PEPPER=dev-api-key-pepper-do-not-use-in-prod-min-32-chars
```

- [ ] **Step 10: Run the full test suite**

Run: `cargo test`
Expected: all binaries compile and pass. If any test fails with an auth mismatch (401 where 200 was expected), check that both the key-seeding call and the `AppState` the request goes through used the same pepper (`common::TEST_API_KEY_PEPPER`).

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock src/config.rs src/auth.rs src/main.rs src/registration.rs src/api_keys.rs .env.example tests/common/mod.rs tests/observability_test.rs tests/auth_test.rs tests/routing_test.rs tests/health_test.rs tests/proxy_test.rs tests/org_proxy_test.rs tests/org_admin_test.rs tests/grpc_proxy_test.rs tests/registration_test.rs tests/org_members_test.rs tests/api_keys_test.rs tests/usage_metering_test.rs
git commit -m "fix: replace argon2 with HMAC-SHA256 for API key hashing"
```

---

### Task 2: Fix permission escalation (`db:sync` implying `db:query`)

**Files:**
- Modify: `src/proxy.rs`
- Modify: `tests/org_proxy_test.rs`

**Interfaces:**
- Consumes: `roles::Permission::{DbQuery, DbSync}` (unchanged, existing).
- Produces: nothing new consumed by later tasks.

- [ ] **Step 1: Write the failing test**

Append to `tests/org_proxy_test.rs`:

```rust
/// The permission gate must key off the actual protocol being spoken (the
/// request path), not the client's chosen HTTP version. Before this fix, a
/// `db:sync`-only member could run arbitrary Hrana SQL by simply sending it
/// over an h2c connection instead of HTTP/1.1 — the gate saw HTTP/2 and asked
/// for `db:sync`, which they legitimately hold, granting full read/write SQL
/// they were never given `db:query` for.
#[tokio::test]
async fn member_with_only_db_sync_cannot_run_hrana_queries_over_h2c() {
    let pool = test_pool().await;
    let namespace = format!("orgescalate-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let (org_id, key) = seed_org_member(&pool, &namespace, &[Permission::DbSync]).await;
    let gateway = spawn_gateway(pool).await;

    let mut builder = HyperClient::builder(TokioExecutor::new());
    builder.http2_only(true);
    let client: HyperClient<HttpConnector, Full<Bytes>> = builder.build(HttpConnector::new());
    let request = hyper::Request::builder()
        .method("POST")
        .uri(format!("{gateway}/"))
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-org-id", org_id.to_string())
        .body(Full::new(Bytes::from_static(
            br#"{"statements":["SELECT 1"]}"#,
        )))
        .unwrap();
    let status = client
        .request(request)
        .await
        .expect("request failed")
        .status();
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "db:sync-only member ran a Hrana query over h2c — permission escalation"
    );

    delete_namespace(&namespace).await;
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test org_proxy_test member_with_only_db_sync_cannot_run_hrana_queries_over_h2c -- --nocapture`
Expected: FAIL — `status` is `200 OK`, not `403 FORBIDDEN` (the bug being fixed).

- [ ] **Step 3: Fix `src/proxy.rs`**

Add a constant near the top of the file, after the `X_ORG_ID` constant:

```rust
/// Path prefixes sqld's gRPC services live under. Only `/wal_log.` is
/// verified against real traffic this gateway proxies today (hivemind's
/// embedded-replica sync speaks `/wal_log.ReplicationLog/*`) — if another
/// sqld gRPC service (e.g. its `Proxy` write-forwarding service) is ever
/// proxied through here, its prefix belongs in this list too. Used to
/// classify a request's *actual* protocol for the permission gate below,
/// rather than trusting the client's chosen HTTP version — a Hrana/SQL
/// request can legally arrive over HTTP/2 as well as HTTP/1.1, so the
/// version alone doesn't distinguish "this is a gRPC replication call" from
/// "this is a SQL query that happens to use h2c."
const GRPC_PATH_PREFIXES: [&str; 1] = ["/wal_log."];

fn is_grpc_path(path: &str) -> bool {
    GRPC_PATH_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}
```

Replace the `required` permission computation inside `proxy_handler`'s `Some(Ok(org_id))` arm:

```rust
        Some(Ok(org_id)) => {
            org_id_for_usage = Some(org_id);
            // Gates on the request's actual path, not the client's chosen
            // HTTP version — a Hrana/SQL request is legal over both HTTP/1.1
            // and HTTP/2, so the version alone previously let a db:sync-only
            // caller run full SQL by sending it over h2c. `db:sync` gates
            // sqld's gRPC replication paths; `db:query` gates everything
            // else (Hrana/HTTP), regardless of transport.
            //
            // `require_permission` takes the whole `AuthedOwner` and rejects
            // a non-`user` owner_type itself — a workspace/org-owned key's
            // owner_id names a workspace/org, not a user, and must never be
            // looked up as an `org_members.user_id`.
            let path = parts.uri.path();
            let required = if is_grpc_path(path) {
                Permission::DbSync
            } else {
                Permission::DbQuery
            };
            // A gRPC path can only be legitimately served over HTTP/2 —
            // reject the HTTP/1.1 combination outright rather than
            // forwarding it and depending on sqld to reject it.
            if is_grpc_path(path) && parts.version != Version::HTTP_2 {
                return StatusCode::BAD_REQUEST.into_response();
            }
            if let Err(status) =
                roles::require_permission(&state.pool, org_id, &owner, required).await
            {
                return status.into_response();
            }
            match routing::resolve_org_namespace(&state.pool, org_id).await {
                Ok(ns) => ns,
                Err(status) => return status.into_response(),
            }
        }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test org_proxy_test member_with_only_db_sync_cannot_run_hrana_queries_over_h2c -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the full `org_proxy_test` and `grpc_proxy_test` binaries**

Run: `cargo test --test org_proxy_test` and `cargo test --test grpc_proxy_test`
Expected: all pass, including `member_with_db_sync_reaches_the_orgs_namespace_over_h2c` (a real gRPC call to `/wal_log.ReplicationLog/Hello`, which must still work — it's an `is_grpc_path` match, over HTTP/2, exactly the case this fix is supposed to keep allowing) and `member_without_db_sync_is_forbidden_over_h2c` (unchanged behavior).

- [ ] **Step 6: Commit**

```bash
git add src/proxy.rs tests/org_proxy_test.rs
git commit -m "fix: gate proxy permission on request path, not HTTP version"
```

---

### Task 3: Email validation and normalization on `POST /users`

**Files:**
- Create: `migrations/0005_case_insensitive_email.sql`
- Modify: `src/registration.rs`
- Modify: `tests/registration_test.rs`

**Interfaces:**
- Produces: `fn registration::validate_and_normalize_email(email: &str) -> Result<String, Response>` — not consumed by other tasks, but Task 4 (rate limiting) shares this file and must not conflict with this task's edits.

- [ ] **Step 1: Write the migration**

Create `migrations/0005_case_insensitive_email.sql`:

```sql
-- `users.email`'s original UNIQUE constraint is case-sensitive, which let
-- `foo@example.com` and `Foo@example.com` exist as two separate accounts —
-- the root cause of the case-variant-account ambiguity `org/members.rs`'s
-- `add_member` has to defend against with a fetch-all-and-refuse-to-guess
-- branch instead of a simple lookup. `POST /users` now normalizes to
-- lowercase before insert (see src/registration.rs), so this constraint
-- enforces the same invariant at the database level for defense in depth.
--
-- Uses a separate unique index rather than replacing the column-level
-- UNIQUE, since dropping the auto-named constraint requires knowing its
-- generated name and this is simpler and equally effective.
CREATE UNIQUE INDEX users_email_lower_idx ON users (lower(email));
```

- [ ] **Step 2: Run the migration**

Run: `cargo test --test health_test` (any test touching `db::connect` runs pending migrations; `health_test` is the smallest/fastest one that does)
Expected: passes, and the new index is created. If it fails with a uniqueness violation, the dev database already has case-variant duplicate emails from earlier test runs — run `scripts/reset-dev-db.sh` first, then retry.

- [ ] **Step 3: Write the failing test**

Append to `tests/registration_test.rs`:

```rust
#[tokio::test]
async fn create_user_rejects_invalid_email() {
    let pool = test_pool().await;
    let app = hivewarden::app(test_state(pool));

    for bad_email in ["", "not-an-email", "@example.com", "foo@", "foo@@example.com"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/users")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"email":"{bad_email}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected {bad_email:?} to be rejected"
        );
    }
}

#[tokio::test]
async fn create_user_normalizes_email_case_and_rejects_case_variant_duplicates() {
    let pool = test_pool().await;
    let app = hivewarden::app(test_state(pool));
    let email = format!("MixedCase-{}@Example.com", Uuid::new_v4());

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

    // Same address, different case — must be treated as the same account.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{}"}}"#,
                    email.to_lowercase()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
```

(Uses whatever `test_state(pool)` helper this file already has for building an `AppState` with the Task-1 pepper argument — check the top of `tests/registration_test.rs` for the existing pattern other tests in this file use, and match it exactly. If no such helper exists yet, build inline: `AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string())`.)

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test --test registration_test create_user_rejects_invalid_email create_user_normalizes_email_case_and_rejects_case_variant_duplicates -- --nocapture`
Expected: FAIL — no validation exists yet, so malformed emails currently get `201`/`500` instead of `400`, and the case-variant duplicate currently gets `201` (a second account) instead of `409`.

- [ ] **Step 5: Add validation to `src/registration.rs`**

Add near the top of the file, after the imports:

```rust
/// Validates and lowercases an email address before it reaches the
/// database. Deliberately simple (not a full RFC 5321 parser): non-empty,
/// within RFC 5321's 254-character maximum, exactly one `@` with non-empty
/// local and domain parts, no control characters or whitespace. Returns the
/// normalized (lowercased) form on success, or the 400 response to return
/// directly on failure.
fn validate_and_normalize_email(email: &str) -> Result<String, Response> {
    let bad_request = || (StatusCode::BAD_REQUEST, "invalid email address").into_response();

    if email.is_empty() || email.len() > 254 {
        return Err(bad_request());
    }
    if email.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(bad_request());
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad_request());
    };
    if local.is_empty() || domain.is_empty() {
        return Err(bad_request());
    }
    Ok(email.to_lowercase())
}
```

Update `create_user` to validate before calling `insert_user`:

```rust
pub async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Response {
    let email = match validate_and_normalize_email(&req.email) {
        Ok(email) => email,
        Err(resp) => return resp,
    };
    match insert_user(&state.pool, &email, state.api_key_pepper.as_bytes()).await {
        Ok((user_id, api_key, outbox_row)) => {
```

(Only the first two lines and the `insert_user` call's first argument change — the rest of `create_user`'s body, from `if let Err(e) = provisioning::attempt_provisioning(...)` onward, stays exactly as it is.)

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test registration_test -- --nocapture`
Expected: all tests in this file pass, including the two new ones and the pre-existing `create_user_with_an_already_registered_email_returns_409`.

- [ ] **Step 7: Commit**

```bash
git add migrations/0005_case_insensitive_email.sql src/registration.rs tests/registration_test.rs
git commit -m "fix: validate and case-normalize email on POST /users"
```

---

### Task 4: Rate limiting and org-per-user quota on `POST /users`/`POST /orgs`

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Modify: `src/registration.rs`
- Modify: `src/main.rs`
- Modify: `tests/registration_test.rs`

**Interfaces:**
- Consumes: `registration::create_user`, `registration::create_org` (unchanged handler signatures).
- Produces: nothing new consumed by later tasks.

- [ ] **Step 1: Add `tower_governor`**

Run:
```sh
cargo add tower_governor
```

This resolves whatever the current published version is — do not hand-edit a version number in. **`tower_governor`'s exact construction API has shifted across versions** (some require `Box::leak`-ing the config to satisfy a `'static` lifetime bound, some don't). The code below reflects a commonly-stable shape (`GovernorConfigBuilder` → `GovernorLayer::new`), but if it doesn't match what `cargo add` resolved, check that version's docs.rs page for the `GovernorLayer`/`GovernorConfigBuilder` construction example and adapt — the important part is the *behavior* (per-IP, N requests per time window, applied only to `/users` and `/orgs`), not the exact incantation.

- [ ] **Step 2: Write the failing test**

Append to `tests/registration_test.rs`:

```rust
/// Real server, real TCP client — `tower_governor`'s IP extraction needs a
/// genuine `ConnectInfo`, which `oneshot` never provides (see
/// `tests/org_proxy_test.rs`'s `spawn_gateway` for the same pattern used for
/// h2c tests). Sends more requests than the configured limit from one
/// address and asserts the overflow gets 429.
#[tokio::test]
async fn post_users_rate_limits_by_ip() {
    let pool = test_pool().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = hivewarden::app(test_state(pool))
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let mut last_status = None;
    // One more than the configured burst — see src/lib.rs's governor config
    // for the exact number this must exceed.
    for _ in 0..7 {
        let resp = client
            .post(format!("http://{addr}/users"))
            .header("content-type", "application/json")
            .body(format!(r#"{{"email":"ratelimit-{}@example.com"}}"#, Uuid::new_v4()))
            .send()
            .await
            .unwrap();
        last_status = Some(resp.status());
    }
    assert_eq!(
        last_status.unwrap().as_u16(),
        429,
        "expected the request past the rate limit to be rejected"
    );
}
```

(Uses the same `test_state(pool)` helper referenced in Task 3 — if this file doesn't have one yet, define it once: `fn test_state(pool: sqlx::PgPool) -> AppState { AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()) }`, and use it consistently for both this test and Task 3's tests.)

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --test registration_test post_users_rate_limits_by_ip -- --nocapture`
Expected: FAIL — every request succeeds (or fails for unrelated reasons like duplicate emails, which this test avoids via unique addresses), none get 429, since no rate limiting exists yet.

- [ ] **Step 4: Wire the governor layer into `src/lib.rs`**

Add the import and a config constant near the top:

```rust
use std::time::Duration;

use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};
```

Change `app()` to apply a governor layer scoped to just `/users` via `.route_layer(...)` (this wraps only that route's handler, not the whole router, so `/healthz`/`/metrics`/the proxy paths/every other admin endpoint stay unaffected). **`/orgs` deliberately does NOT get the IP rate limiter** — it's authenticated (unlike `/users`), so the per-user org quota added in Step 8 below is a more precise defense than an IP-based one (a legitimate customer behind a shared/NAT IP creating orgs shouldn't be limited by how many *other* people share their IP; the quota already caps abuse per identity, which is the thing that actually matters once a caller is authenticated):

```rust
pub fn app(state: AppState) -> Router {
    let registration_governor_config = GovernorConfigBuilder::default()
        .per_second(720) // 5/hour/IP — see the design spec's finding #3.
        .burst_size(5)
        .finish()
        .expect("static governor config values are always valid");
    let registration_limiter = GovernorLayer::new(&registration_governor_config);

    Router::new()
        .route(
            "/api-keys",
            get(api_keys::list_keys).post(api_keys::create_key),
        )
        .route("/api-keys/{id}", delete(api_keys::revoke_key))
        .route("/orgs", post(registration::create_org))
        .route(
            "/orgs/{org_id}/members",
            get(org::members::list_members).post(org::members::add_member),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}",
            delete(org::members::remove_member),
        )
        .route(
            "/orgs/{org_id}/roles",
            post(org::admin::create_role).get(org::admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org::admin::update_role).delete(org::admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org::admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .route("/metrics", get(observability::metrics_handler))
        .route(
            "/users",
            post(registration::create_user).route_layer(registration_limiter),
        )
        .with_state(state)
}
```

(`per_second(720)` + `burst_size(5)` approximates "5 requests, then a slow trickle" — `tower_governor`'s model is a token-bucket refilling at `per_second`, not a strict rolling-hour window; a burst of 5 immediately followed by one token every 720 seconds is close enough to "5/hour" for this project's current needs. If the resolved `tower_governor` version's builder methods differ from `per_second`/`burst_size`, check its docs for the equivalent and keep the same effective shape: a small burst allowance, a slow refill.)

If `GovernorConfigBuilder::default()` (with no explicit `IpKeyExtractor` configured) doesn't default to peer-IP extraction in the resolved version, explicitly set one — check the crate's docs for whichever of `PeerIpKeyExtractor`/`SmartIpKeyExtractor` it exposes and use it (`PeerIpKeyExtractor` is the simpler, more predictable choice for this project, since there's no reverse proxy in front of the gateway yet to make `X-Forwarded-For` trustworthy).

- [ ] **Step 5: Enable `ConnectInfo` in `src/main.rs`**

`tower_governor`'s IP extraction needs real connection info, which requires the server to be built with `into_make_service_with_connect_info`. Change the final lines of `main`:

```rust
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!("hivewarden listening on {}", config.listen_addr);
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test --test registration_test post_users_rate_limits_by_ip -- --nocapture`
Expected: PASS.

- [ ] **Step 7: Write the failing test for the org quota**

Append to `tests/registration_test.rs`:

```rust
#[tokio::test]
async fn create_org_is_capped_per_user() {
    let pool = test_pool().await;
    let app = hivewarden::app(test_state(pool));
    let (_user_id, api_key) =
        register(app.clone(), &format!("orgquota-{}@example.com", Uuid::new_v4())).await;

    // The cap is 10 (see src/registration.rs's ORG_QUOTA_PER_USER) — create
    // exactly that many, all of which must succeed, then confirm the next
    // one is rejected.
    for i in 0..10 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/orgs")
                    .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"name":"org-{i}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "org {i} should have succeeded");
    }

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"one-too-many"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}
```

(Uses `register(...)`, an existing helper already present in `tests/registration_test.rs` for creating a user and returning `(user_id, api_key)` — check the top of that file for its exact signature and reuse it as-is; do not redefine it.)

Note: `/orgs` has no IP rate limiter (see Step 4's design note), so this test's 11 requests are governed only by the per-user quota being added in Step 8 below — no conflict with Task 4's `/users` rate limiter, which this test never touches.

- [ ] **Step 8: Add the quota to `src/registration.rs`**

Add a constant and a check at the top of `create_org`:

```rust
/// Maximum orgs a single user may create. This project has no billing/plan
/// tiers yet to derive a real number from — 10 is a conservative ceiling
/// against runaway namespace creation (finding #3/#4), not a product
/// decision about how many orgs a legitimate customer needs.
const ORG_QUOTA_PER_USER: i64 = 10;

pub async fn create_org(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Json(req): Json<CreateOrgRequest>,
) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let owned_org_count: Result<(i64,), sqlx::Error> = sqlx::query_as(
        "SELECT COUNT(*) FROM org_members om
         JOIN roles r ON r.id = om.role_id
         WHERE om.user_id = $1 AND r.name = 'owner'",
    )
    .bind(owner.owner_id)
    .fetch_one(&state.pool)
    .await;
    match owned_org_count {
        Ok((count,)) if count >= ORG_QUOTA_PER_USER => {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!("org quota check failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    match insert_org(&state.pool, &req.name, owner.owner_id).await {
```

(Only the new block between the `owner_type` check and the existing `match insert_org(...)` line is added — everything from `Ok((org_id, outbox_row)) => {` onward stays exactly as it is. Note this quota query matches on role *name* `'owner'`, the bootstrap role every `create_org` call creates — see `insert_org`'s existing `INSERT INTO roles (id, org_id, name) VALUES ($1, $2, 'owner')` line. This is deliberately about *org creation*, not general org membership: a user who is a plain member of 50 other people's orgs is unaffected.)

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test --test registration_test -- --nocapture`
Expected: all tests in this file pass, including the two new ones from this task and Task 3's two tests.

- [ ] **Step 10: Run the full test suite**

Run: `cargo test`
Expected: all binaries pass — the `route_layer` change in `src/lib.rs` only affects `/users`/`/orgs`, so no other test should be impacted, but confirm.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/registration.rs src/main.rs tests/registration_test.rs
git commit -m "feat: rate limit and cap org creation on POST /users and /orgs"
```

---

### Task 5: Drop `namespace` label from proxy metrics

**Files:**
- Modify: `src/observability.rs`
- Modify: `src/proxy.rs`
- Modify: `tests/observability_test.rs`
- Modify: `docs/superpowers/specs/2026-08-12-observability-design.md`

**Interfaces:**
- Produces: `pub fn observability::record_proxy_metrics(protocol: &'static str, status: StatusCode, duration: Duration)` (signature change — drops the `namespace: &str` parameter).

- [ ] **Step 1: Write the failing test**

In `tests/observability_test.rs`, find `successful_request_increments_counter_and_histogram` (added when proxy metrics were first built) and replace its label assertions — the test should now confirm `namespace` is **absent**, not present:

```rust
    let rendered = handle.render();
    assert!(
        common::has_labeled_metric(
            &rendered,
            "gateway_proxy_requests_total",
            &["protocol=\"query\"", "status_class=\"2xx\""],
        ),
        "missing counter sample: {rendered}"
    );
    assert!(
        !rendered.contains("gateway_proxy_requests_total{") || !rendered.lines().any(|line| {
            line.starts_with("gateway_proxy_requests_total{") && line.contains("namespace=")
        }),
        "gateway_proxy_requests_total must not carry a namespace label: {rendered}"
    );
    assert!(
        common::has_labeled_metric(
            &rendered,
            "gateway_proxy_request_duration_seconds_count",
            &["protocol=\"query\""],
        ),
        "missing histogram sample: {rendered}"
    );
```

(Replace the old assertions that built a `namespace_label` string and checked for its presence — this task removes that check and adds the inverse: asserting the label is gone.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test observability_test successful_request_increments_counter_and_histogram -- --nocapture`
Expected: FAIL — the negative assertion fails because `namespace=` is still present (the label hasn't been removed from the code yet).

- [ ] **Step 3: Remove the label from `src/observability.rs`**

Replace `record_proxy_metrics`:

```rust
/// Records one proxied request's outcome: a `gateway_proxy_requests_total`
/// increment and a `gateway_proxy_request_duration_seconds` observation.
/// Called only from the same two points in `proxy_handler` that already emit
/// a usage event (`src/proxy.rs`) — a request rejected before reaching sqld
/// never calls this, matching `gateway_proxy_requests_total`'s definition as
/// proxy traffic that actually reached sqld. `duration` measures request
/// entry to response *head*, not full transfer — on the `sync` path, where
/// the streaming gRPC body is the long-lived part, it says very little about
/// how long the request really took (same caveat `src/proxy.rs` documents
/// for its usage-event `duration_ms`).
///
/// Deliberately carries **no per-tenant label**. `namespace` was dropped
/// (security-hardening finding #4): every namespace is permanently retained
/// in the Prometheus recorder's memory with no eviction, and every namespace
/// is created by a `POST /users` call — an unbounded, externally-triggerable
/// memory-growth vector. Per-tenant attribution stays available through the
/// `usage`-target tracing events (`src/proxy.rs`'s `emit_usage_event`),
/// which already carry `namespace`/`owner_id`/`org_id` and are designed for
/// exactly this kind of high-cardinality data.
pub fn record_proxy_metrics(protocol: &'static str, status: StatusCode, duration: Duration) {
    let status_class = format!("{}xx", status.as_u16() / 100);
    metrics::counter!(
        "gateway_proxy_requests_total",
        "protocol" => protocol,
        "status_class" => status_class,
    )
    .increment(1);
    metrics::histogram!(
        "gateway_proxy_request_duration_seconds",
        "protocol" => protocol,
    )
    .record(duration.as_secs_f64());
}
```

- [ ] **Step 4: Update the two call sites in `src/proxy.rs`**

Change both `record_proxy_metrics` calls to drop the `&namespace` argument:

```rust
            crate::observability::record_proxy_metrics(
                protocol,
                StatusCode::GATEWAY_TIMEOUT,
                start.elapsed(),
            );
```

and:

```rust
    crate::observability::record_proxy_metrics(
        protocol,
        upstream_parts.status,
        start.elapsed(),
    );
```

(These are the only two call sites — same two locations Task 2 left untouched.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --test observability_test -- --nocapture`
Expected: all tests in this file pass, including the modified one.

- [ ] **Step 6: Amend the observability design spec**

In `docs/superpowers/specs/2026-08-12-observability-design.md`, find the `gateway_proxy_requests_total`/`gateway_proxy_request_duration_seconds` rows in the metric catalog table and remove `namespace` from their Labels column. Add a short note above the table:

```markdown
**Amendment (security hardening, `docs/superpowers/specs/2026-08-13-security-hardening-design.md`):** the `namespace` label originally on `gateway_proxy_requests_total`/`gateway_proxy_request_duration_seconds` was removed — combined with this endpoint's per-tenant traffic data, it created an unbounded, externally-triggerable (via anonymous `POST /users`) memory-growth vector in the metrics recorder. Per-tenant attribution stays available through the `usage`-target tracing events.
```

- [ ] **Step 7: Run the full test suite**

Run: `cargo test`
Expected: all binaries pass.

- [ ] **Step 8: Commit**

```bash
git add src/observability.rs src/proxy.rs tests/observability_test.rs docs/superpowers/specs/2026-08-12-observability-design.md
git commit -m "fix: drop unbounded-cardinality namespace label from proxy metrics"
```

---

### Task 6: Provisioning idempotency

**Files:**
- Modify: `src/provisioning.rs`
- Modify: `tests/provisioning_test.rs`

**Interfaces:**
- Consumes: nothing new from earlier tasks.
- Produces: nothing new consumed by later tasks.

- [ ] **Step 1: Write the failing test**

Append to `tests/provisioning_test.rs`:

```rust
/// Simulates the exact stranding scenario the fix closes: the namespace was
/// already created by an earlier attempt (or the worker, racing the inline
/// attempt) but `database_mappings` was never written — e.g. the process
/// died between sqld's 200 and the transaction commit. Before the fix, this
/// row would fail identically forever (sqld's create returns 400 for an
/// existing namespace) and the tenant's key would resolve to a permanent
/// 404. After the fix, the retry recognizes the namespace already exists
/// and completes the mapping.
#[tokio::test]
async fn attempt_provisioning_recovers_when_namespace_exists_but_mapping_is_missing() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let namespace = format!("provrecover-{}", Uuid::new_v4());
    // Create the namespace directly, out of band — standing in for a prior
    // attempt whose sqld call succeeded but whose Postgres write never
    // landed.
    create_namespace_directly(&namespace).await;
    let row = seed_outbox_row(&pool, "user", &namespace).await;

    let result = provisioning::attempt_provisioning(&pool, &admin_url(), &row)
        .await
        .unwrap();
    assert!(result, "recovery attempt should be treated as success");

    let mapping: Option<(String,)> = sqlx::query_as(
        "SELECT sqld_namespace FROM database_mappings WHERE owner_type = $1 AND owner_id = $2",
    )
    .bind(&row.owner_type)
    .bind(row.owner_id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(mapping.unwrap().0, namespace);

    let (status, _attempts) = outbox_status(&pool, row.id).await;
    assert_eq!(status, "done");

    delete_namespace(&namespace).await;
    common::delete_outbox_row(&pool, row.id).await;
}

async fn create_namespace_directly(name: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/namespaces/{name}/create", admin_url()))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .expect("admin API create-namespace request failed");
    assert!(resp.status().is_success(), "failed to pre-create {name}");
}
```

(`test_pool`/`admin_url`/`seed_outbox_row`/`outbox_status`/`delete_namespace` are all existing helpers already in `tests/provisioning_test.rs` — reuse them as-is.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test provisioning_test attempt_provisioning_recovers_when_namespace_exists_but_mapping_is_missing -- --nocapture`
Expected: FAIL — `result` is `false` (sqld's create returns 400 for the already-existing namespace, and `attempt_provisioning` currently treats that as an ordinary failure), and no `database_mappings` row is ever written.

- [ ] **Step 3: Add existence-check recovery to `src/provisioning.rs`**

Add a helper and change the non-success arm of `attempt_provisioning`:

```rust
/// Confirms `namespace` genuinely exists on sqld, for the recovery path
/// below — a non-2xx from `create` is ambiguous between "already exists"
/// (recoverable) and "some other failure" (not), so this makes it concrete.
/// `GET /v1/namespaces/{name}` mirrors the `DELETE /v1/namespaces/{name}`
/// path this codebase's tests already use for cleanup (see e.g.
/// `tests/provisioning_test.rs`'s `delete_namespace`) — same resource path,
/// different verb, the standard REST-admin-API shape for "the base resource
/// path exists/doesn't" vs. the `/create` sub-path used only for creation.
async fn namespace_exists(sqld_admin_url: &str, namespace: &str) -> bool {
    PROVISIONING_CLIENT
        .get(format!(
            "{}/v1/namespaces/{}",
            sqld_admin_url.trim_end_matches('/'),
            namespace
        ))
        .send()
        .await
        .map(|resp| resp.status().is_success())
        .unwrap_or(false)
}
```

Replace the `match create_result` block's non-success arm:

```rust
    match create_result {
        Ok(resp) if resp.status().is_success() => {
            complete_provisioning(pool, row).await
        }
        Ok(resp) => {
            // A non-success create might mean the namespace already exists —
            // e.g. an earlier attempt's sqld call succeeded but the process
            // died before the database_mappings write landed, or the
            // background worker won a race against this inline attempt. If
            // it genuinely exists, this is a recovery, not a failure: finish
            // the same way a successful create would.
            if namespace_exists(sqld_admin_url, &row.sqld_namespace).await {
                complete_provisioning(pool, row).await
            } else {
                record_failure(
                    pool,
                    row,
                    &format!("sqld admin API returned {}", resp.status()),
                )
                .await?;
                crate::observability::record_provisioning_outcome("failure");
                Ok(false)
            }
        }
        Err(e) => {
            record_failure(pool, row, &format!("sqld admin API request failed: {e}")).await?;
            crate::observability::record_provisioning_outcome("failure");
            Ok(false)
        }
    }
```

Extract the success path (previously the `Ok(resp) if resp.status().is_success()` arm's body) into a shared helper, now called from both the direct-success and the recovered-via-existence-check paths, with `ON CONFLICT DO NOTHING` guarding against a partially-applied prior attempt:

```rust
/// Records `row`'s successful provisioning: the `database_mappings` insert
/// and the outbox row's `done` transition, in one transaction. `ON CONFLICT
/// DO NOTHING` on the mapping insert makes this safe to call twice for the
/// same `(owner_type, owner_id)` — the recovery path in `attempt_provisioning`
/// above can reach this after a namespace that already has its mapping
/// written (a genuinely duplicate recovery attempt), which would otherwise
/// fail the whole transaction on `database_mappings`' `UNIQUE (owner_type,
/// owner_id)` constraint.
async fn complete_provisioning(pool: &PgPool, row: &OutboxRow) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (owner_type, owner_id) DO NOTHING",
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
    crate::observability::record_provisioning_outcome("success");
    Ok(true)
}
```

Confirm this against the real container before trusting it: `podman-compose up`, then `curl` `GET /v1/namespaces/{name}` directly for both a namespace you just created and one you didn't, and check the status codes match "success only when it exists." If it doesn't, adjust `namespace_exists` to whatever the real admin API actually returns, keeping the same behavioral contract.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test provisioning_test attempt_provisioning_recovers_when_namespace_exists_but_mapping_is_missing -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the full `provisioning_test` binary**

Run: `cargo test --test provisioning_test`
Expected: all tests pass, including the pre-existing `attempt_provisioning_records_failure_and_stays_pending` (a genuinely unreachable admin URL, `http://127.0.0.1:1` — confirm this still fails as a real failure, not a false "namespace exists" recovery, since `namespace_exists` would also fail to connect and return `false` there).

- [ ] **Step 6: Commit**

```bash
git add src/provisioning.rs tests/provisioning_test.rs
git commit -m "fix: recover provisioning when a namespace exists but its mapping is missing"
```

---

### Task 7: Namespace isolation defense-in-depth

**Files:**
- Modify: `src/routing.rs`
- Modify: `src/proxy.rs`
- Modify: `tests/routing_test.rs`
- Modify: `tests/proxy_test.rs`

**Interfaces:**
- Consumes: nothing new from earlier tasks.
- Produces: nothing new consumed by later tasks.

- [ ] **Step 1: Write the failing test for `resolve_namespace`'s owner-type guard**

Append to `tests/routing_test.rs`:

```rust
/// `resolve_namespace` must reject anything that isn't a personal
/// (`owner_type == "user"`) key, even though no code path mints an
/// `org`/`workspace`-owned key today — the schema already permits it
/// (`database_mappings.owner_type` and `api_keys.owner_type` both allow
/// `'workspace'`/`'org'`), and without this guard the moment such a key
/// exists it would resolve straight to a shared namespace with the entire
/// role/permission system bypassed (the `X-Org-Id` path is the only
/// sanctioned route to an org's namespace, and it goes through
/// `roles::require_permission` — this path must not become a silent
/// alternative to it).
#[tokio::test]
async fn resolve_namespace_rejects_non_user_owner_type() {
    let pool = test_pool().await;
    let owner = AuthedOwner {
        owner_type: "org".to_string(),
        owner_id: Uuid::new_v4(),
    };
    let result = routing::resolve_namespace(&pool, &owner).await;
    assert_eq!(result, Err(StatusCode::FORBIDDEN));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test routing_test resolve_namespace_rejects_non_user_owner_type -- --nocapture`
Expected: FAIL — `resolve_namespace` currently does a `database_mappings` lookup regardless of `owner_type`, returning `Err(StatusCode::NOT_FOUND)` (no mapping exists for this random `owner_id`), not `Err(StatusCode::FORBIDDEN)`.

- [ ] **Step 3: Add the guard to `src/routing.rs`**

```rust
use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::AuthedOwner;
use crate::db;

/// Resolves a *personal* key's namespace. Only a personal
/// (`owner_type == "user"`) key resolves this way without a permission
/// check — a workspace/org-owned key must go through
/// `roles::require_permission` via the `X-Org-Id` path instead (see
/// `proxy::proxy_handler`). The schema permits `owner_type` values this
/// function doesn't handle (`'workspace'`, `'org'`); reject them explicitly
/// rather than resolving them, so introducing such a key later forces a
/// conscious decision about how it should route instead of silently
/// bypassing the role/permission system.
pub async fn resolve_namespace(pool: &PgPool, owner: &AuthedOwner) -> Result<String, StatusCode> {
    if owner.owner_type != "user" {
        return Err(StatusCode::FORBIDDEN);
    }
    match db::find_database_mapping(pool, &owner.owner_type, owner.owner_id).await {
        Ok(Some(namespace)) => Ok(namespace),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("database mapping lookup failed: {e:#}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Resolves an org's shared namespace directly by `org_id`, bypassing the
/// authenticated owner entirely — used only after `roles::require_permission`
/// has already confirmed the caller is a member of this org with the right
/// permission (see `proxy::proxy_handler`).
pub async fn resolve_org_namespace(pool: &PgPool, org_id: Uuid) -> Result<String, StatusCode> {
    match db::find_database_mapping(pool, "org", org_id).await {
        Ok(Some(namespace)) => Ok(namespace),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("database mapping lookup failed: {e:#}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
```

(Only `resolve_namespace` changes — `resolve_org_namespace` stays identical.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --test routing_test -- --nocapture`
Expected: all tests in this file pass, including `resolves_namespace_for_mapped_owner` and `returns_404_when_owner_has_no_mapping` (both use `owner_type = "user"`, unaffected by the new guard).

- [ ] **Step 5: Write the failing test for the namespace-selector fail-closed assertion**

Append to `tests/proxy_test.rs` — this test needs to observe what would happen if `x-namespace-bin` were ever missing from the outbound request, which means testing the *assertion itself* rather than trying to actually trigger a real missing-header bug (the existing code always sets it correctly). Add a unit test in `src/proxy.rs`'s own `#[cfg(test)] mod tests` block instead, which can construct the scenario directly without needing a full end-to-end request:

In `src/proxy.rs`, inside `mod tests`, add:

```rust
    #[test]
    fn missing_namespace_selector_is_detected() {
        let headers = HeaderMap::new(); // x-namespace-bin deliberately absent
        assert!(!headers.contains_key(&X_NAMESPACE_BIN));
        // The real assertion this proves the shape of lives inline in
        // proxy_handler (see assert_namespace_selector_present below) — this
        // unit test exists so the helper function has direct coverage
        // independent of a full proxied request.
        assert!(!assert_namespace_selector_present(&headers, "some-namespace"));
    }

    #[test]
    fn correct_namespace_selector_passes() {
        let mut headers = HeaderMap::new();
        let namespace = "probens";
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(namespace);
        headers.insert(
            X_NAMESPACE_BIN,
            HeaderValue::from_str(&encoded).unwrap(),
        );
        assert!(assert_namespace_selector_present(&headers, namespace));
    }
```

- [ ] **Step 6: Run the tests to verify they fail**

Run: `cargo test --test proxy_test` (this runs the crate's unit tests too, including `src/proxy.rs`'s `mod tests`)
Expected: FAIL — `assert_namespace_selector_present` doesn't exist yet.

- [ ] **Step 7: Add the fail-closed assertion to `src/proxy.rs`**

Add a helper function near `forwardable_headers`:

```rust
/// Returns `true` only if `headers` carries `x-namespace-bin` set to exactly
/// the expected unpadded-base64 encoding of `namespace`. The whole namespace
/// isolation model rests on this header being present and correct on every
/// outbound request — if it were ever dropped (a future refactor bug, a
/// header-size limit, an early-return regression), sqld's authority-based
/// fallback resolves to a shared `default` namespace rather than erroring,
/// which is a silent cross-tenant leak, not a rejected request. Called
/// immediately before the outbound request is sent, so a failure here turns
/// that hypothetical silent leak into a loud, immediate 500 instead.
fn assert_namespace_selector_present(headers: &HeaderMap, namespace: &str) -> bool {
    let expected = base64::engine::general_purpose::STANDARD_NO_PAD.encode(namespace);
    headers
        .get(&X_NAMESPACE_BIN)
        .and_then(|v| v.to_str().ok())
        == Some(expected.as_str())
}
```

In `proxy_handler`, immediately before the line `let upstream = match tokio::time::timeout(...)`, add:

```rust
    if !assert_namespace_selector_present(&headers, &namespace) {
        tracing::error!(
            "x-namespace-bin missing or incorrect on outbound request for namespace \
             {namespace:?} — refusing to proxy rather than risk falling back to a shared \
             default namespace"
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --test proxy_test`
Expected: all pass, including the two new unit tests and every existing proxy test (which all set `x-namespace-bin` correctly via the normal code path and so never trip the new assertion).

- [ ] **Step 9: Add a deployment note about sqld's `default` namespace**

In `README.md`, find the deployment/quickstart section and add:

```markdown
**Production deployment note:** sqld falls back to an implicit `default`
namespace for any request whose namespace selector doesn't resolve — see
`src/proxy.rs`'s `assert_namespace_selector_present` for the gateway-side
guard against ever sending such a request. As defense in depth, check
whether the pinned libsql-server version supports disabling the implicit
default namespace entirely (a startup flag), or pre-create and reserve
`default` as a sentinel `database_mappings` row no real tenant can ever be
assigned (`sqld_namespace` is `UNIQUE`).
```

- [ ] **Step 10: Run the full test suite**

Run: `cargo test`
Expected: all binaries pass.

- [ ] **Step 11: Commit**

```bash
git add src/routing.rs src/proxy.rs tests/routing_test.rs tests/proxy_test.rs README.md
git commit -m "fix: defense-in-depth for namespace isolation (owner-type guard, fail-closed selector check)"
```

---

### Task 8: Last-admin lockout guard

**Files:**
- Modify: `src/roles.rs`
- Modify: `src/org/admin.rs`
- Modify: `src/org/members.rs`
- Modify: `tests/org_admin_test.rs`
- Modify: `tests/org_members_test.rs`

**Interfaces:**
- Produces: `pub async fn roles::would_leave_org_without_manage_roles(pool: &PgPool, org_id: Uuid, excluding_user_id: Option<Uuid>) -> Result<bool, sqlx::Error>` — used by `update_role`, `remove_member`, and `assign_member_role`.

- [ ] **Step 1: Write the failing tests**

Append to `tests/org_admin_test.rs`:

```rust
/// The bootstrap `owner` role holder cannot strip `org:manage_roles` from
/// their own role via `update_role` if they're the org's only holder of it —
/// doing so would permanently lock the org out of managing its own roles,
/// with no recovery path over the API.
#[tokio::test]
async fn update_role_refuses_to_strip_the_last_manage_roles_holder() {
    let pool = test_pool().await;
    let (org_id, key) = seed_org_member(
        &pool,
        &[Permission::OrgManageRoles, Permission::OrgManageMembers],
    )
    .await;
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let app = hivewarden::app(test_state(pool));
    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/orgs/{org_id}/roles/{role_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"permissions":["org:manage_members"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
```

(Uses whichever `seed_org_member`-style helper and `test_state` this file already has — check its top for the existing pattern. Add a small `bootstrap_role_id(pool, org_id)` helper if one doesn't exist: `sqlx::query_as::<_, (Uuid,)>("SELECT role_id FROM org_members WHERE org_id = $1 LIMIT 1").bind(org_id).fetch_one(pool).await.unwrap().0`.)

Append to `tests/org_members_test.rs`:

```rust
/// `remove_member` must refuse to remove the org's last `org:manage_roles`
/// holder — same reasoning as `update_role_refuses_to_strip_the_last_manage_roles_holder`
/// in `tests/org_admin_test.rs`, different endpoint.
#[tokio::test]
async fn remove_member_refuses_to_remove_the_last_manage_roles_holder() {
    let pool = test_pool().await;
    let (org_id, user_id, key) = seed_org_member_full(
        &pool,
        &[Permission::OrgManageRoles, Permission::OrgManageMembers],
    )
    .await;

    let app = hivewarden::app(test_state(pool));
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/orgs/{org_id}/members/{user_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
```

(Uses whichever `seed_org_member_full`-style helper this file already has, matching the pattern in `tests/org_proxy_test.rs`'s helper of the same name if this file's is different.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test org_admin_test update_role_refuses_to_strip_the_last_manage_roles_holder -- --nocapture` and `cargo test --test org_members_test remove_member_refuses_to_remove_the_last_manage_roles_holder -- --nocapture`
Expected: both FAIL — currently both operations succeed (`204`), since nothing checks for this.

- [ ] **Step 3: Add the check to `src/roles.rs`**

Add near `require_permission`:

```rust
/// True if removing `excluding_user_id` (or, when `None`, evaluating the
/// org's *current* membership as-is) would leave zero members holding
/// `org:manage_roles` in this org. Shared by `update_role` (simulating the
/// role's *new* permission set — see its call site, which passes the
/// simulated state a different way, see below), `remove_member`, and
/// `assign_member_role`.
///
/// This checks role-holder *membership*, not the permission set of a role
/// being edited — `update_role` needs a different shape (see
/// `org::admin::update_role`'s own inline check) since it's asking "if this
/// role's permissions changed to X, would anyone still hold
/// org:manage_roles," not "if this member left, would anyone."
pub async fn would_leave_org_without_manage_roles(
    pool: &PgPool,
    org_id: Uuid,
    excluding_user_id: Option<Uuid>,
) -> Result<bool, sqlx::Error> {
    let (remaining,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM org_members om
         JOIN role_permissions rp ON rp.role_id = om.role_id
         WHERE om.org_id = $1
           AND rp.permission = 'org:manage_roles'
           AND ($2::uuid IS NULL OR om.user_id != $2)",
    )
    .bind(org_id)
    .bind(excluding_user_id)
    .fetch_one(pool)
    .await?;
    Ok(remaining == 0)
}
```

- [ ] **Step 4: Wire the check into `remove_member` (`src/org/members.rs`)**

```rust
pub async fn remove_member(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }

    match roles::would_leave_org_without_manage_roles(&state.pool, org_id, Some(user_id)).await {
        Ok(true) => {
            return (
                StatusCode::CONFLICT,
                "would leave the org with no member holding org:manage_roles",
            )
                .into_response();
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!("last-admin check failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let result = sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
        .bind(org_id)
        .bind(user_id)
        .execute(&state.pool)
        .await;
    match result {
        Ok(res) if res.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("remove member failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

(Only the new block between the permission check and the existing `DELETE` is added.)

- [ ] **Step 5: Wire the check into `update_role` (`src/org/admin.rs`)**

`update_role` needs a different check shape than `remove_member`'s: it's not asking "if this member left," it's asking "if this role's permission set changed to the new one, would the org still have someone holding `org:manage_roles`." Add this check between parsing the new permissions and calling `roles::set_role_permissions`:

```rust
pub async fn update_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, role_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateRoleRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageRoles).await
    {
        return status.into_response();
    }
    let permissions = match parse_permissions(&req.permissions) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if !permissions.contains(&Permission::OrgManageRoles) {
        // This role is losing org:manage_roles (or never had it) — check
        // whether any *other* role in the org still grants it to someone.
        // Distinct from `roles::would_leave_org_without_manage_roles`'s
        // membership-exclusion shape: here the role itself stays assigned to
        // its current holders, only its permission set changes, so the
        // check is "does any role other than this one, held by anyone,
        // still carry org:manage_roles" rather than "excluding one member."
        let (other_holders,): (i64,) = match sqlx::query_as(
            "SELECT COUNT(*) FROM org_members om
             JOIN role_permissions rp ON rp.role_id = om.role_id
             WHERE om.org_id = $1
               AND rp.permission = 'org:manage_roles'
               AND om.role_id != $2",
        )
        .bind(org_id)
        .bind(role_id)
        .fetch_one(&state.pool)
        .await
        {
            Ok(row) => row,
            Err(e) => {
                tracing::error!("last-admin check failed: {e:#}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        if other_holders == 0 {
            return (
                StatusCode::CONFLICT,
                "would leave the org with no member holding org:manage_roles",
            )
                .into_response();
        }
    }

    // `set_role_permissions` is org-scoped and does its own existence check
    // in the same transaction as the rewrite, so a nonexistent or wrong-org
    // `role_id` is `NotFound` here rather than a silent 204.
    match roles::set_role_permissions(&state.pool, org_id, role_id, &permissions).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(roles::SetRolePermissionsError::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(roles::SetRolePermissionsError::Db(e)) if is_unique_violation(&e) => {
            (StatusCode::CONFLICT, "role name already exists in this org").into_response()
        }
        Err(roles::SetRolePermissionsError::Db(e)) => {
            tracing::error!("update role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 6: Wire the check into `assign_member_role` (`src/org/admin.rs`)**

`assign_member_role` can also strip a member's `org:manage_roles` by reassigning them to a lesser role. Add the same membership-exclusion check `remove_member` uses, since reassignment is conceptually "this member's current role-derived permission no longer applies, does anyone else still have it":

```rust
pub async fn assign_member_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<AssignRoleRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }

    match roles::would_leave_org_without_manage_roles(&state.pool, org_id, Some(user_id)).await {
        Ok(true) => {
            return (
                StatusCode::CONFLICT,
                "would leave the org with no member holding org:manage_roles",
            )
                .into_response();
        }
        Ok(false) => {}
        Err(e) => {
            tracing::error!("last-admin check failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    match roles::assign_member_role(&state.pool, org_id, user_id, req.role_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("assign member role failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

**Note this check has a real limitation worth being aware of, not fixing here:** `would_leave_org_without_manage_roles(pool, org_id, Some(user_id))` excludes the member entirely, but a reassignment doesn't remove them from the org — it moves them to a *different* role. If that new role happens to also grant `org:manage_roles`, this check would still (correctly, just for a different reason than it computed) find the org non-empty of holders, since the check only cares about the post-change count being nonzero, not which role gets there. This is fine: the check's actual invariant ("will someone still hold `org:manage_roles` after this change") holds either way. No further action needed — noted so a future reader doesn't mistake this for a bug.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --test org_admin_test` and `cargo test --test org_members_test`
Expected: all pass, including the two new tests and every pre-existing test in both files (in particular `assign_member_role_updates_an_existing_members_role`, which must still succeed for a reassignment that doesn't strip the last holder).

- [ ] **Step 8: Run the full test suite**

Run: `cargo test`
Expected: all binaries pass — `a_role_created_and_assigned_over_http_authorizes_the_proxy` in `tests/org_proxy_test.rs` reassigns a member's role and must still work (it reassigns *to* a role with `db:query`, and the bootstrap role that member is leaving isn't their last `org:manage_roles` source unless it's their only role — confirm this test's seeded permissions still leave at least one `org:manage_roles` holder after the reassignment; if not, that test's setup needs a second seeded member so the check doesn't block it, since the fix's job is to prevent *actual* lockouts, not to break a legitimate reassignment in a single-member test org).

- [ ] **Step 9: Commit**

```bash
git add src/roles.rs src/org/admin.rs src/org/members.rs tests/org_admin_test.rs tests/org_members_test.rs
git commit -m "fix: guard against an org locking itself out of org:manage_roles"
```

---

### Task 9: DB round-trip reduction

**Files:**
- Modify: `src/roles.rs`
- Modify: `src/db.rs`
- Modify: `tests/roles_test.rs`

**Interfaces:**
- Produces: `pub async fn roles::member_permissions(...)` keeps its existing signature and `Option<HashSet<Permission>>` contract — this task changes its *implementation* (one query instead of two), not its interface, so nothing downstream needs to change.

- [ ] **Step 1: Run the existing test suite as a baseline**

Run: `cargo test --test roles_test`
Expected: passes — this establishes the behavior this task must not change.

- [ ] **Step 2: Collapse the two-query lookup in `src/roles.rs`**

Replace `member_permissions`:

```rust
/// Looks up an org member's effective permissions via their assigned role,
/// in one query (a member→role→permissions `LEFT JOIN`, not two round
/// trips) — this sits on the hot path of every proxied request and every
/// admin endpoint, via `require_permission`.
///
/// `Ok(None)` means the user is not a member of this org at all — distinct
/// from `Ok(Some(empty set))`, which means a member holds a role with zero
/// permissions attached (allowed, just useless). The `LEFT JOIN` makes the
/// distinction: no rows at all means not a member; one or more rows with
/// `permission IS NULL` means a member whose role has no permissions
/// attached.
pub async fn member_permissions(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<Option<HashSet<Permission>>, sqlx::Error> {
    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT rp.permission FROM org_members om
         LEFT JOIN role_permissions rp ON rp.role_id = om.role_id
         WHERE om.org_id = $1 AND om.user_id = $2",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        rows.into_iter()
            .filter_map(|(p,)| p)
            .filter_map(|p| Permission::from_db_str(&p))
            .collect(),
    ))
}
```

- [ ] **Step 3: Run the test to verify behavior is preserved**

Run: `cargo test --test roles_test`
Expected: all tests pass unchanged, including `member_permissions_returns_none_for_a_non_member` and `member_permissions_returns_the_roles_permissions` — this task changes only the query shape, not the observable contract, so no test file edits should be needed. If any test fails, the query rewrite has a bug — do not weaken the test to match, fix the query.

- [ ] **Step 4: Configure explicit pool sizing in `src/db.rs`**

Change `connect`:

```rust
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(32)
        .acquire_timeout(Duration::from_secs(3))
        .connect(database_url)
        .await?;
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
```

(`max_connections(32)`/`acquire_timeout(3s)` replace sqlx's bare defaults of 10 connections and a 30-second acquire timeout — under saturation, callers now get a fast, clear failure instead of a long hang. These are reasonable starting values for this project's current scale, not load-tested numbers; revisit once the load-test/DR-validation sub-project runs.)

- [ ] **Step 5: Run the full test suite**

Run: `cargo test`
Expected: all binaries pass — every test's `db::connect` call now goes through `PgPoolOptions` instead of the bare `PgPool::connect`, but with the same `database_url`, so behavior should be unaffected except for pool sizing.

- [ ] **Step 6: Commit**

```bash
git add src/roles.rs src/db.rs
git commit -m "perf: collapse permission lookup to one query, configure explicit pool sizing"
```

---

### Task 10: Constant-time metrics token comparison

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/observability.rs`
- Modify: `tests/observability_test.rs`

**Interfaces:**
- Produces: nothing new consumed by other tasks — this is the plan's last task.

- [ ] **Step 1: Add `subtle`**

Run:
```sh
cargo add subtle
```

- [ ] **Step 2: Write the failing test**

Append to `tests/observability_test.rs`:

```rust
#[tokio::test]
async fn metrics_endpoint_rejects_a_token_of_different_length() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle)
        .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = hivewarden::app(state);

    // Neither a prefix nor a suffix of the real token — proves the check
    // isn't accidentally doing a substring/prefix match, just confirms
    // rejection. The actual constant-time property isn't something a
    // functional test can observe directly; this test exists to lock in
    // that changing the comparison mechanism doesn't change the pass/fail
    // outcome for a clearly-wrong token.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}x", common::TEST_METRICS_TOKEN),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
```

(This test should already pass against the current `!=`-based check — it's here as a regression lock, not to prove a bug. The actual fix is a refactor with no externally-observable behavior change other than timing, which no functional test can assert on; see Step 3's code comment for why this is still worth fixing.)

- [ ] **Step 3: Run the test to confirm baseline behavior**

Run: `cargo test --test observability_test metrics_endpoint_rejects_a_token_of_different_length -- --nocapture`
Expected: PASS (already, against the current code — this step just confirms the test itself is correct before the refactor).

- [ ] **Step 4: Make the comparison constant-time in `src/observability.rs`**

Add the import:

```rust
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
```

Replace `metrics_handler`'s token check:

```rust
pub async fn metrics_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // Compares SHA-256 digests rather than the raw strings: `ConstantTimeEq`
    // on unequal-length byte slices isn't meaningfully constant-time (the
    // length mismatch itself is an early, visible signal), and hashing both
    // sides to a fixed 32 bytes first removes that. `state.metrics_token`
    // empty still fails closed via the explicit `is_empty()` check — a
    // digest of an empty string would otherwise be a normal-looking 32-byte
    // value that could theoretically match some presented token's digest by
    // construction, and fail-closed should never depend on ct_eq alone.
    let presented_digest = Sha256::digest(token.as_bytes());
    let expected_digest = Sha256::digest(state.metrics_token.as_bytes());
    if state.metrics_token.is_empty()
        || !bool::from(presented_digest.ct_eq(&expected_digest))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    refresh_pg_pool_gauges(&state.pool);
    let body = state.metrics_handle.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}
```

- [ ] **Step 5: Run the tests to verify they still pass**

Run: `cargo test --test observability_test`
Expected: all tests in this file pass, including the new one and every pre-existing `/metrics`-touching test (`metrics_endpoint_requires_valid_bearer_token`, the fail-closed test from the observability sub-project's own hardening, etc.).

- [ ] **Step 6: Run the full test suite**

Run: `cargo test`
Expected: all binaries pass.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/observability.rs tests/observability_test.rs
git commit -m "fix: constant-time comparison for the /metrics bearer token"
```
