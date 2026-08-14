# OTel Tracing and Loki Log Aggregation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give hivewarden distributed tracing (HTTP → DB → sqld proxy spans, exported via OTLP through an OTel Collector into Tempo) and searchable, trace-correlated logs (pushed to Loki), both browsable in Grafana.

**Architecture:** `hivewarden` gains a layered `tracing_subscriber` stack — the existing stdout JSON layer, plus two new optional layers (OTel export, Loki push) that no-op when their env vars are unset. `tower-http`'s `TraceLayer` creates one span per HTTP request and stamps a `trace_id` field onto it (read back from the OTel context via `tracing-opentelemetry`'s public `OpenTelemetrySpanExt`), so every JSON log line emitted during that request carries `trace_id` — Loki's derived fields turn that into a clickable jump into the matching Tempo trace. New compose services (`loki`, `tempo`, `otel-collector`) and Grafana datasources wire it all up for local dev.

**Tech Stack:** `opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp` (OTLP/gRPC/tonic exporter), `tracing-opentelemetry` (bridges `tracing` spans to OTel), `tower-http` (`trace` feature, HTTP-layer spans), `tracing-loki` (Loki push layer), Grafana Loki + Tempo + `otel/opentelemetry-collector-contrib`.

**Spec:** `docs/superpowers/specs/2026-08-14-otel-tracing-and-logs-design.md`

## Global Constraints

- `OTEL_EXPORTER_OTLP_ENDPOINT` and `LOKI_URL` are **optional** `Config` fields (unlike `API_KEY_PEPPER`/`METRICS_TOKEN`) — their absence must not be an error, and `cargo test`/a bare `cargo run` outside podman-compose must keep working unmodified with neither set.
- New compose services (`loki`, `tempo`, `otel-collector`) publish **no host ports** — internal compose-network only, matching the spec's scope decision.
- Every `cargo add` in this plan installs whatever the latest compatible version resolves to — do not hand-edit `Cargo.toml` version numbers.
- Exact dependency versions are not pinned in this plan (per the spec) — use `cargo add <crate>` and let it resolve.

---

### Task 1: Fix the stale Prometheus scrape target

**Files:**
- Modify: `prometheus.yml`

**Interfaces:** None — standalone config fix, no code dependencies.

- [ ] **Step 1: Fix the target hostname**

`prometheus.yml` currently reads:

```yaml
global:
  scrape_interval: 15s

scrape_configs:
  - job_name: hivewarden
    authorization:
      # Local-dev-only placeholder — do not reuse this value for a real
      # deployment. Set the gateway's actual METRICS_TOKEN here instead.
      credentials: dev-metrics-token
    static_configs:
      - targets: ["gateway:8787"]
```

Change the `targets` line — `gateway` was the compose service's old name before it was renamed to `hivewarden`:

```yaml
    static_configs:
      - targets: ["hivewarden:8787"]
```

- [ ] **Step 2: Verify the target comes up healthy**

```bash
podman-compose -f podman-compose.yml up -d prometheus
sleep 20
curl -s http://127.0.0.1:9090/api/v1/targets | grep -o '"health":"[a-z]*"'
```

Expected: `"health":"up"` (previously `"health":"down"` with `lastError` about connecting to `gateway:8787`).

- [ ] **Step 3: Commit**

```bash
git add prometheus.yml
git commit -m "fix: point prometheus at the renamed hivewarden compose service

The gateway->hivewarden compose service rename left prometheus.yml
scraping a hostname that no longer resolves, silently taking the
metrics target down."
```

---

### Task 2: Add Loki, Tempo, and the OTel Collector to podman-compose.yml

**Files:**
- Create: `tempo.yaml`
- Create: `otel-collector-config.yaml`
- Modify: `podman-compose.yml`

**Interfaces:**
- Produces: compose services reachable from other containers as `http://loki:3100`, `http://tempo:3200` (query) / `tempo:4317` (OTLP, collector-only), `http://otel-collector:4317` (OTLP gRPC, what `hivewarden` will point at in Task 6).

- [ ] **Step 1: Write the Tempo config**

Create `tempo.yaml`:

```yaml
server:
  http_listen_port: 3200

distributor:
  receivers:
    otlp:
      protocols:
        grpc:
          endpoint: 0.0.0.0:4317
        http:
          endpoint: 0.0.0.0:4318

storage:
  trace:
    backend: local
    local:
      path: /tmp/tempo/blocks

compactor:
  compaction:
    block_retention: 24h
```

- [ ] **Step 2: Write the OTel Collector config**

Create `otel-collector-config.yaml`:

```yaml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317

exporters:
  otlp:
    endpoint: tempo:4317
    tls:
      insecure: true

service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [otlp]
```

- [ ] **Step 3: Add the three services to podman-compose.yml**

In `podman-compose.yml`, after the `sqld` service block and before `hivewarden`, insert:

```yaml
  loki:
    image: docker.io/grafana/loki:latest
    # No host-published port: only hivewarden (log push) and grafana
    # (query) need to reach it, both over the compose network.

  tempo:
    image: docker.io/grafana/tempo:latest
    command: ["-config.file=/etc/tempo.yaml"]
    volumes:
      - ./tempo.yaml:/etc/tempo.yaml:ro,Z
    # No host-published port: only otel-collector (OTLP) and grafana
    # (query) need to reach it.

  otel-collector:
    image: docker.io/otel/opentelemetry-collector-contrib:latest
    command: ["--config=/etc/otel-collector-config.yaml"]
    volumes:
      - ./otel-collector-config.yaml:/etc/otel-collector-config.yaml:ro,Z
    depends_on:
      - tempo
    # No host-published port: only hivewarden needs to reach it, over the
    # compose network.
```

- [ ] **Step 4: Point hivewarden at the collector and Loki**

In the `hivewarden` service's `environment` block, add two new lines after `API_KEY_PEPPER`:

```yaml
      API_KEY_PEPPER: dev-api-key-pepper-do-not-use-in-prod-min-32-chars
      OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-collector:4317
      LOKI_URL: http://loki:3100
```

And add `otel-collector` and `loki` to its `depends_on` list:

```yaml
    depends_on:
      - postgres
      - sqld
      - otel-collector
      - loki
```

- [ ] **Step 5: Bring the new services up and verify they're healthy**

```bash
podman-compose -f podman-compose.yml up -d loki tempo otel-collector
sleep 10
podman exec hivewarden_loki_1 wget -qO- http://127.0.0.1:3100/ready
podman exec hivewarden_tempo_1 wget -qO- http://127.0.0.1:3200/ready
podman logs hivewarden_otel-collector_1 --tail 20
```

Expected: Loki prints `ready`, Tempo returns 200 (empty body is fine), and the collector's log shows it started its `traces` pipeline without an error.

- [ ] **Step 6: Commit**

```bash
git add tempo.yaml otel-collector-config.yaml podman-compose.yml
git commit -m "feat: add Loki, Tempo, and an OTel Collector to the local dev stack

Internal-only services (no host-published ports) — hivewarden will
push traces to the collector and logs to Loki once instrumented."
```

---

### Task 3: Provision Loki and Tempo as Grafana datasources

**Files:**
- Create: `grafana/provisioning/datasources/loki.yml`
- Create: `grafana/provisioning/datasources/tempo.yml`

**Interfaces:** None — Grafana reads these on startup; no code depends on them.

- [ ] **Step 1: Add the Tempo datasource**

Create `grafana/provisioning/datasources/tempo.yml`:

```yaml
apiVersion: 1

datasources:
  - name: Tempo
    type: tempo
    access: proxy
    uid: tempo
    url: http://tempo:3200
    jsonData:
      tracesToLogsV2:
        datasourceUid: loki
        spanStartTimeShift: '-1h'
        spanEndTimeShift: '1h'
        filterByTraceID: true
        customQuery: true
        query: '{service_name="hivewarden"} |= "$${__span.traceId}"'
```

- [ ] **Step 2: Add the Loki datasource, with a derived field back to Tempo**

Create `grafana/provisioning/datasources/loki.yml`:

```yaml
apiVersion: 1

datasources:
  - name: Loki
    type: loki
    access: proxy
    uid: loki
    url: http://loki:3100
    jsonData:
      derivedFields:
        - datasourceUid: tempo
          matcherRegex: '"trace_id":"(\w+)"'
          name: TraceID
          url: '$${__value.raw}'
```

- [ ] **Step 3: Restart Grafana and verify both datasources are registered**

```bash
podman-compose -f podman-compose.yml up -d grafana
sleep 5
curl -s http://127.0.0.1:3000/api/datasources | grep -o '"name":"[A-Za-z]*"'
```

Expected: `"name":"Prometheus"`, `"name":"Loki"`, `"name":"Tempo"` all present (anonymous viewer access is already enabled in `podman-compose.yml`, so no login is needed for this call).

- [ ] **Step 4: Commit**

```bash
git add grafana/provisioning/datasources/loki.yml grafana/provisioning/datasources/tempo.yml
git commit -m "feat: provision Loki and Tempo as Grafana datasources

Tempo's tracesToLogsV2 and Loki's derived field cross-reference each
other by uid, so Explore can jump from a trace to its logs and back."
```

---

### Task 4: Add optional OTel/Loki config fields

**Files:**
- Modify: `src/config.rs`
- Modify: `.env.example`

**Interfaces:**
- Produces: `Config.otel_exporter_otlp_endpoint: Option<String>`, `Config.loki_url: Option<String>` — consumed by Task 6's `main.rs` subscriber wiring.

- [ ] **Step 1: Add the two fields**

In `src/config.rs`, add both fields to the `Config` struct:

```rust
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub sqld_url: String,
    pub sqld_admin_url: String,
    pub listen_addr: String,
    pub metrics_token: String,
    pub api_key_pepper: String,
    /// OTLP/gRPC endpoint traces are exported to (e.g.
    /// `http://otel-collector:4317`). Optional: `None` means tracing spans
    /// are created and logged normally but never exported anywhere —
    /// `cargo test`/a bare `cargo run` must not require a collector.
    pub otel_exporter_otlp_endpoint: Option<String>,
    /// Base URL of a Loki instance to push logs to (e.g.
    /// `http://loki:3100`). Optional: `None` means logs stay stdout-only,
    /// exactly like today.
    pub loki_url: Option<String>,
}
```

- [ ] **Step 2: Read them in `Config::from_env`**

In the same file, inside `from_env`'s `Ok(Config { ... })` block, add both new fields — read with `.ok()`, not `.context(...)?`, since absence is valid:

```rust
        Ok(Config {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            sqld_url: std::env::var("SQLD_URL").context("SQLD_URL must be set")?,
            sqld_admin_url: std::env::var("SQLD_ADMIN_URL")
                .context("SQLD_ADMIN_URL must be set")?,
            listen_addr: std::env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8787".to_string()),
            metrics_token,
            api_key_pepper,
            otel_exporter_otlp_endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
            loki_url: std::env::var("LOKI_URL").ok(),
        })
```

- [ ] **Step 3: Document both in `.env.example`**

Append to `.env.example`:

```
# Optional. OTLP/gRPC endpoint traces are exported to. Unset means tracing
# spans are still created (and logged) but never exported anywhere — useful
# for `cargo run`/`cargo test` outside podman-compose, where no collector is
# running. Inside podman-compose, the hivewarden service sets this itself.
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
# Optional. Base URL of a Loki instance to push logs to. Unset means logs
# stay stdout-only, exactly like without this variable at all.
LOKI_URL=http://127.0.0.1:3100
```

- [ ] **Step 4: Verify it compiles and existing tests still pass unmodified**

```bash
cargo build
cargo test --test config 2>/dev/null; cargo test 2>&1 | tail -20
```

Expected: builds cleanly; the full suite passes with no changes needed anywhere else — nothing constructs `Config` directly except `main.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs .env.example
git commit -m "feat: add optional OTEL_EXPORTER_OTLP_ENDPOINT and LOKI_URL config

Both default to None/unset rather than erroring, unlike API_KEY_PEPPER
and METRICS_TOKEN — cargo test and a bare cargo run must keep working
with neither set."
```

---

### Task 5: Add the telemetry module (tracer + Loki layer construction)

**Files:**
- Modify: `Cargo.toml`
- Create: `src/telemetry.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks (pure new code + deps).
- Produces: `telemetry::init_tracer(otlp_endpoint: &str) -> anyhow::Result<opentelemetry_sdk::trace::Tracer>` and `telemetry::init_loki_layer(loki_url: &str) -> anyhow::Result<(tracing_loki::Layer, tracing_loki::BackgroundTask)>` — both consumed by Task 6's `main.rs`.

- [ ] **Step 1: Add the new dependencies**

```bash
cargo add opentelemetry
cargo add opentelemetry_sdk
cargo add opentelemetry-otlp --features grpc-tonic
cargo add tracing-opentelemetry
cargo add tracing-loki
cargo add url
```

- [ ] **Step 2: Write `src/telemetry.rs`**

```rust
//! Constructs the two *optional* tracing_subscriber layers this project
//! adds on top of its always-on stdout JSON layer: OTLP trace export and a
//! Loki log push. Both are wired up in `main.rs` only when their
//! corresponding `Config` field is `Some` — see
//! `docs/superpowers/specs/2026-08-14-otel-tracing-and-logs-design.md`.

use anyhow::Context;
use opentelemetry_otlp::WithExportConfig;

/// Builds an OTLP/gRPC (tonic) span exporter pointed at `otlp_endpoint`,
/// wraps it in a batching `SdkTracerProvider` tagged with `service.name =
/// hivewarden`, registers that provider as the process-global OTel tracer
/// provider, and returns a `Tracer` from it — ready to hand to
/// `tracing_opentelemetry::layer().with_tracer(...)` in `main.rs`.
pub fn init_tracer(otlp_endpoint: &str) -> anyhow::Result<opentelemetry_sdk::trace::Tracer> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otlp_endpoint)
        .build()
        .context("failed to build OTLP span exporter")?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name("hivewarden")
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    Ok(provider.tracer("hivewarden"))
}

/// Builds a `tracing-loki` layer pushing to `loki_url`, labeled
/// `service_name=hivewarden` — matching the `service_name` label the Tempo
/// datasource's `tracesToLogsV2` query filters on
/// (`grafana/provisioning/datasources/tempo.yml`), so both point at the
/// same log stream. Returns the layer plus its background delivery task,
/// which the caller (`main.rs`) must `tokio::spawn` — the layer only
/// buffers lines, it does not send them itself.
pub fn init_loki_layer(
    loki_url: &str,
) -> anyhow::Result<(tracing_loki::Layer, tracing_loki::BackgroundTask)> {
    let url = url::Url::parse(loki_url).context("invalid LOKI_URL")?;
    let (layer, task) = tracing_loki::builder()
        .label("service_name", "hivewarden")
        .context("invalid tracing-loki label")?
        .build_url(url)
        .context("failed to build tracing-loki layer")?;
    Ok((layer, task))
}
```

- [ ] **Step 3: Register the module**

In `src/lib.rs`, add to the module list at the top (alphabetical, matching the existing ordering):

```rust
pub mod routing;
pub mod telemetry;
```

(insert `pub mod telemetry;` right after the existing `pub mod routing;` line)

- [ ] **Step 4: Verify it compiles**

```bash
cargo build
```

Expected: builds cleanly. `init_tracer`/`init_loki_layer` are unused at this point (Task 6 wires them into `main.rs`) — `cargo build` will warn about unused `pub` functions in a binary crate only if nothing in the crate calls them yet, which is expected and fine to leave until Task 6 lands in the same session.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/telemetry.rs src/lib.rs
git commit -m "feat: add telemetry module for OTLP tracer and Loki layer construction

Not yet wired into main.rs — that's the next task. Both constructors
are pure functions so they're straightforward to call from main.rs
without touching this file again."
```

---

### Task 6: Wire the layered subscriber into main.rs

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `telemetry::init_tracer` and `telemetry::init_loki_layer` (Task 5), `config.otel_exporter_otlp_endpoint`/`config.loki_url` (Task 4).

- [ ] **Step 1: Replace the subscriber init block**

`src/main.rs` currently starts:

```rust
use std::time::Duration;

use hivewarden::{AppState, app, config::Config, db, observability, provisioning};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::EnvFilter;
```

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // JSON output for aggregator-friendly NDJSON, but still honouring
    // `RUST_LOG` — `fmt().json()` alone hard-wires the level floor to INFO
    // with no way to raise or lower verbosity in a deployed environment,
    // which the plain `fmt::init()` this replaced did support.
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = Config::from_env()?;
```

Replace the imports with:

```rust
use std::time::Duration;

use hivewarden::{AppState, app, config::Config, db, observability, provisioning, telemetry};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
```

And replace the subscriber-init block (everything from `tracing_subscriber::fmt()` through `.init();`) with:

```rust
    // `Config::from_env` must run before the subscriber is built — whether
    // the OTel/Loki layers exist at all depends on its output.
    let config = Config::from_env()?;

    // The OTel layer and the Loki layer are each `Option<Layer>` — a blanket
    // impl in tracing_subscriber makes `Option<L>: Layer<S>` a no-op when
    // `None`, so the subscriber degrades cleanly to today's stdout-JSON-only
    // behavior when neither OTEL_EXPORTER_OTLP_ENDPOINT nor LOKI_URL is set.
    let otel_layer = config
        .otel_exporter_otlp_endpoint
        .as_deref()
        .map(telemetry::init_tracer)
        .transpose()?
        .map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer));

    let mut loki_task = None;
    let loki_layer = match config.loki_url.as_deref() {
        Some(loki_url) => {
            let (layer, task) = telemetry::init_loki_layer(loki_url)?;
            loki_task = Some(task);
            Some(layer)
        }
        None => None,
    };

    // JSON output for aggregator-friendly NDJSON, but still honouring
    // `RUST_LOG` — `fmt().json()` alone hard-wires the level floor to INFO
    // with no way to raise or lower verbosity in a deployed environment,
    // which the plain `fmt::init()` this replaced did support.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .with(otel_layer)
        .with(loki_layer)
        .init();

    if let Some(task) = loki_task {
        tokio::spawn(task);
    }
```

- [ ] **Step 2: Remove the now-duplicate `let config = Config::from_env()?;` line**

Immediately below the block just inserted, the original code still has its own `let config = Config::from_env()?;` — delete that line (it's now the first statement of the block above).

- [ ] **Step 3: Verify it builds and the app still starts cleanly with no OTel/Loki env vars set**

```bash
cargo build
DATABASE_URL=postgres://gateway:gateway@127.0.0.1:5433/gateway \
SQLD_URL=http://127.0.0.1:8081 \
SQLD_ADMIN_URL=http://127.0.0.1:8090 \
METRICS_TOKEN=t \
API_KEY_PEPPER=dev-api-key-pepper-do-not-use-in-prod-min-32-chars \
LISTEN_ADDR=127.0.0.1:18787 \
timeout 3 cargo run || true
```

Expected: builds cleanly, and the timed-out run's output shows the normal `"hivewarden listening on 127.0.0.1:18787"` JSON log line with no OTel/Loki-related errors (neither env var was set, so both layers are `None`).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: wire optional OTel and Loki tracing_subscriber layers into main.rs

Neither layer is active unless its env var is set — a bare cargo run
or cargo test is unaffected."
```

---

### Task 7: Add HTTP-layer tracing spans with trace_id correlation

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: every HTTP request now runs inside a `tracing::Span` named `http_request` carrying a `trace_id` field, readable via `tracing::Span::current()` by any code called during request handling (this is what Tasks 8-10's DB/proxy spans nest under).

- [ ] **Step 1: Add tower-http**

```bash
cargo add tower-http --features trace
```

- [ ] **Step 2: Add the TraceLayer to `app()`**

In `src/lib.rs`, add to the imports:

```rust
use opentelemetry::trace::{Span as _, TraceContextExt};
use tower_http::trace::TraceLayer;
use tracing_opentelemetry::OpenTelemetrySpanExt;
```

(`Span as _` avoids colliding with `tracing::Span`, already used elsewhere in this file as a return type — only its `.span_context()` method is needed, so it's imported unnamed.)

Add a `make_span` helper function above `pub fn app`:

```rust
/// Creates the one-per-request span `TraceLayer` attaches below. Stamps a
/// `trace_id` field onto it immediately, read back from this span's own
/// OTel context via `OpenTelemetrySpanExt` — this is what makes every JSON
/// log line emitted during the request carry a `trace_id` (the fmt layer's
/// default `with_current_span(true)` includes it), which is what Loki's
/// derived field (`grafana/provisioning/datasources/loki.yml`) keys on to
/// link a log line to its Tempo trace. Works whether or not the OTel layer
/// is actually active (`main.rs`, Task 6) — with no OTel layer registered,
/// `context().span().span_context()` is a valid-but-empty span context, and
/// `trace_id` renders as all-zeroes rather than failing.
fn make_span(request: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    let span = tracing::info_span!(
        "http_request",
        method = %request.method(),
        path = %request.uri().path(),
        trace_id = tracing::field::Empty,
    );
    let trace_id = span.context().span().span_context().trace_id();
    span.record("trace_id", tracing::field::display(trace_id));
    span
}
```

Then, in `pub fn app(state: AppState) -> Router`, add `.layer(TraceLayer::new_for_http().make_span_with(make_span))` to the router chain. The end of the chain currently reads:

```rust
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
```

Add the trace layer immediately before `.with_state(state)`, so it wraps every route including `/healthz`/`/metrics`/`/users`:

```rust
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
        .layer(TraceLayer::new_for_http().make_span_with(make_span))
        .with_state(state)
```

- [ ] **Step 3: Verify the full test suite still passes**

```bash
cargo test
```

Expected: passes unmodified — `TraceLayer` only adds tracing spans around requests, it doesn't change status codes or bodies, so no existing assertion is affected.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs
git commit -m "feat: add tower-http TraceLayer with trace_id-stamped request spans

One span per HTTP request, trace_id read back from the span's own OTel
context so every log line during the request carries it for Loki<->Tempo
correlation."
```

---

### Task 8: Instrument DB-access functions

**Files:**
- Modify: `src/db.rs`
- Modify: `src/registration.rs`

**Interfaces:** None new — purely adds spans around existing functions, no signature changes.

- [ ] **Step 1: Instrument `src/db.rs`'s query functions**

Add `#[tracing::instrument(skip(pool))]` immediately above each of the two query functions (`pool` is skipped because a `&PgPool` isn't meaningfully loggable):

```rust
#[tracing::instrument(skip(pool))]
pub async fn find_api_key_by_prefix(
    pool: &PgPool,
    prefix: &str,
) -> Result<Option<ApiKeyRow>, sqlx::Error> {
```

```rust
#[tracing::instrument(skip(pool))]
pub async fn find_database_mapping(
    pool: &PgPool,
    owner_type: &str,
    owner_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
```

- [ ] **Step 2: Instrument `src/registration.rs`'s insert functions**

Add `#[tracing::instrument(skip(pool, api_key_pepper))]` above `insert_user` — `api_key_pepper` is skipped because it's a secret, not just because it isn't loggable:

```rust
#[tracing::instrument(skip(pool, api_key_pepper))]
async fn insert_user(
    pool: &PgPool,
    email: &str,
    api_key_pepper: &[u8],
) -> Result<(Uuid, String, OutboxRow), sqlx::Error> {
```

Add `#[tracing::instrument(skip(pool))]` above `insert_org`:

```rust
#[tracing::instrument(skip(pool))]
async fn insert_org(
    pool: &PgPool,
    name: &str,
    creator_user_id: Uuid,
) -> Result<(Uuid, OutboxRow), sqlx::Error> {
```

- [ ] **Step 3: Verify the full test suite still passes**

```bash
cargo test
```

Expected: passes unmodified — `#[tracing::instrument]` only adds a span around the function body, no behavior change.

- [ ] **Step 4: Commit**

```bash
git add src/db.rs src/registration.rs
git commit -m "feat: instrument DB-access functions with tracing spans

A slow request's trace can now show whether time was spent in
Postgres specifically, nested under the request's http_request span."
```

---

### Task 9: Instrument the org/API-key HTTP handlers

**Files:**
- Modify: `src/api_keys.rs`
- Modify: `src/org/members.rs`
- Modify: `src/org/admin.rs`

**Interfaces:** None new — purely adds spans around existing handlers, no signature changes.

- [ ] **Step 1: Instrument `src/api_keys.rs`**

Add `#[tracing::instrument(skip(state))]` above each of the three handlers (`skip_all` is not used here because `Extension(owner): Extension<AuthedOwner>` — `AuthedOwner` derives `Debug` — is useful, free correlation data in the span; only `state: AppState`, which doesn't derive `Debug`, needs skipping):

```rust
#[tracing::instrument(skip(state))]
pub async fn list_keys(State(state): State<AppState>, Extension(owner): Extension<AuthedOwner>) -> Response {
```

```rust
#[tracing::instrument(skip(state))]
pub async fn create_key(State(state): State<AppState>, Extension(owner): Extension<AuthedOwner>) -> Response {
```

```rust
#[tracing::instrument(skip(state))]
pub async fn revoke_key(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(key_id): Path<Uuid>,
) -> Response {
```

- [ ] **Step 2: Instrument `src/org/members.rs`**

`add_member` takes a `Json<AddMemberRequest>`, which does not derive `Debug` — skip it explicitly alongside `state`. `list_members`/`remove_member` only need `state` skipped.

```rust
#[tracing::instrument(skip(state))]
pub async fn list_members(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
) -> Response {
```

```rust
#[tracing::instrument(skip(state, req))]
pub async fn add_member(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Response {
```

`remove_member`'s `Path` parameter destructures a tuple (`Path((org_id, user_id))`) — `#[tracing::instrument]`'s automatic field capture is unreliable on tuple-destructured parameters across versions, so use `skip_all` here rather than naming individual params to skip:

```rust
#[tracing::instrument(skip_all)]
pub async fn remove_member(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
) -> Response {
```

- [ ] **Step 3: Instrument `src/org/admin.rs`**

`create_role`/`update_role`/`assign_member_role` each take a `Json<...Request>` (none derive `Debug`) — skip those alongside `state`. `list_roles`/`delete_role` only need `state` skipped.

```rust
#[tracing::instrument(skip(state, req))]
pub async fn create_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
    Json(req): Json<CreateRoleRequest>,
) -> Response {
```

```rust
#[tracing::instrument(skip(state))]
pub async fn list_roles(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
) -> Response {
```

`update_role` also destructures a tuple `Path` parameter — same reasoning, use `skip_all`:

```rust
#[tracing::instrument(skip_all)]
pub async fn update_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, role_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateRoleRequest>,
) -> Response {
```

`delete_role` and `assign_member_role` also destructure a tuple `Path` parameter — same reasoning as `remove_member` above, use `skip_all`:

```rust
#[tracing::instrument(skip_all)]
pub async fn delete_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, role_id)): Path<(Uuid, Uuid)>,
) -> Response {
```

```rust
#[tracing::instrument(skip_all)]
pub async fn assign_member_role(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<AssignRoleRequest>,
) -> Response {
```

- [ ] **Step 4: Verify the full test suite still passes**

```bash
cargo test
```

Expected: passes unmodified.

- [ ] **Step 5: Commit**

```bash
git add src/api_keys.rs src/org/members.rs src/org/admin.rs
git commit -m "feat: instrument org/API-key HTTP handlers with tracing spans

Each handler's span nests under its request's http_request span,
alongside the DB spans from the previous task."
```

---

### Task 10: Instrument the sqld proxy handler

**Files:**
- Modify: `src/proxy.rs`

**Interfaces:** None new — purely adds spans around existing code, no signature changes.

- [ ] **Step 1: Add the `Instrument` import**

In `src/proxy.rs`, add to the imports:

```rust
use tracing::Instrument;
```

- [ ] **Step 2: Instrument `proxy_handler`**

`Extension(owner)`/`request: Request` don't derive `Debug` (`Request` doesn't at all) — use `skip_all`:

```rust
#[tracing::instrument(skip_all)]
pub async fn proxy_handler(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    request: Request,
) -> Response {
```

- [ ] **Step 3: Wrap the outbound sqld call in its own child span**

The existing outbound-call block reads:

```rust
    let upstream = match tokio::time::timeout(RESPONSE_HEAD_TIMEOUT, client.request(outbound)).await
    {
```

Change it to wrap the request future in a `sqld_request` span, so a trace's waterfall shows time spent waiting on sqld as a distinct segment from local gateway processing:

```rust
    let sqld_span = tracing::info_span!("sqld_request", protocol);
    let upstream = match tokio::time::timeout(
        RESPONSE_HEAD_TIMEOUT,
        client.request(outbound).instrument(sqld_span),
    )
    .await
    {
```

(`protocol` is already in scope at this point in the function, computed a few lines above as `let protocol = if outbound_version == Version::HTTP_2 { "sync" } else { "query" };`.)

- [ ] **Step 4: Verify the full test suite still passes**

```bash
cargo test
```

Expected: passes unmodified — this only adds spans, no behavior change to the request/response handling.

- [ ] **Step 5: Commit**

```bash
git add src/proxy.rs
git commit -m "feat: instrument proxy_handler with a child span around the sqld call

Isolates time-in-sqld from local gateway processing (auth, namespace
resolution, response streaming) in a request's trace waterfall."
```

---

### Task 11: End-to-end verification against the full local stack

**Files:** None modified — this task is manual verification only, per the spec's Testing section.

**Interfaces:** None.

- [ ] **Step 1: Rebuild and bring up the full stack**

```bash
podman-compose -f podman-compose.yml down
podman-compose -f podman-compose.yml up -d --build
sleep 15
```

- [ ] **Step 2: Confirm the Prometheus target is healthy (Task 1's fix)**

```bash
curl -s http://127.0.0.1:9090/api/v1/targets | grep -o '"health":"[a-z]*"'
```

Expected: `"health":"up"`.

- [ ] **Step 3: Register a user and issue a few requests to generate a trace**

```bash
curl -s -X POST http://127.0.0.1:8787/users \
  -H 'Content-Type: application/json' \
  -d '{"email":"otel-check@example.com"}' | tee /tmp/user.json
API_KEY=$(grep -o '"api_key":"[^"]*"' /tmp/user.json | cut -d'"' -f4)
curl -s http://127.0.0.1:8787/api-keys -H "Authorization: Bearer $API_KEY"
```

- [ ] **Step 4: Confirm a trace landed in Tempo**

```bash
sleep 5
curl -s "http://127.0.0.1:3200/api/search?tags=&limit=5" | head -c 500
```

(There is no host-published Tempo port per this plan's design, so run this `curl` from inside the compose network instead if the host can't reach it directly: `podman exec hivewarden_otel-collector_1 wget -qO- "http://tempo:3200/api/search?tags=&limit=5"`.)

Expected: a non-empty `traces` array, including a trace named `http_request` for the `POST /users` and `GET /api-keys` calls just made.

- [ ] **Step 5: Confirm logs landed in Loki with a `trace_id` field**

```bash
podman exec hivewarden_loki_1 wget -qO- 'http://127.0.0.1:3100/loki/api/v1/query_range?query={service_name="hivewarden"}&limit=5'
```

Expected: a non-empty `result` array; at least one log line's JSON payload contains a `"trace_id"` field with a non-zero 32-hex-character value.

- [ ] **Step 6: Confirm the Grafana Explore trace<->log correlation works**

Open `http://localhost:3000/explore` in a browser, select the Tempo datasource, search for recent traces, open the `http_request` trace from Step 4, and confirm a "Logs for this span" (or equivalent) link is present and lands on the matching Loki log lines. Then, from a Loki log line containing `trace_id`, confirm the derived-field link jumps back to the same Tempo trace.

- [ ] **Step 7: No commit for this task** — it's verification only. If any step above fails, return to the relevant earlier task, fix the root cause, and re-run this task's steps from Step 1.
