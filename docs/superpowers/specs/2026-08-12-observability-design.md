# hivewarden Observability — Design

**Status:** Approved. Sixth sub-project of `hivewarden`, built on the walking skeleton, org roles, database provisioning, org membership/API keys, and usage metering.

## Goal

Give whoever operates the gateway (not the future billing system — that's usage metering's job) enough runtime visibility to answer "is the service healthy right now" and "why is this slow/failing" before real traffic arrives. This is the deferred half of the original "usage metering + observability" backlog item, split from usage metering during that sub-project's brainstorming because the two have different consumers, different data shapes, and different urgency.

## Motivation

Pre-launch hardening. No specific incident is driving this — it's the last operational gap before the load-test/DR-validation gate.

## Scope decisions (from brainstorming)

- **Prometheus `/metrics` endpoint**, not spans or a push-based system. Standard pull-based counters/histograms/gauges via the `metrics` facade crate + `metrics-exporter-prometheus`. Works with Grafana/Prometheus/most cloud monitors out of the box; no new infra dependency beyond a scrape target. Distributed tracing spans remain out of scope, same as usage metering's design explicitly deferred them.
- **Full coverage: request-level, dependency health, and the provisioning worker.** Not just proxy traffic — also Postgres pool state, sqld reachability, and `namespace_provisioning_outbox` queue depth. The goal is "is the problem us, a dependency, or a stuck background job" answerable from one endpoint.
- **`GET /metrics` requires a static bearer token (`METRICS_TOKEN`)**, checked before rendering, superseding this slice's original brainstorming choice of "unauthenticated, Prometheus convention." Reversed during Task 2's review: the `namespace` label (see below) makes an unauthenticated `/metrics` a tenant-enumeration endpoint — any caller can list every tenant UUID, request volume, and latency profile, and anonymous `POST /users` can grow that per-tenant label set without bound. A shared-secret token is a small addition (one new `Config` field, one header check) and closes the enumeration-read path. It does **not** address the growth-vector concern — an unauthenticated caller can still repeatedly `POST /users` to grow the recorder's distinct-`namespace` label set, since the token only gates *reading* `/metrics`, not writing metrics — that risk remains separately tracked below ("No cap on distinct namespace values in this slice"). Distinct from the API-key system — this is a single deployment-wide secret, not a per-caller credential.
- **`namespace` is a label on proxy request metrics**, despite unbounded cardinality risk as tenant count grows. Chosen over the cardinality-safe alternative (protocol/status-class only) because per-tenant breakdown at a glance is worth more than the safety margin right now. **No cap on distinct namespace values in this slice** — accepted risk, consistent with this project's existing pattern of documenting known-but-not-yet-urgent risks (e.g. no rate limiting yet either) rather than over-engineering a mitigation before it's needed. Revisit if/when tenant count grows or rate limiting ships, whichever comes first.

**Explicitly out of scope for this slice:**
- Distributed tracing spans (deferred by usage metering's own design; nothing here changes that).
- Alerting rules or Grafana dashboards themselves — this slice makes the data available; building specific dashboards/alerts is whoever operates the deployed service's job, same as usage metering left pipeline consumption out of scope.
- Per-caller `/metrics` authentication — a single shared `METRICS_TOKEN` is in scope and implemented (see Scope decisions above); separately revocable, per-scraper credentials are not.
- Namespace-label cardinality capping (accepted risk, see above).
- Byte/bandwidth metrics — same reasoning usage metering used to exclude byte counting (would require wrapping the streaming body in `proxy.rs`).

## Architecture

### Crate and wiring

`metrics` (facade) + `metrics-exporter-prometheus`. The Prometheus recorder is installed **once, in `main.rs`** — not inside `app()` or `AppState::new` — specifically so that test binaries building `AppState` directly (without calling `main`) never trigger a second global-recorder install, which panics. `main.rs` builds the recorder and its render handle together, then threads the handle into `AppState` (e.g. `AppState::new(...).with_metrics_handle(handle)`) so the `/metrics` route handler can render it. Test code that constructs `AppState` without a real handle gets a locally-built, non-globally-installed handle instead (`PrometheusBuilder::new().build_recorder()`, not `.install_recorder()`) — `metrics::counter!`/`histogram!`/`gauge!` macro calls become safe no-ops without a global recorder, and the route itself still renders whatever *is* recorded through that local handle when a test exercises it directly.

### `GET /metrics`

Registered alongside `/healthz` in `src/lib.rs`'s `app()`, outside the `auth_middleware` layer. Because it sits outside that layer, the handler validates its own `Authorization: Bearer <METRICS_TOKEN>` header before doing anything else — fail-closed if `METRICS_TOKEN` is unset (an empty `state.metrics_token`, `AppState::new`'s default, rejects every request). Only once that check passes does it render `AppState`'s `PrometheusHandle` to the standard Prometheus text exposition format. Before rendering, it also refreshes the two Postgres pool gauges (see below) from `PgPool::size()`/`num_idle()` — cheap, synchronous, no extra query, so doing it at scrape time rather than on a timer adds no meaningful cost.

### Metric catalog

**Amendment (security hardening, `docs/superpowers/specs/2026-08-13-security-hardening-design.md`):** the `namespace` label originally on `gateway_proxy_requests_total`/`gateway_proxy_request_duration_seconds` was removed — combined with this endpoint's per-tenant traffic data, it created an unbounded, externally-triggerable (via anonymous `POST /users`) memory-growth vector in the metrics recorder. Per-tenant attribution stays available through the `usage`-target tracing events.

| Name | Type | Labels | Emitted where |
|---|---|---|---|
| `gateway_proxy_requests_total` | counter | `protocol` (`query`/`sync`), `status_class` (`2xx`/`4xx`/`5xx`) | `proxy_handler` (`src/proxy.rs`), same point as `emit_usage_event` |
| `gateway_proxy_request_duration_seconds` | histogram | `protocol` | same point |
| `gateway_proxy_requests_in_flight` | gauge | (none) | RAII guard struct, incremented at `proxy_handler` entry, decremented in its `Drop` impl — covers every early-return path (401/403/404/504/502/200) without instrumenting each one individually |
| `gateway_pg_pool_size` | gauge | (none) | computed at `/metrics` scrape time from `PgPool::size()` |
| `gateway_pg_pool_idle` | gauge | (none) | computed at `/metrics` scrape time from `PgPool::num_idle()` |
| `gateway_sqld_up` | gauge (0/1) | (none) | periodic background task in `main.rs`, own interval (decoupled from scrape cadence — `/metrics` must never block on a network call to sqld) |
| `gateway_provisioning_outbox_pending` | gauge | (none) | updated once per `run_worker` tick (`src/provisioning.rs`), reusing its existing interval and a `COUNT(*) WHERE status = 'pending'` query |
| `gateway_provisioning_outbox_failed` | gauge | (none) | same tick, `COUNT(*) WHERE status = 'failed'` |
| `gateway_provisioning_attempts_total` | counter | `outcome` (`success`/`failure`) | `attempt_provisioning` (`src/provisioning.rs`) |
| `gateway_provisioning_worker_run_duration_seconds` | histogram | (none) | wraps one full tick of `run_worker`'s per-tick batch loop |

Only `namespace` carries per-tenant cardinality; every other label is a small fixed set.

### Local dev stack (`podman-compose.yml`)

Two new services, both loopback-bound like every existing service in this file:

```yaml
prometheus:
  image: docker.io/prom/prometheus:latest
  volumes:
    - ./prometheus.yml:/etc/prometheus/prometheus.yml:ro,Z
  ports:
    - "127.0.0.1:9090:9090"
  depends_on:
    - gateway

grafana:
  image: docker.io/grafana/grafana:latest
  volumes:
    - ./grafana/provisioning:/etc/grafana/provisioning:ro,Z
  environment:
    GF_AUTH_ANONYMOUS_ENABLED: "true"
    GF_AUTH_ANONYMOUS_ORG_ROLE: Viewer
  ports:
    - "127.0.0.1:3000:3000"
  depends_on:
    - prometheus
```

New files:
- `prometheus.yml` — scrape config targeting `gateway:8787` (compose's internal DNS, matching how `postgres`/`sqld` are already addressed by the `gateway` service in this same file).
- `grafana/provisioning/datasources/prometheus.yml` — auto-provisions Prometheus (`http://prometheus:9090`) as Grafana's datasource, so `podman-compose up` gives a ready-to-query Grafana with no manual UI setup. No pre-built dashboards — dashboard authoring is out of scope (see above).

Grafana's anonymous viewer access is local-dev-only, matching this file's existing "unauthenticated dev services, loopback only" posture for `postgres` and `sqld`.

## Testing

- Integration test: proxy a request, then hit `/metrics`, assert the rendered text contains `gateway_proxy_requests_total` with the expected label values present (text-match against Prometheus exposition format — matches this project's "test real behavior" bar without pulling in a Prometheus client parser just for tests).
- Integration test: assert `gateway_proxy_requests_in_flight` returns to `0` after a completed request, and separately after a rejected (401) request — proves the RAII guard's `Drop` fires on every exit path, not just the success path.
- Worker-metrics test: reuse the existing short-interval `run_worker` test pattern (already used for provisioning), assert `gateway_provisioning_outbox_pending`/`_failed` and `gateway_provisioning_attempts_total` reflect a seeded outbox row's fate after one tick.
- `gateway_sqld_up` test: point the background health-check task at a real sqld test instance (expect `1`) and separately at an unreachable address (expect `0`), on a short interval like the worker tests already use.

## Non-goals / risks carried forward

- No alerting rules or dashboards shipped — this slice makes data available; consuming it is whoever operates the deployed service's job.
- No cap on `namespace` label cardinality — accepted risk, revisit if/when tenant count grows or rate limiting ships.
- No per-caller `/metrics` authentication (a single shared `METRICS_TOKEN`, not per-scraper credentials) — sufficient for one Prometheus scrape target; revisit if multiple distinct consumers need separately revocable access.
- No distributed tracing spans — deferred by usage metering's design, unchanged here.
