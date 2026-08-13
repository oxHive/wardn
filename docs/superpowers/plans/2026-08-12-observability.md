# hivewarden Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose a Prometheus `/metrics` endpoint covering proxy request volume/latency, dependency health (Postgres pool, sqld reachability), and the provisioning worker's queue depth — so an operator can answer "is the service healthy" before real traffic arrives — and wire up a local Grafana + Prometheus stack in `podman-compose.yml` for testing it.

**Architecture:** `metrics` (facade) + `metrics-exporter-prometheus`, recorder installed once in `main.rs`, handle threaded through `AppState` and rendered by a new `GET /metrics` route gated by a static bearer token (`METRICS_TOKEN` — added during Task 2's review as a fix for a tenant-enumeration finding; see the amendment note after Task 1). All new metric-recording logic lives in one new module, `src/observability.rs`, called into from `proxy.rs` and `provisioning.rs` at their existing instrumentation points.

**Tech Stack:** Rust/axum 0.8 (edition 2024), `metrics` + `metrics-exporter-prometheus`, sqlx 0.8 (runtime-checked queries only), existing `reqwest` client for the sqld health probe.

## Global Constraints

- Rust edition 2024, axum 0.8, sqlx 0.8 — runtime-checked queries only (`sqlx::query`/`query_as`, never the `query!`/`query_as!` compile-time macros).
- `#[tokio::test]` stays the bare, single-threaded-runtime attribute in every test file — no `flavor = "multi_thread"`. Not directly load-bearing for this slice's own tests, but must not be broken for the existing usage-metering tests that depend on it.
- Integration tests exercise real Postgres and real sqld (via `DATABASE_URL`/`SQLD_URL`/`SQLD_ADMIN_URL` env vars, defaulting to the `podman-compose.yml` dev instances) — no mocks.
- `GET /metrics` requires a valid `Authorization: Bearer <METRICS_TOKEN>` header, checked in `metrics_handler` itself (not via `auth_middleware`, which is for API keys — this is a single deployment-wide secret). Still registered outside `auth_middleware`'s route group, same as `/healthz`/`/users` (see the amendment note after Task 1 for why this superseded the plan's original "unauthenticated" decision).
- Only the `namespace` label carries per-tenant cardinality; every other metric label is a small fixed set. No cardinality cap in this slice (accepted risk, per the design spec).
- The Prometheus global recorder is installed **exactly once**, in `main.rs` — never inside `AppState::new`, `app()`, or any test helper that could run more than once per process. `AppState::new` builds its own local, non-globally-installed handle so every existing call site (production and test) keeps compiling unchanged.
- New `podman-compose.yml` services stay loopback-bound (`127.0.0.1:...`), matching every existing service in that file.
- Commit after each task's tests pass.

---

### Task 1: Metrics recorder wiring + `GET /metrics` endpoint

**Files:**
- Create: `src/observability.rs`
- Modify: `src/lib.rs` (add `pub mod observability;` and the `/metrics` route)
- Modify: `src/auth.rs` (add `AppState::metrics_handle` field + `with_metrics_handle`)
- Modify: `src/main.rs` (install the global recorder, pass the handle to `AppState`)
- Modify: `Cargo.toml` (new dependencies)
- Create: `tests/observability_test.rs`
- Modify: `tests/common/mod.rs` (add `metrics_handle()` test helper)

**Interfaces:**
- Produces: `pub async fn observability::metrics_handler(State(state): State<AppState>) -> impl IntoResponse` — later tasks do not call this directly, but Task 3 modifies its body.
- Produces: `AppState::with_metrics_handle(self, handle: metrics_exporter_prometheus::PrometheusHandle) -> Self`.
- Produces (test-only): `common::metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle` — every later task's tests that assert on metric output call this to get the process-wide test handle. **Only one test in the whole binary may call `install_recorder()`** (a second call panics); this helper is the single point that does it, via `std::sync::LazyLock`, so every test in a binary shares one installed recorder.

- [ ] **Step 1: Add the new dependencies**

Run:
```sh
cargo add metrics metrics-exporter-prometheus
```

This adds two lines under `[dependencies]` in `Cargo.toml` (exact versions are whatever `cargo add` resolves — don't hand-edit them in). No extra features needed on either crate: we render the handle manually inside our own axum route, not through `metrics-exporter-prometheus`'s optional built-in HTTP server.

- [ ] **Step 2: Run `cargo build` to confirm the new dependencies compile**

Run: `cargo build`
Expected: succeeds, no errors.

- [ ] **Step 3: Write the failing test**

Create `tests/observability_test.rs`:

```rust
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivewarden::auth::AppState;
use hivewarden::db;
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

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text_unauthenticated() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool, test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle);
    let app = hivewarden::app(state);

    // No Authorization header — proves this route sits outside auth_middleware.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert!(content_type.to_str().unwrap().starts_with("text/plain"));
}
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test --test observability_test -- --nocapture`
Expected: compile error — `AppState::with_metrics_handle`, `common::metrics_handle`, and the `/metrics` route don't exist yet. That's the expected failure this task's remaining steps fix.

- [ ] **Step 5: Add the `metrics_handle` field to `AppState`**

In `src/auth.rs`, add the import:

```rust
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
```

Change the `AppState` struct and its `new` constructor to:

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
}

impl AppState {
    pub fn new(pool: PgPool, sqld_url: String) -> Self {
        let (_recorder, metrics_handle) = PrometheusBuilder::new().build_recorder();
        Self {
            pool,
            sqld_url,
            sqld_admin_url: String::new(),
            client: ProxyClient::new(),
            metrics_handle,
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
}
```

- [ ] **Step 6: Create `src/observability.rs`**

```rust
//! Prometheus metrics for hivewarden. One global recorder for the
//! whole process, installed once in `main.rs` — see `AppState::metrics_handle`
//! for why every other call site builds a local, uninstalled handle instead.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;

use crate::auth::AppState;

/// Renders the process's Prometheus metrics as exposition-format text.
/// Unauthenticated, registered outside `auth_middleware` in `src/lib.rs`
/// alongside `/healthz`.
pub async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics_handle.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
}
```

- [ ] **Step 7: Add the test metrics-handle helper**

In `tests/common/mod.rs`, add near the top (after the existing imports):

```rust
use std::sync::LazyLock;

use metrics_exporter_prometheus::PrometheusHandle;
```

And, anywhere in the file:

```rust
static METRICS_HANDLE: LazyLock<PrometheusHandle> = LazyLock::new(|| {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("install prometheus recorder for tests")
});

/// Installs (once per test binary, via `LazyLock`) the global Prometheus
/// recorder and returns its handle. Every test in a binary that asserts on
/// metric output must go through this: `metrics::counter!`/`gauge!`/
/// `histogram!` calls anywhere in the crate route to whichever recorder is
/// globally installed for the process, and a second `install_recorder()`
/// call in the same process panics — so this is the one place that calls it.
pub fn metrics_handle() -> PrometheusHandle {
    METRICS_HANDLE.clone()
}
```

- [ ] **Step 8: Register the route in `src/lib.rs`**

Add `pub mod observability;` to the top of `src/lib.rs`, alongside the other `pub mod` lines.

Add the route next to `/healthz` — after `.layer(...)`, so it is **not** wrapped by `auth_middleware`:

```rust
        .route("/healthz", get(healthz))
        .route("/metrics", get(observability::metrics_handler))
        .route("/users", post(registration::create_user))
```

- [ ] **Step 9: Wire the recorder into `main.rs`**

In `src/main.rs`, add the import `use metrics_exporter_prometheus::PrometheusBuilder;`, then install the recorder right after connecting to Postgres and pass its handle into `AppState`:

```rust
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;

    let metrics_handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("install prometheus recorder");

    let worker_pool = pool.clone();
    let worker_admin_url = config.sqld_admin_url.clone();
    tokio::spawn(provisioning::run_worker(
        worker_pool,
        worker_admin_url,
        PROVISIONING_WORKER_INTERVAL,
    ));

    let state = AppState::new(pool, config.sqld_url.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone())
        .with_metrics_handle(metrics_handle);
```

- [ ] **Step 10: Run the test to verify it passes**

Run: `cargo test --test observability_test -- --nocapture`
Expected: `metrics_endpoint_returns_prometheus_text_unauthenticated` passes.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml Cargo.lock src/observability.rs src/lib.rs src/auth.rs src/main.rs tests/observability_test.rs tests/common/mod.rs
git commit -m "feat: add Prometheus /metrics endpoint"
```

---

**Amendment (made during Task 2's review):** Task 2's task review found that this task's unauthenticated `/metrics` route, combined with Task 2's `namespace` label, lets anyone enumerate every tenant's UUID and traffic volume, and lets anonymous `POST /users` grow the metrics recorder's per-tenant label set without bound. The human decided: gate `/metrics` behind a static bearer token rather than drop the `namespace` label or leave it unauthenticated. This changes Task 1's shipped behavior — `metrics_handler` now requires `Authorization: Bearer <METRICS_TOKEN>` — implemented as part of Task 2's fix round (touching `src/config.rs`, `src/auth.rs`, `src/main.rs`, `src/observability.rs`, and `tests/observability_test.rs`'s Task-1-authored test). See the design spec's amended "Scope decisions" section. Task 3 and Task 5 below are written to already account for this (test requests to `/metrics` include the token; the Prometheus scrape config carries it).

---

### Task 2: Proxy request metrics

**Files:**
- Modify: `src/observability.rs` (add `InFlightGuard`, `record_proxy_metrics`)
- Modify: `src/proxy.rs` (instrument `proxy_handler`)
- Modify: `tests/common/mod.rs` (add Prometheus-text parsing helpers)
- Modify: `tests/observability_test.rs`

**Interfaces:**
- Consumes: `AppState::metrics_handle` (Task 1), `common::metrics_handle()` (Task 1).
- Produces: `pub struct observability::InFlightGuard` with `InFlightGuard::new() -> Self` and a `Drop` impl.
- Produces: `pub fn observability::record_proxy_metrics(protocol: &'static str, namespace: &str, status: StatusCode, duration: Duration)`.
- Produces (test-only): `common::extract_unlabeled_metric(rendered: &str, metric_name: &str) -> f64` and `common::extract_labeled_metric(rendered: &str, metric_name: &str, must_contain: &[&str]) -> Option<f64>` and `common::has_labeled_metric(rendered: &str, metric_name: &str, must_contain: &[&str]) -> bool` — later tasks (3, 4) reuse all three.

- [ ] **Step 1: Add the parsing helpers to `tests/common/mod.rs`**

Prometheus counters/gauges are process-wide and cumulative — several tests in the same binary can observe each other's samples. These helpers let a test either look for its own uniquely-labeled sample (e.g. a `namespace="<uuid>"` value nothing else will produce) or measure a before/after delta rather than asserting an absolute value.

```rust
/// Parses a single **unlabeled** gauge/counter value out of Prometheus text
/// exposition format — a line of exactly `metric_name value`, no `{...}`
/// label block. Returns `0.0` if the metric has no samples yet in this
/// process (Prometheus text output simply omits metrics nothing has touched).
pub fn extract_unlabeled_metric(rendered: &str, metric_name: &str) -> f64 {
    for line in rendered.lines() {
        if let Some((name, value)) = line.split_once(' ') {
            if name == metric_name {
                if let Ok(v) = value.trim().parse::<f64>() {
                    return v;
                }
            }
        }
    }
    0.0
}

/// Finds the Prometheus text line for `metric_name{...}` whose label block
/// contains every string in `must_contain` (e.g. `["namespace=\"...\"",
/// "protocol=\"query\""]`), and parses its trailing value. `None` if no
/// matching line exists yet.
pub fn extract_labeled_metric(rendered: &str, metric_name: &str, must_contain: &[&str]) -> Option<f64> {
    let prefix = format!("{metric_name}{{");
    for line in rendered.lines() {
        if !line.starts_with(&prefix) {
            continue;
        }
        if !must_contain.iter().all(|needle| line.contains(needle)) {
            continue;
        }
        if let Some(value) = line.rsplit(' ').next() {
            if let Ok(v) = value.parse::<f64>() {
                return Some(v);
            }
        }
    }
    None
}

/// True if a labeled sample matching `must_contain` exists at all — see
/// [`extract_labeled_metric`] when the value itself matters.
pub fn has_labeled_metric(rendered: &str, metric_name: &str, must_contain: &[&str]) -> bool {
    extract_labeled_metric(rendered, metric_name, must_contain).is_some()
}
```

- [ ] **Step 2: Add `InFlightGuard` and `record_proxy_metrics` to `src/observability.rs`**

Add near the top, after the existing imports:

```rust
use std::time::Duration;
```

Append to the file:

```rust
/// Increments `gateway_proxy_requests_in_flight` on construction and
/// decrements it on drop — one guard per `proxy_handler` call, created before
/// any early return, so every exit path (a permission rejection, a namespace
/// lookup failure, a successful proxy, a timeout) decrements it exactly once
/// without each return site needing its own instrumentation.
pub struct InFlightGuard;

impl InFlightGuard {
    pub fn new() -> Self {
        metrics::gauge!("gateway_proxy_requests_in_flight").increment(1.0);
        Self
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!("gateway_proxy_requests_in_flight").decrement(1.0);
    }
}

impl Default for InFlightGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Records one proxied request's outcome: a `gateway_proxy_requests_total`
/// increment and a `gateway_proxy_request_duration_seconds` observation.
/// Called only from the same two points in `proxy_handler` that already emit
/// a usage event (`src/proxy.rs`) — a request rejected before reaching sqld
/// never calls this, matching `gateway_proxy_requests_total`'s definition as
/// proxy traffic that actually reached sqld.
pub fn record_proxy_metrics(protocol: &'static str, namespace: &str, status: StatusCode, duration: Duration) {
    let status_class = format!("{}xx", status.as_u16() / 100);
    metrics::counter!(
        "gateway_proxy_requests_total",
        "protocol" => protocol,
        "status_class" => status_class,
        "namespace" => namespace.to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "gateway_proxy_request_duration_seconds",
        "protocol" => protocol,
        "namespace" => namespace.to_string(),
    )
    .record(duration.as_secs_f64());
}
```

- [ ] **Step 3: Instrument `proxy_handler` in `src/proxy.rs`**

Add the in-flight guard right after `let start = Instant::now();`:

```rust
    let start = Instant::now();
    let _in_flight = crate::observability::InFlightGuard::new();
    let (parts, body) = request.into_parts();
```

Hoist the `protocol` computation out of the `emit_usage_event` closure so both it and `record_proxy_metrics` share one value. Immediately after `let (client, outbound_version) = state.client.for_version(parts.version);`, add:

```rust
    let protocol = if outbound_version == Version::HTTP_2 {
        "sync"
    } else {
        "query"
    };
```

In the `emit_usage_event` closure body, replace its own local `protocol` computation:

```rust
    let emit_usage_event = |status: StatusCode| {
        let protocol = if outbound_version == Version::HTTP_2 {
            "sync"
        } else {
            "query"
        };
```

with a reference to the now-shared variable (the closure captures `protocol` by copy, since `&'static str` is `Copy`):

```rust
    let emit_usage_event = |status: StatusCode| {
```

(i.e. delete those four lines from inside the closure — `protocol` is already in scope from the hoisted `let` above).

At the response-head-timeout arm, add the metrics call right after the existing `emit_usage_event` call:

```rust
        Err(_) => {
            tracing::error!(
                "proxy request to sqld timed out after {RESPONSE_HEAD_TIMEOUT:?} \
                 waiting for a response"
            );
            emit_usage_event(StatusCode::GATEWAY_TIMEOUT);
            crate::observability::record_proxy_metrics(
                protocol,
                &namespace,
                StatusCode::GATEWAY_TIMEOUT,
                start.elapsed(),
            );
            return StatusCode::GATEWAY_TIMEOUT.into_response();
        }
```

At the success path, add the metrics call right after the existing `emit_usage_event(upstream_parts.status);` line:

```rust
    emit_usage_event(upstream_parts.status);
    crate::observability::record_proxy_metrics(
        protocol,
        &namespace,
        upstream_parts.status,
        start.elapsed(),
    );
```

- [ ] **Step 4: Write the failing tests**

Append to `tests/observability_test.rs` (add `use hivewarden::auth::AppState;` etc. are already imported; add these new imports at the top of the file alongside the existing ones):

```rust
use axum::body::Body as _;
use bytes::Bytes as _;
use uuid::Uuid;
```

(Only add imports actually missing — `Uuid` is new; `Body`/`bytes` are already imported by Step 8 of Task 1 if named identically, so only add what the compiler flags as missing.)

Add a `register` helper, matching the one already used by `tests/usage_metering_test.rs`:

```rust
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

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

#[tokio::test]
async fn successful_request_increments_counter_and_histogram() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle.clone());
    let app = hivewarden::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("obs-{}@example.com", Uuid::new_v4())).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let rendered = handle.render();
    let namespace_label = format!("namespace=\"{user_id}\"");
    assert!(
        common::has_labeled_metric(
            &rendered,
            "gateway_proxy_requests_total",
            &["protocol=\"query\"", "status_class=\"2xx\"", &namespace_label],
        ),
        "missing counter sample: {rendered}"
    );
    assert!(
        common::has_labeled_metric(
            &rendered,
            "gateway_proxy_request_duration_seconds_count",
            &["protocol=\"query\"", &namespace_label],
        ),
        "missing histogram sample: {rendered}"
    );

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn in_flight_gauge_returns_to_baseline_after_request_completes() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle.clone());
    let app = hivewarden::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("obs-{}@example.com", Uuid::new_v4())).await;

    let baseline =
        common::extract_unlabeled_metric(&handle.render(), "gateway_proxy_requests_in_flight");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        common::extract_unlabeled_metric(&handle.render(), "gateway_proxy_requests_in_flight"),
        baseline,
        "gauge did not return to baseline after a successful request"
    );

    // Rejected before reaching sqld (no Authorization header) — the guard
    // must still fire even on a path that never calls record_proxy_metrics.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        common::extract_unlabeled_metric(&handle.render(), "gateway_proxy_requests_in_flight"),
        baseline,
        "gauge did not return to baseline after a rejected request"
    );

    delete_namespace(&user_id.to_string()).await;
}
```

- [ ] **Step 5: Run the tests to verify they fail, then pass**

Run: `cargo test --test observability_test -- --nocapture`
Expected first (before Steps 2-3 are applied): compile error, `observability::InFlightGuard`/`record_proxy_metrics` not found. After Steps 2-3: both new tests pass, alongside Task 1's test.

- [ ] **Step 6: Commit**

```bash
git add src/observability.rs src/proxy.rs tests/observability_test.rs tests/common/mod.rs
git commit -m "feat: record proxy request metrics"
```

---

### Task 3: Dependency health — Postgres pool + sqld reachability

**Files:**
- Modify: `src/observability.rs` (add `refresh_pg_pool_gauges`, `check_sqld_up`, `sqld_health_check_loop`; modify `metrics_handler`)
- Modify: `src/main.rs` (spawn the health-check loop)
- Modify: `tests/observability_test.rs`

**Interfaces:**
- Consumes: `common::metrics_handle()`, `common::extract_unlabeled_metric` (Tasks 1-2).
- Produces: `pub fn observability::refresh_pg_pool_gauges(pool: &PgPool)`.
- Produces: `pub async fn observability::check_sqld_up(client: &reqwest::Client, sqld_url: &str) -> bool` — the pure reachability probe, unit-testable without spawning the loop.
- Produces: `pub async fn observability::sqld_health_check_loop(client: reqwest::Client, sqld_url: String, interval: Duration)` — production wiring only; not called from tests (see Step 3's rationale).

- [ ] **Step 1: Add `refresh_pg_pool_gauges` and wire it into `metrics_handler`**

In `src/observability.rs`, add the import `use sqlx::PgPool;` and this function:

```rust
/// Sets `gateway_pg_pool_size`/`gateway_pg_pool_idle` from the pool's current
/// state. Called at `/metrics` scrape time (`metrics_handler`) rather than on
/// a timer — `PgPool::size`/`num_idle` are synchronous, in-memory reads, so
/// there is no cost to computing them fresh on every scrape.
pub fn refresh_pg_pool_gauges(pool: &PgPool) {
    metrics::gauge!("gateway_pg_pool_size").set(pool.size() as f64);
    metrics::gauge!("gateway_pg_pool_idle").set(pool.num_idle() as f64);
}
```

By this point (after Task 2's fix round added the `METRICS_TOKEN` bearer-token check — see the amendment note after Task 1), `metrics_handler` already validates the token before doing anything else and returns early on failure. Call `refresh_pg_pool_gauges` right after that check, on the authorized path only:

```rust
pub async fn metrics_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if state.metrics_token.is_empty() || token != Some(state.metrics_token.as_str()) {
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

(If the actual signature/body you find in `src/observability.rs` at this point differs slightly from this snippet — e.g. different variable names from how the fix round implemented it — adapt to match what's actually there; the important part is that `refresh_pg_pool_gauges` runs only after the token check passes, not before.)

- [ ] **Step 2: Add `check_sqld_up` and `sqld_health_check_loop`**

Append to `src/observability.rs`:

```rust
/// A single reachability probe against `sqld_url`: any HTTP response at all
/// counts as up (this answers "is the network path and the sqld process
/// alive," not "is every endpoint healthy"). A connection failure or timeout
/// counts as down.
pub async fn check_sqld_up(client: &reqwest::Client, sqld_url: &str) -> bool {
    client.get(sqld_url).send().await.is_ok()
}

/// Runs forever, probing `sqld_url` on `interval` and publishing the result
/// to the `gateway_sqld_up` gauge (`1.0` up, `0.0` down). Spawned once, in
/// production only, alongside the provisioning worker — see `main.rs`. Its
/// own interval, decoupled from `/metrics` scrape cadence, so a scrape never
/// blocks on a network call to sqld.
pub async fn sqld_health_check_loop(client: reqwest::Client, sqld_url: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let up = check_sqld_up(&client, &sqld_url).await;
        metrics::gauge!("gateway_sqld_up").set(if up { 1.0 } else { 0.0 });
    }
}
```

- [ ] **Step 3: Spawn the loop from `main.rs`**

In `src/main.rs`, add a constant alongside `PROVISIONING_WORKER_INTERVAL`:

```rust
const SQLD_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(15);
```

After the provisioning worker's `tokio::spawn` call, add:

```rust
    let health_check_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client construction with these settings cannot fail");
    tokio::spawn(observability::sqld_health_check_loop(
        health_check_client,
        config.sqld_url.clone(),
        SQLD_HEALTH_CHECK_INTERVAL,
    ));
```

Add `observability` to the `use hivewarden::{...}` import list at the top of `main.rs`.

- [ ] **Step 4: Write the failing tests**

Append to `tests/observability_test.rs`:

```rust
use hivewarden::observability;
use std::time::Duration;

#[tokio::test]
async fn metrics_endpoint_reports_pg_pool_gauges() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool, test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle)
        .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = hivewarden::app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", common::TEST_METRICS_TOKEN),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();

    // `set()` registers the metric even at value 0 — presence, not
    // magnitude, is what a freshly connected pool can promise.
    assert!(rendered.contains("gateway_pg_pool_size "));
    assert!(rendered.contains("gateway_pg_pool_idle "));
}

#[tokio::test]
async fn check_sqld_up_true_for_reachable_sqld() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(observability::check_sqld_up(&client, &test_sqld_url()).await);
}

#[tokio::test]
async fn check_sqld_up_false_for_unreachable_sqld() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    assert!(!observability::check_sqld_up(&client, "http://127.0.0.1:1").await);
}
```

- [ ] **Step 5: Run the tests to verify they fail, then pass**

Run: `cargo test --test observability_test -- --nocapture`
Expected first: compile error, `refresh_pg_pool_gauges`/`check_sqld_up` not found. After Steps 1-2: all three new tests pass.

- [ ] **Step 6: Run `cargo build` to confirm `main.rs` still compiles**

Run: `cargo build`
Expected: succeeds.

- [ ] **Step 7: Commit**

```bash
git add src/observability.rs src/main.rs tests/observability_test.rs
git commit -m "feat: add Postgres pool and sqld reachability gauges"
```

---

### Task 4: Provisioning worker metrics

**Files:**
- Modify: `src/observability.rs` (add `record_provisioning_outcome`, `refresh_provisioning_outbox_gauges`)
- Modify: `src/provisioning.rs` (call both from `attempt_provisioning`/`run_worker`; add a worker-tick duration histogram)
- Modify: `tests/provisioning_test.rs`

**Interfaces:**
- Consumes: `common::lock_outbox`, `common::delete_outbox_row` (existing), `common::metrics_handle`, `common::extract_labeled_metric` (Tasks 1-2).
- Produces: `pub fn observability::record_provisioning_outcome(outcome: &'static str)`.
- Produces: `pub async fn observability::refresh_provisioning_outbox_gauges(pool: &PgPool) -> Result<(), sqlx::Error>`.

- [ ] **Step 1: Add the two functions to `src/observability.rs`**

```rust
/// Increments `gateway_provisioning_attempts_total{outcome}` — called from
/// `attempt_provisioning` (`src/provisioning.rs`) with `"success"` or
/// `"failure"`.
pub fn record_provisioning_outcome(outcome: &'static str) {
    metrics::counter!("gateway_provisioning_attempts_total", "outcome" => outcome).increment(1);
}

/// Sets `gateway_provisioning_outbox_pending`/`_failed` from a fresh count of
/// `namespace_provisioning_outbox`. Called once per `run_worker` tick
/// (`src/provisioning.rs`) — a full `COUNT(*)`, not `fetch_pending`'s
/// batch-limited row count, so the gauge reflects the true queue depth even
/// when it exceeds one tick's batch size.
pub async fn refresh_provisioning_outbox_gauges(pool: &PgPool) -> Result<(), sqlx::Error> {
    let (pending,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM namespace_provisioning_outbox WHERE status = 'pending'",
    )
    .fetch_one(pool)
    .await?;
    let (failed,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM namespace_provisioning_outbox WHERE status = 'failed'",
    )
    .fetch_one(pool)
    .await?;
    metrics::gauge!("gateway_provisioning_outbox_pending").set(pending as f64);
    metrics::gauge!("gateway_provisioning_outbox_failed").set(failed as f64);
    Ok(())
}
```

- [ ] **Step 2: Call `record_provisioning_outcome` from `attempt_provisioning`**

In `src/provisioning.rs`, in `attempt_provisioning`'s `match create_result`, add one line to each arm:

```rust
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
            crate::observability::record_provisioning_outcome("success");
            Ok(true)
        }
        Ok(resp) => {
            record_failure(
                pool,
                row,
                &format!("sqld admin API returned {}", resp.status()),
            )
            .await?;
            crate::observability::record_provisioning_outcome("failure");
            Ok(false)
        }
        Err(e) => {
            record_failure(pool, row, &format!("sqld admin API request failed: {e}")).await?;
            crate::observability::record_provisioning_outcome("failure");
            Ok(false)
        }
    }
```

- [ ] **Step 3: Update `run_worker`'s tick to refresh the outbox gauges and record its own duration**

Add `use std::time::Instant;` to `src/provisioning.rs`'s existing `use std::time::Duration;` line (making it `use std::time::{Duration, Instant};`).

Replace `run_worker`'s loop body:

```rust
pub async fn run_worker(pool: PgPool, sqld_admin_url: String, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        let tick_start = Instant::now();
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
        if let Err(e) = crate::observability::refresh_provisioning_outbox_gauges(&pool).await {
            tracing::error!("failed to refresh provisioning outbox gauges: {e:#}");
        }
        metrics::histogram!("gateway_provisioning_worker_run_duration_seconds")
            .record(tick_start.elapsed().as_secs_f64());
    }
}
```

- [ ] **Step 4: Write the failing tests**

Append to `tests/provisioning_test.rs`:

```rust
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
```

Note: `use std::time::Duration;` is already present near the bottom of `tests/provisioning_test.rs` from the existing worker test — no new import needed for it, only `common::metrics_handle`/`extract_labeled_metric`, which come from `mod common;` already declared at the top of the file.

- [ ] **Step 5: Run the tests to verify they fail, then pass**

Run: `cargo test --test provisioning_test -- --nocapture`
Expected first: compile error, `observability::record_provisioning_outcome`/`refresh_provisioning_outbox_gauges` not found. After Steps 1-3: all `provisioning_test` tests pass, including the two new ones.

- [ ] **Step 6: Run the full test suite**

Run: `cargo test`
Expected: all tests across all binaries pass (this task touches shared production code paths — `attempt_provisioning`, `run_worker` — exercised by every other provisioning-adjacent test too).

- [ ] **Step 7: Commit**

```bash
git add src/observability.rs src/provisioning.rs tests/provisioning_test.rs
git commit -m "feat: add provisioning worker metrics"
```

---

### Task 5: Local Grafana + Prometheus stack, README docs

**Files:**
- Modify: `podman-compose.yml`
- Create: `prometheus.yml`
- Create: `grafana/provisioning/datasources/prometheus.yml`
- Modify: `README.md`

**Interfaces:**
- Consumes: the `gateway` service's `/metrics` endpoint (Tasks 1-4), already published at `127.0.0.1:8787` in `podman-compose.yml`.
- Produces: nothing consumed by later tasks — this is the plan's last task.

- [ ] **Step 1: Add the Prometheus scrape config**

Create `prometheus.yml`:

```yaml
global:
  scrape_interval: 15s

scrape_configs:
  - job_name: hivewarden
    authorization:
      credentials: dev-metrics-token
    static_configs:
      - targets: ["gateway:8787"]
```

`gateway:8787` uses compose's internal DNS — the same way the `gateway` service already reaches `postgres`/`sqld` by service name on their internal ports (see `podman-compose.yml`'s existing `DATABASE_URL`/`SQLD_URL`/`SQLD_ADMIN_URL` environment values). `dev-metrics-token` matches the `METRICS_TOKEN` value set on the `gateway` service in Step 3 below — `/metrics` now requires `Authorization: Bearer <METRICS_TOKEN>` (added during Task 2's review; see the amendment note after Task 1).

- [ ] **Step 2: Add the Grafana datasource provisioning file**

Create `grafana/provisioning/datasources/prometheus.yml`:

```yaml
apiVersion: 1

datasources:
  - name: Prometheus
    type: prometheus
    access: proxy
    url: http://prometheus:9090
    isDefault: true
```

- [ ] **Step 3: Add both services to `podman-compose.yml`, and give `gateway` its `METRICS_TOKEN`**

`METRICS_TOKEN` is now a required environment variable for the `gateway` service (added during Task 2's review — see the amendment note after Task 1). Add it to the existing `gateway` service's `environment` block, alongside `DATABASE_URL`/`SQLD_URL`/`SQLD_ADMIN_URL`/`LISTEN_ADDR`:

```yaml
      METRICS_TOKEN: dev-metrics-token
```

Then add the two new services after the existing `gateway` service block:

```yaml
  prometheus:
    image: docker.io/prom/prometheus:latest
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml:ro
    # Local-dev-only, unauthenticated by default — loopback only, matching
    # every other service in this file.
    ports:
      - "127.0.0.1:9090:9090"
    depends_on:
      - gateway
  grafana:
    image: docker.io/grafana/grafana:latest
    volumes:
      - ./grafana/provisioning:/etc/grafana/provisioning:ro
    environment:
      # Local dev only: anonymous viewer access so `podman-compose up` gives
      # a ready-to-browse Grafana with no login step. Loopback only, same
      # posture as postgres/sqld above.
      GF_AUTH_ANONYMOUS_ENABLED: "true"
      GF_AUTH_ANONYMOUS_ORG_ROLE: Viewer
    ports:
      - "127.0.0.1:3000:3000"
    depends_on:
      - prometheus
```

- [ ] **Step 4: Validate the compose file**

Run: `podman-compose -f podman-compose.yml config`
Expected: prints the fully resolved compose config with no errors (validates YAML syntax and service references). If `podman-compose` isn't available in this environment, validate the two new YAML files with a syntax check instead:

Run: `python3 -c "import yaml, sys; [yaml.safe_load(open(f)) for f in ['podman-compose.yml', 'prometheus.yml', 'grafana/provisioning/datasources/prometheus.yml']]; print('ok')"`
Expected: `ok`.

- [ ] **Step 5: Bring up the full stack and confirm Prometheus is scraping the gateway**

Run:
```sh
podman-compose -f podman-compose.yml up -d --build
sleep 20
curl -s http://127.0.0.1:9090/api/v1/targets | grep -o '"health":"[a-z]*"'
```
Expected: at least one `"health":"up"` (the `hivewarden` job). If it prints `"health":"down"` instead, check `podman-compose logs gateway` and `podman-compose logs prometheus` before proceeding — a down target here means Task 1-4's `/metrics` route or this task's scrape config has a mismatch, not something to paper over.

Run: `curl -s http://127.0.0.1:3000/api/datasources` (anonymous viewer access, no auth header needed)
Expected: JSON containing `"name":"Prometheus"` — proves the datasource provisioning file loaded.

Tear down: `podman-compose -f podman-compose.yml down`

- [ ] **Step 6: Document it in `README.md`**

Add a new subsection under `## Development`, after the existing `### Logging` subsection:

```markdown
### Observability

`GET /metrics` exposes Prometheus-format metrics: proxy request counts and
latency (by protocol/status/namespace), Postgres pool size, sqld
reachability, and the provisioning worker's outbox queue depth. Requires
`Authorization: Bearer <METRICS_TOKEN>` — added because the per-tenant
`namespace` label would otherwise let anyone reachable enumerate every
tenant's UUID and traffic volume through an unauthenticated endpoint. Set
`METRICS_TOKEN` to any secret string; the dev compose stack below uses
`dev-metrics-token`.

`podman-compose up` also brings up Prometheus (`127.0.0.1:9090`, scraping
the gateway every 15s with that token) and Grafana (`127.0.0.1:3000`,
anonymous viewer access, Prometheus pre-wired as its datasource) for local
testing. No dashboards ship by default — add your own in Grafana's UI, or
build them against the metric names above.
```

- [ ] **Step 7: Commit**

```bash
git add podman-compose.yml prometheus.yml grafana/provisioning/datasources/prometheus.yml README.md
git commit -m "feat: add local Prometheus/Grafana stack and observability docs"
```
