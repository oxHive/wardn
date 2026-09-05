# Usage Metering Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emit one structured `tracing` usage event per proxied request that actually reaches sqld, in JSON, so a future billing system has the raw per-tenant data it needs.

**Architecture:** Switch the whole process's logging to JSON output (one-line `tracing_subscriber` change), then emit a `target: "usage"` event from `proxy_handler` right before returning its response — but only on the path where a response actually came back from sqld. A shared test helper captures these events structurally (not by parsing log text) for assertions.

**Tech Stack:** `tracing`/`tracing-subscriber` (already dependencies) — no new external dependencies.

## Global Constraints

- Rust edition 2024, axum 0.8, sqlx 0.8 Postgres. Runtime-checked queries only where this plan touches SQL (it mostly doesn't — no new tables, no new queries).
- **Data-plane proxy traffic only.** `proxy_handler` (`src/proxy.rs`) is the only emission point. No control-plane (admin API) operation is metered by this plan.
- **Only requests that reach sqld are events.** Any early `return` in `proxy_handler` (auth/permission/namespace-resolution rejection, malformed headers) must never emit a usage event.
- **Counts only, no byte counting.** The event describes what kind of request happened and how long it took to get a response, not payload size.
- **Fields:** `owner_type`, `owner_id`, `org_id` (empty string when `X-Org-Id` wasn't used), `namespace`, `protocol` (`"query"` or `"sync"`, mirroring the existing `db:query`/`db:sync` split), `status`, `duration_ms`.
- **All test seed values (UUIDs, emails, namespace names) must derive from a fresh `Uuid::new_v4()` per test run** — hardcoded literals collide with `UNIQUE` constraints on repeat runs against the persistent dev Postgres container.
- No new migration, no new Postgres table.

---

### Task 1: Switch logging to JSON output

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks (this is the foundation).
- Produces: nothing new tasks consume by name — this is a process-wide format change that Task 2's usage events (and every existing `tracing::error!`/`info!` call) now emit as JSON.

- [ ] **Step 1: Add the `json` feature to `tracing-subscriber`**

In `Cargo.toml`, change:

```toml
tracing-subscriber = "0.3"
```

to:

```toml
tracing-subscriber = { version = "0.3", features = ["json"] }
```

(Leave every other line in `Cargo.toml` unchanged — this only adds a feature flag to an existing dependency, no new crate.)

- [ ] **Step 2: Switch the subscriber to JSON**

In `src/main.rs`, change:

```rust
    tracing_subscriber::fmt::init();
```

to:

```rust
    tracing_subscriber::fmt().json().init();
```

Leave every other line in `src/main.rs` unchanged.

- [ ] **Step 3: Verify**

Run: `cargo build`
Expected: succeeds, no errors.

Run: `cargo test`
Expected: the full existing suite still passes bare — no test asserts on the exact text shape of any log line today, so this format change doesn't break anything.

Run (from the repo root, with the dev Postgres/sqld up via `podman-compose up -d`):
```sh
SQLD_ADMIN_URL=http://127.0.0.1:8090 cargo run
```
Expected: the startup log line (`wardn listening on ...`) now prints as a single JSON object per line (e.g. `{"timestamp":"...","level":"INFO","fields":{"message":"wardn listening on 127.0.0.1:8787"},"target":"wardn"}`), not the previous human-readable format. Ctrl-C to stop.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs
git commit -m "feat: switch logging to JSON output"
```

---

### Task 2: Emit usage events from the proxy

**Files:**
- Modify: `src/proxy.rs`
- Modify: `tests/common/mod.rs`
- Test: `tests/usage_metering_test.rs` (new)

**Interfaces:**
- Consumes: `tracing_subscriber = { version = "0.3", features = ["json"] }` from Task 1 (the `registry`/`Layer` machinery this task's test helper uses is a default feature of `tracing-subscriber`, already available regardless of the `json` feature — Task 1 doesn't gate it, this note is just for context).
- Produces: `pub async fn capture_usage_events<F, Fut, T>(f: F) -> (T, Vec<HashMap<String, String>>)` in `tests/common/mod.rs` — no later task in this plan consumes it beyond this task's own test file, but it's `pub` for any future test file that needs the same capture pattern.

- [ ] **Step 1: Write the failing tests**

Create `tests/usage_metering_test.rs`:

```rust
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use wardn::auth::AppState;
use wardn::db;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tower::ServiceExt;
use uuid::Uuid;

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

async fn create_namespace(name: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/namespaces/{name}/create", admin_url()))
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .expect("admin API create-namespace request failed");
    assert!(resp.status().is_success(), "failed to create {name}");
}

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

async fn register(app: axum::Router, email: &str) -> (Uuid, String) {
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
    let user_id: Uuid = created["user_id"].as_str().unwrap().parse().unwrap();
    let api_key = created["api_key"].as_str().unwrap().to_string();
    (user_id, api_key)
}

async fn create_org(app: axum::Router, owner_key: &str, name: &str) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    created["org_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn successful_query_request_emits_a_usage_event() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("usage-{}@example.com", Uuid::new_v4())).await;

    let (resp, events) = common::capture_usage_events(|| {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
    })
    .await;
    let resp = resp.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        events.len(),
        1,
        "expected exactly one usage event: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["owner_type"], "user");
    assert_eq!(event["owner_id"], user_id.to_string());
    assert_eq!(event["org_id"], "");
    assert_eq!(event["namespace"], user_id.to_string());
    assert_eq!(event["protocol"], "query");
    assert_eq!(event["status"], "200");
    assert!(
        event.contains_key("duration_ms"),
        "expected a duration_ms field: {event:?}"
    );

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn rejected_request_does_not_emit_a_usage_event() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (resp, events) = common::capture_usage_events(|| {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
    })
    .await;
    let resp = resp.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        events.is_empty(),
        "a rejected request must not emit a usage event: {events:?}"
    );
}

#[tokio::test]
async fn org_shared_request_emits_org_id_field() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", Uuid::new_v4())).await;
    let org_id = create_org(app.clone(), &owner_key, &format!("Org-{}", Uuid::new_v4())).await;

    let (resp, events) = common::capture_usage_events(|| {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
    })
    .await;
    let resp = resp.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(events.len(), 1, "expected exactly one usage event: {events:?}");
    let event = &events[0];
    assert_eq!(event["org_id"], org_id);
    assert_eq!(event["namespace"], org_id);
    assert_eq!(event["protocol"], "query");

    delete_namespace(&org_id).await;
}

const GRPC_METHOD: &str = "/wal_log.ReplicationLog/Hello";
const EMPTY_GRPC_MESSAGE: [u8; 5] = [0, 0, 0, 0, 0];

/// Boots the real gateway on an ephemeral port with `axum::serve`, so the
/// server side of the h2c connection is genuinely exercised (`oneshot`
/// never touches the wire).
async fn spawn_gateway(pool: sqlx::PgPool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = wardn::app(
        AppState::new(pool, test_sqld_url()).with_sqld_admin_url(admin_url()),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Sends a minimal h2c gRPC call and returns its HTTP status.
async fn grpc_call(base: &str, api_key: &str) -> StatusCode {
    let mut builder = Client::builder(TokioExecutor::new());
    builder.http2_only(true);
    let client: Client<HttpConnector, Full<Bytes>> = builder.build(HttpConnector::new());

    let request = hyper::Request::builder()
        .method("POST")
        .uri(format!("{base}{GRPC_METHOD}"))
        .header(header::CONTENT_TYPE, "application/grpc")
        .header("te", "trailers")
        .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
        .body(Full::new(Bytes::from_static(&EMPTY_GRPC_MESSAGE)))
        .unwrap();

    let response = client.request(request).await.expect("gRPC request failed");
    let status = response.status();
    let _ = response.into_body().collect().await;
    status
}

#[tokio::test]
async fn sync_request_emits_protocol_sync() {
    let pool = test_pool().await;
    let namespace = format!("usagesync-{}", Uuid::new_v4());
    create_namespace(&namespace).await;

    let owner_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("usagesync-{owner_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
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
    let (full_key, prefix, hash) = wardn::auth::generate_api_key();
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

    let (status, events) = common::capture_usage_events(|| async move {
        let gateway = spawn_gateway(pool.clone()).await;
        grpc_call(&gateway, &full_key).await
    })
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        events.len(),
        1,
        "expected exactly one usage event: {events:?}"
    );
    assert_eq!(events[0]["protocol"], "sync");
    assert_eq!(events[0]["namespace"], namespace);

    delete_namespace(&namespace).await;
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test usage_metering_test`
Expected: compile failure — `common::capture_usage_events` doesn't exist yet.

- [ ] **Step 3: Add the capture helper to `tests/common/mod.rs`**

Replace the full contents of `tests/common/mod.rs`'s import block (the first non-comment lines) — i.e., change:

```rust
use sqlx::{PgPool, Postgres, Transaction};
```

to:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sqlx::{PgPool, Postgres, Transaction};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
```

Then append to the end of the file (after `delete_outbox_row`):

```rust
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
```

- [ ] **Step 4: Emit the usage event in `src/proxy.rs`**

Change the top-of-file import:

```rust
use std::time::Duration;
```

to:

```rust
use std::time::{Duration, Instant};
```

Replace the full body of `proxy_handler` (from `pub async fn proxy_handler(` through its closing `}`) with:

```rust
pub async fn proxy_handler(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    request: Request,
) -> Response {
    let start = Instant::now();
    let (parts, body) = request.into_parts();

    let mut org_id_for_usage: Option<Uuid> = None;
    let namespace = match org_id_header(&parts.headers) {
        Some(Ok(org_id)) => {
            org_id_for_usage = Some(org_id);
            // Mirrors the HTTP/2-in-means-h2c-out fork `ProxyClient::for_version`
            // makes below: db:sync gates the gRPC replication leg, db:query
            // gates everything else (Hrana/HTTP1.1).
            //
            // `require_permission` takes the whole `AuthedOwner` and rejects a
            // non-`user` owner_type itself — a workspace/org-owned key's
            // owner_id names a workspace/org, not a user, and must never be
            // looked up as an `org_members.user_id`.
            let required = if parts.version == Version::HTTP_2 {
                Permission::DbSync
            } else {
                Permission::DbQuery
            };
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
        Some(Err(())) => return StatusCode::BAD_REQUEST.into_response(),
        None => match routing::resolve_namespace(&state.pool, &owner).await {
            Ok(ns) => ns,
            Err(status) => return status.into_response(),
        },
    };

    let target = format!(
        "{}{}",
        state.sqld_url.trim_end_matches('/'),
        parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
    );
    let target: Uri = match target.parse() {
        Ok(uri) => uri,
        Err(e) => {
            tracing::error!("could not build sqld target URI from {target:?}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Strip the client's credentials (they authenticate to *us*, not to sqld)
    // and both namespace selectors, then set the selectors ourselves from the
    // namespace Postgres resolved for this owner. Never `unwrap` on the
    // header values: a malformed `sqld_namespace` row must produce a 500 for
    // this one request, not panic the worker and kill the connection.
    // `migrations/0002_namespace_format_constraint.sql` makes such a row
    // impossible to insert in the first place; this is the belt to its braces.
    let mut headers = forwardable_headers(
        &parts.headers,
        &[header::AUTHORIZATION, header::HOST, X_NAMESPACE_BIN, X_ORG_ID],
    );
    let namespace_bin = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&namespace);
    let Ok(namespace_bin_value) = HeaderValue::from_str(&namespace_bin) else {
        tracing::error!(
            "sqld_namespace {namespace:?} cannot be encoded into an x-namespace-bin \
             header; refusing to proxy"
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    headers.insert(X_NAMESPACE_BIN, namespace_bin_value);

    let (client, outbound_version) = state.client.for_version(parts.version);

    // `Host` only means anything on the Hrana/HTTP endpoints, which are
    // HTTP/1.1 only in practice — and RFC 9113 §8.3.1 treats an HTTP/2
    // request carrying both `:authority` (derived from `target` below) and a
    // `Host` with a different value as malformed. sqld/hyper tolerate the
    // mismatch today, but a stricter intermediary in front of sqld later
    // would not, so it's simplest to just never send it on the h2 leg.
    if outbound_version == Version::HTTP_11 {
        let namespace_host = format!("{namespace}.local");
        let Ok(host_value) = HeaderValue::from_str(&namespace_host) else {
            tracing::error!(
                "sqld_namespace {namespace:?} cannot be encoded into a Host header; \
                 refusing to proxy"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        headers.insert(header::HOST, host_value);
    }

    // RFC 9113 §8.2.2 permits `te: trailers` on HTTP/2 even though `te` is
    // otherwise hop-by-hop — some gRPC servers (grpc-go) reject a request
    // that omits it. `forwardable_headers` already stripped it above; put it
    // back, but only when the client actually sent it and we're speaking h2
    // outbound (sqld's own gRPC service doesn't check for it, but another
    // gRPC upstream might).
    if outbound_version == Version::HTTP_2
        && let Some(te) = parts.headers.get(header::TE)
        && te.as_bytes().eq_ignore_ascii_case(b"trailers")
    {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }

    // The body is moved through untouched — no `to_bytes` buffering in either
    // direction, so an arbitrarily large upload or a long-lived streaming
    // gRPC response costs the gateway a constant amount of memory.
    let mut outbound = hyper::Request::builder()
        .method(parts.method)
        .uri(target)
        .version(outbound_version);
    match outbound.headers_mut() {
        Some(slot) => *slot = headers,
        None => {
            tracing::error!("could not build outbound request to sqld");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let outbound = match outbound.body(body) {
        Ok(req) => req,
        Err(e) => {
            tracing::error!("could not build outbound request to sqld: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let upstream = match tokio::time::timeout(RESPONSE_HEAD_TIMEOUT, client.request(outbound)).await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            tracing::error!("proxy request to sqld failed: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
        Err(_) => {
            tracing::error!(
                "proxy request to sqld timed out after {RESPONSE_HEAD_TIMEOUT:?} \
                 waiting for a response"
            );
            return StatusCode::GATEWAY_TIMEOUT.into_response();
        }
    };

    let (upstream_parts, upstream_body) = upstream.into_parts();
    // `content-length`/`transfer-encoding` describe upstream's framing of the
    // body on *its* connection. We re-frame it on ours, so hyper recomputes
    // them; copying upstream's across desynchronises the response.
    let response_headers = forwardable_headers(
        &upstream_parts.headers,
        &[header::CONTENT_LENGTH, header::TRANSFER_ENCODING],
    );

    // One usage event per request that actually reached sqld — see
    // docs/superpowers/specs/2026-08-12-usage-metering-design.md. Emitted
    // here, not earlier: every early `return` above is a request that never
    // consumed database resources (auth/permission/namespace-resolution
    // rejections, malformed headers), and isn't a billable event.
    let protocol = if outbound_version == Version::HTTP_2 {
        "sync"
    } else {
        "query"
    };
    let org_id_field = org_id_for_usage
        .map(|id| id.to_string())
        .unwrap_or_default();
    tracing::info!(
        target: "usage",
        owner_type = %owner.owner_type,
        owner_id = %owner.owner_id,
        org_id = %org_id_field,
        namespace = %namespace,
        protocol,
        status = upstream_parts.status.as_u16() as u64,
        duration_ms = start.elapsed().as_millis() as u64,
        "usage event"
    );

    // `Body::new` keeps the upstream body as a stream *and* passes its trailer
    // frame through — gRPC carries its `grpc-status`/`grpc-message` in HTTP/2
    // trailers on a successful call, so dropping them would turn every synced
    // RPC into a hang or a protocol error.
    let mut response = Response::new(Body::new(upstream_body));
    *response.status_mut() = upstream_parts.status;
    *response.headers_mut() = response_headers;
    response
}
```

(The `#[cfg(test)] mod tests { ... }` block at the end of `src/proxy.rs` is unchanged — leave it exactly as it is.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test usage_metering_test`
Expected: all 4 tests PASS.

Run: `cargo test`
Expected: full suite still passes — in particular `tests/proxy_test.rs`, `tests/grpc_proxy_test.rs`, `tests/org_proxy_test.rs` (the existing proxy-path tests) must still pass unchanged, proving the usage-event addition didn't alter any response behavior.

- [ ] **Step 6: Commit**

```bash
git add src/proxy.rs tests/common/mod.rs tests/usage_metering_test.rs
git commit -m "feat: emit structured usage events from the proxy"
```

---

## Self-Review Notes

- **Spec coverage:** JSON logging switch (Task 1) — usage event emitted only for requests that reach sqld, never on early-return rejections (Task 2, tested explicitly by `rejected_request_does_not_emit_a_usage_event`) — all 7 specified fields present (Task 2's emission code and `successful_query_request_emits_a_usage_event`'s assertions) — `protocol` distinguishing query/sync (Task 2, two dedicated tests) — `org_id` present/absent correctly (Task 2, two dedicated tests) — counts only, no byte counting (confirmed: no body-wrapping code anywhere in this plan) — data-plane only, no control-plane metering (confirmed: the only `tracing::info!(target: "usage", ...)` call in this plan is in `proxy_handler`; `src/registration.rs`, `src/org/admin.rs`, `src/org/members.rs`, `src/api_keys.rs` are untouched). No gaps found.
- **Placeholder scan:** none found — every step has complete, runnable code.
- **Type consistency:** `capture_usage_events`'s signature (`tests/common/mod.rs`, Task 2) is used identically across all 4 new tests in `tests/usage_metering_test.rs`. The usage-event field names (`owner_type`, `owner_id`, `org_id`, `namespace`, `protocol`, `status`, `duration_ms`) match exactly between the `tracing::info!` call in `src/proxy.rs` and every test assertion against `event["..."]`.
