# hivemind-gateway Usage Metering — Design

**Status:** Approved. Fifth sub-project of `hivemind-gateway`, built on the walking skeleton, org roles, database provisioning, and org membership/API keys.

## Goal

Emit a structured usage event for every proxied request that actually reaches sqld, so a future billing system has the raw data it needs to aggregate usage per tenant. This is the first half of the "Usage metering + observability" backlog item — split from observability (tracing/metrics for operational health) during brainstorming, since the two have different consumers (a future billing system vs. whoever runs the service), different data shapes, and different urgency. Observability is a separate future sub-project.

## Scope decisions (from brainstorming)

- **Data-plane proxy traffic only.** `proxy_handler` (`src/proxy.rs`) is the only emission point. Control-plane admin operations (org creation, role changes, key issuance) aren't metered by this slice — "usage" here means actual database traffic, the thing a database-as-a-service bills for.
- **Only requests that reach sqld are events.** A request rejected at auth, permission, or namespace resolution (401/403/404) never consumed database resources and isn't a billable event. Only requests that got proxied and received a response emit a usage event.
- **Structured `tracing` events, not a new Postgres table.** One `tracing::info!(target: "usage", ...)` per completed proxied request. No new DB writes on the hot path, no new table, no new migration. Consuming/aggregating these events (shipping them to a billing pipeline) is explicitly out of scope for this slice — it emits well-structured data; a future sub-project decides how that data gets consumed. This matches the pattern used by every prior slice in this project (`namespace_provisioning_outbox`'s worker is the one exception, and that had a concrete, immediate consumer already specified — billing aggregation does not yet).
- **Counts only, no byte counting.** Fields describe *what kind* of request happened (owner, org, namespace, protocol, status) and *when*, not payload size. Adding byte counts would require wrapping the currently-unbuffered streaming body in `proxy.rs` — real complexity for a dimension nothing downstream consumes yet.
- **All logs switch to JSON output**, not just usage events. One-line change to the existing `tracing_subscriber` setup (`tracing-subscriber`'s `json` feature). Every log line becomes NDJSON (one JSON object per line) — the standard input format for essentially every log aggregator (Vector, Fluentd, Loki, CloudWatch Logs, Datadog). The `target: "usage"` field is what lets a future pipeline filter usage events out from ordinary operational logs; the structured fields land nested under a `fields` key per line (standard `tracing`-crate JSON shape), which a consuming pipeline flattens with one normalization step — not this project's concern to build.

**Explicitly out of scope for this slice:**
- Actually wiring up a log aggregation/shipping pipeline. This slice emits events; consuming them is future infra work.
- Byte/bandwidth counting.
- Metering control-plane (admin API) operations.
- Anything observability-flavored: latency percentiles, error-rate dashboards, distributed tracing spans, metrics endpoints (`/metrics`, Prometheus, etc.) — that's the separate observability sub-project.
- Persisting or querying usage data within the gateway itself. There is no `GET /usage` endpoint and no billing calculation anywhere in this slice.

## Architecture

### Event shape

One `tracing::info!` call, with `target: "usage"`, emitted from `proxy_handler` right before returning the final response — but only on the path where a response actually came back from sqld (not on any early-return rejection path). Fields:

| Field | Type | Meaning |
|---|---|---|
| `owner_type` | string | The authenticated caller's owner type (`"user"`, `"workspace"`, or `"org"`) |
| `owner_id` | string (UUID) | The authenticated caller's id |
| `org_id` | string (UUID) | Always present: the org id when the request used `X-Org-Id` (org-shared namespace access), the empty string otherwise. Emitting the key unconditionally keeps the event schema stable for a downstream parser, which an optional key would not |
| `namespace` | string | The resolved sqld namespace the request was proxied to |
| `protocol` | string | `"query"` (Hrana/HTTP1.1) or `"sync"` (h2c/gRPC replication) — mirrors the existing `db:query`/`db:sync` permission split |
| `status` | number | The HTTP status code returned to the client |
| `duration_ms` | number | Time from request entry to response headers being ready — consistent with the streaming body (this is not full-transfer duration) |

### Emission point

`src/proxy.rs`'s `proxy_handler` already computes every one of these fields by the time it builds its final `Response` — `owner` (from the `AuthedOwner` extension), the resolved `namespace`, `org_id` (from `org_id_header`, when present), and `outbound_version` (which determines `protocol` the same way it already picks the outbound client). Capture `std::time::Instant::now()` at function entry; compute `duration_ms` from that at the emission point. Emit immediately before returning the response built from the upstream reply — i.e., after the `tokio::time::timeout`-wrapped `client.request(outbound)` call succeeds (or times out — a `GATEWAY_TIMEOUT` still reached sqld's connection attempt and is worth counting; a request that never got that far, e.g. rejected by `require_permission`, is not).

### Logging setup

`Cargo.toml`: add the `json` feature to the existing `tracing-subscriber` dependency.

`src/main.rs`: change `tracing_subscriber::fmt::init()` to `tracing_subscriber::fmt().json().init()`.

This is a global change — every existing `tracing::error!`/`tracing::info!` call in the codebase starts emitting JSON instead of the current human-readable format. No call site needs to change; only the initialization.

## Testing

- A unit or integration test asserting that a successful proxied request through the full router produces a `tracing` event with `target: "usage"` and the expected field set — using `tracing`'s test-subscriber capture utilities (e.g. `tracing_test` or a custom `tracing::subscriber::with_default` + a capturing layer) rather than parsing stdout, matching this project's existing preference for testing real behavior over string-matching output.
- A test proving a rejected request (invalid key, missing permission, unknown namespace) does *not* emit a usage event — the negative case that makes "only requests that reach sqld are billable" a real, tested property rather than just a stated intent.
- A test distinguishing `protocol: "query"` vs `protocol: "sync"` — reusing the existing h2c/Hrana test infrastructure already present in `tests/grpc_proxy_test.rs`/`tests/proxy_test.rs`.
- A test confirming `org_id` is present when `X-Org-Id` was used and absent otherwise.

## Non-goals / risks carried forward

- No cost/pricing logic anywhere — this slice produces raw counting data, not bills. Turning usage events into an actual invoice is the future billing sub-project's job, and it doesn't exist yet.
- No safeguard against usage-event volume becoming a logging cost/throughput problem at scale — every proxied request writes one line to stdout via `tracing`. Pre-launch, no evidence this matters yet; if it does, that's an operational tuning problem (log sampling, async writers) for whoever operates the deployed service, not a gateway code change.
- Switching to JSON logs makes local `podman-compose logs` output less human-readable during development. Accepted trade-off — matches the "structured, aggregator-compatible logs" goal this slice exists to deliver.
