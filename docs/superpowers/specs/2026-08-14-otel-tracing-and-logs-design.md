# wardn OTel Tracing and Log Aggregation — Design

**Status:** Approved. Seventh sub-project of `wardn`, built on the walking skeleton, org roles, database provisioning, org membership/API keys, usage metering, and observability (metrics).

## Goal

Give whoever operates the gateway a way to answer "why is *this specific request* slow/failing" (a trace through HTTP → Postgres → sqld) and "show me every log line for this request" (searchable, trace-linked logs) — the two visibility gaps the original observability slice explicitly deferred.

## Motivation

The metrics slice (`docs/superpowers/specs/2026-08-12-observability-design.md`) answers "is the service healthy right now" in aggregate. It cannot answer "why did *this* request take 4 seconds" or "show me every log line for request X" — that needs traces and searchable, correlated logs. This slice closes that gap.

## Amendment to the observability design

`docs/superpowers/specs/2026-08-12-observability-design.md` explicitly deferred distributed tracing spans: *"Distributed tracing spans (deferred by usage metering's own design; nothing here changes that)."* This slice reverses that deferral — tracing is now in scope, following the same pattern the security-hardening spec used to amend that document's metric catalog.

## Scope decisions (from brainstorming)

- **Logs → Loki via a `tracing-loki` layer in the app itself, not an external log-shipper container.** The standard Grafana-stack pattern (Promtail/Alloy tailing container stdout) needs the Podman socket enabled for container discovery in this rootless-Podman dev environment — extra host setup and a source of silent "shipper sees nothing" failures. Pushing logs directly from the app via a second `tracing` layer avoids that dependency entirely: one more service (`loki`) in compose, no shipper container, no socket permissions to get right. The existing stdout JSON logging is unchanged and stays the primary local-dev log view; the Loki push is additive.
- **Traces → OTel Collector → Tempo, not direct-to-Tempo OTLP.** Chosen over the simpler direct-export option for consistency with standard production topologies (a collector in front of the trace backend is the normal shape once more than one thing needs to consume span data), even though this is a dev-only stack today.
- **Log/trace correlation via a `trace_id` field on JSON log lines, not the OTel logs bridge (`opentelemetry-appender-tracing`).** That bridge is the least mature part of the Rust OTel ecosystem. Instead, a small custom `tracing_subscriber::Layer` reads the current span's OTel `trace_id` (via `tracing-opentelemetry`'s span extension / `OpenTelemetrySpanExt`) and records it as a `trace_id` field, which both the stdout JSON layer and the `tracing-loki` layer pick up like any other field. Loki's "derived fields" then turn that `trace_id` value into a link straight into the matching Tempo trace.
- **Instrumentation depth: HTTP + DB + sqld proxy calls.** `tower-http`'s `TraceLayer` for one span per request; `#[tracing::instrument]` on the DB-access functions; a child span around `proxy_handler`'s outbound call to sqld. Together these make a slow-request trace show, at a glance, whether time was spent in Postgres, in sqld, or in the gateway itself.
- **`OTEL_EXPORTER_OTLP_ENDPOINT` and `LOKI_URL` are both optional `Config` fields**, unlike `API_KEY_PEPPER`/`METRICS_TOKEN`. If unset, the app runs exactly as it does today — no forced collector/Loki dependency for `cargo test` or a bare `cargo run` outside podman-compose. `OTEL_EXPORTER_OTLP_ENDPOINT` reuses the OTel SDK's own standard env var name rather than inventing a gateway-specific one, since that's what `opentelemetry-otlp`'s exporter builder already reads by convention.
- **Drive-by fix: `prometheus.yml` still scrapes `gateway:8787`**, the pre-rename service hostname (the `gateway` → `wardn` compose service rename predates this slice). Prometheus's target has been silently down since that rename. Fixed here since this slice is already touching the compose file and every other observability config in it.

**Explicitly out of scope for this slice:**
- Sampling / trace retention policy tuning — Tempo runs with its defaults; a production deployment would need this, a dev stack does not.
- Grafana dashboards or alerting — same reasoning the metrics slice used: this slice makes the data available and browsable via Explore; building specific dashboards is an operational task, not a design one.
- Any change to the existing Prometheus metrics catalog — that stays exactly as the observability slice left it, aside from the unrelated hostname fix above.
- Client-side (SDK-consumer) trace context propagation — this slice covers the gateway process's own internal spans, not teaching hivemind's client library to propagate `traceparent` headers into the gateway.

## Architecture

### Local dev stack (`podman-compose.yml`)

Three new services, all internal-only (no host-published ports) — nothing outside the compose network needs to reach them directly; humans browse traces/logs through Grafana, not by hitting Loki/Tempo/the collector on the host:

- **`loki`** (`docker.io/grafana/loki:latest`) — log storage + query API. `wardn` and Grafana both reach it by service name (`http://loki:3100`).
- **`tempo`** (`docker.io/grafana/tempo:latest`) — trace storage + query API, OTLP receiver on its internal gRPC/HTTP ports. Only the collector talks to it directly.
- **`otel-collector`** (`docker.io/otel/opentelemetry-collector-contrib:latest`) — receives OTLP (gRPC) from `wardn`, exports OTLP to `tempo`. Config is a minimal receiver→exporter pipeline, no processors beyond the defaults.

`wardn`'s environment block gains:
```
OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-collector:4317
LOKI_URL: http://loki:3100
```

`grafana` gains two provisioned datasources (`grafana/provisioning/datasources/loki.yml`, `.../tempo.yml`), alongside the existing Prometheus one. The Tempo datasource enables `tracesToLogsV2` pointing at Loki (keyed on the `trace_id` field); the Loki datasource gets a derived field regex on `trace_id` pointing back at Tempo — this is what makes the Grafana Explore view clickable in both directions.

### Rust code changes

**`Cargo.toml`** — new dependencies (exact versions pinned via `cargo add` at implementation time, not hand-picked here): `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` (gRPC/tonic exporter), `tracing-opentelemetry`, `tower-http` (`trace` feature), `tracing-loki`.

**`src/config.rs`** — two new optional fields:
```rust
pub otel_exporter_otlp_endpoint: Option<String>,  // OTEL_EXPORTER_OTLP_ENDPOINT, no default
pub loki_url: Option<String>,                      // LOKI_URL, no default
```
Both read with `std::env::var(...).ok()`, not `.context(...)?` — absence is a valid, fully-supported configuration, not an error.

**`src/main.rs`** — subscriber construction changes from the current single `fmt().json()` call to a layered `tracing_subscriber::registry()`:
1. The existing JSON `fmt` layer (unchanged output shape, still honors `RUST_LOG`).
2. A small custom layer that stamps the current span's OTel `trace_id` onto a `trace_id` field, present on every layer downstream of it — this is what makes `trace_id` show up in both the JSON stdout logs and the Loki push.
3. `tracing_opentelemetry::layer()`, wired to an OTLP-exporting `TracerProvider` — only constructed when `config.otel_exporter_otlp_endpoint` is `Some`.
4. A `tracing-loki` layer/background task — only constructed when `config.loki_url` is `Some`.

Layers 3 and 4 are conditionally added (`Option<Layer>` composed via `.with(...)`, both are no-ops when `None`), so the subscriber degrades cleanly to today's behavior with neither env var set.

**`src/lib.rs`** — `app()` gains `tower_http::trace::TraceLayer::new_for_http()` (or a small custom `MakeSpan`/`OnResponse` if the default fields aren't descriptive enough) applied to the whole router, giving one span per request with method/path/status/latency.

**DB-access functions** — `#[tracing::instrument(skip(pool))]` added to each `pub async fn` that issues a `sqlx` query directly: `db::find_api_key_by_prefix`, `db::find_database_mapping`, and the equivalent query functions in `registration.rs`, `api_keys.rs`, `org/members.rs`, `org/admin.rs`. `skip(pool)` because `PgPool` isn't meaningfully loggable and would just add noise to every span.

**`src/proxy.rs`** — `proxy_handler` gets `#[tracing::instrument(skip(...))]` (skipping the request/body arguments the same way DB functions skip `pool`), and the outbound hyper call to sqld is wrapped in its own child span so a trace's waterfall shows time-in-sqld as a distinct segment from local gateway processing (auth, namespace resolution, response streaming).

### Data flow

```
wardn (per request)
  ├─ tower-http TraceLayer span (HTTP)
  │    ├─ #[instrument] DB span(s) (auth lookup, namespace lookup, ...)
  │    └─ #[instrument] sqld proxy span (outbound call)
  ├─ spans exported via OTLP → otel-collector → tempo
  └─ JSON log lines (stdout, unchanged) + tracing-loki push → loki
       both carry the same `trace_id` field

Grafana Explore
  ├─ Tempo datasource: browse/search traces, "logs for this trace" → Loki
  └─ Loki datasource: browse/search logs, derived field on trace_id → Tempo
```

### Drive-by fix

`prometheus.yml`'s `static_configs` target changes from `gateway:8787` to `wardn:8787`, matching the compose service's actual name.

## Testing

- Existing test suite (`cargo test`) must continue passing unmodified — no test constructs `AppState`/`Config` with the new env vars, and their absence must not be an error (see the optional-field scope decision above).
- `#[tracing::instrument]` additions are structural (no behavior change to the instrumented functions' logic) — no new unit tests required for them specifically.
- Manual verification via `podman-compose up`: hit a few endpoints, confirm spans appear in Tempo (Grafana Explore), confirm log lines appear in Loki with a `trace_id` field, confirm the derived-field jump from a Loki log line lands on the matching Tempo trace, confirm the Prometheus target is healthy again (fixed hostname).
