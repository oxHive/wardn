# wardn Security Hardening — Design

**Status:** Approved. Seventh sub-project of `wardn` (formerly `hivemind-gateway`), built on the walking skeleton, org roles, database provisioning, org membership/API keys, usage metering, and observability.

## Goal

Fix the Critical and all ten Important findings from a full-codebase security/quality audit (`.superpowers/sdd/` transcript, not committed — see this spec's per-finding sections for the substance). The audit found no cross-tenant data-exposure path and confirmed both of this project's historical vulnerabilities remain fixed; every finding here is about resource governance, permission-model precision, and defense-in-depth around that already-solid core.

## Scope decisions (from the audit review)

- **Critical + all 10 Important findings, in one plan.** Minor/optimization findings (TOCTOU on `delete_role`/`add_member`, missing CHECK on the outbox table, unindexed outbox scans, `MissedTickBehavior`, per-request URI/header reconstruction, N+1 in `list_roles`, no graceful shutdown, Docker running as root) are explicitly deferred to a later pass — none are exploitable today and none block the fixes below.
- **Finding #1 (argon2 DoS) fix: full HMAC-SHA256 migration, no backward-compatibility shim.** This is a brand-new project with no real issued API keys to preserve (confirmed by the user) — so there's no need for a dual-verification fallback path or a rehash-on-use migration. `argon2` is dropped entirely; every key is hashed and verified with HMAC-SHA256 from this point on. Anyone holding a key issued before this change will need a new one (acceptable — dev-only data, `scripts/reset-dev-db.sh` already exists for exactly this kind of reset).
- **Finding #11 (user enumeration on `POST /users`) gets no dedicated fix.** The audit itself calls this "a real tradeoff, not an oversight" — the alternative (identical response + out-of-band key delivery by email) is a materially bigger change (requires an email-sending integration this project doesn't have) that doesn't belong in a hardening pass. The rate limiting added for finding #3 is the accepted interim mitigation (caps enumeration throughput); this stays as a documented accepted risk, same tier as the namespace-cardinality risk already accepted in the observability spec.
- **Finding #5's "wire an alert" note gets no dedicated code task.** There's no alerting system in this project yet (Prometheus/Grafana are wired for manual dashboards only, per the observability spec's explicit non-goal on alerting rules). The `gateway_provisioning_outbox_failed` gauge already exists; actually alerting on it is out of scope here, same as it was out of scope for the observability slice.

## Per-finding architecture

### 1. API key hashing: argon2 → HMAC-SHA256 (Critical)

**Problem:** `Argon2::default()` (m=19MiB, t=2) runs synchronously on the async runtime on every request, including the `DUMMY_HASH` equalization path for unknown prefixes. Unauthenticated, unbounded-concurrency callers can exhaust memory and stall every tenant's traffic on the same worker threads.

**Fix:** API keys are 32 random characters from a 62-symbol alphabet (`generate_api_key`, `src/auth.rs`) — ~190 bits of entropy. A slow, memory-hard KDF defends low-entropy human-chosen secrets against offline cracking; against 2^190 it buys nothing measurable, so there's no security reason to keep it slow. Replace with keyed HMAC-SHA256:

- New required config: `API_KEY_PEPPER` (a server-side secret, distinct from any individual key, folded into every hash so a leaked `key_hash` column alone doesn't let an attacker forge valid keys without also knowing the pepper). Read via `Config::from_env()`, same `.context(...)`-required pattern as `DATABASE_URL`/`METRICS_TOKEN`. Threaded to wherever hashing happens the same way `metrics_token` is threaded through `AppState`.
- `generate_api_key(pepper: &[u8]) -> (String, String, String)` — unchanged random-generation logic; the hash becomes `hex::encode(Hmac::<Sha256>::new_from_slice(pepper).chain_update(full_key).finalize().into_bytes())`.
- `verify_key(pepper: &[u8], full_key: &str, hash: &str) -> bool` — recompute the HMAC and compare via `Mac::verify_slice`, which is constant-time internally (no separate `subtle` dependency needed for this comparison — the `hmac` crate's own verification API is designed for exactly this).
- `DUMMY_HASH`'s role changes from "an argon2 hash that takes the same CPU to verify" to "a fixed HMAC tag that can never match" — still exists, still computed on every unknown-prefix path, for the same reason (equalize DB-hit vs DB-miss timing), just cheap now instead of expensive. That's fine: the goal was never to make the miss path *slow*, only to make it *not observably faster* than the hit path, and a ~1µs HMAC on both paths satisfies that with a much smaller absolute timing budget for an attacker to work with in the first place.
- `Cargo.toml`: remove `argon2`, add `hmac` + `sha2` + `hex` (or reuse the existing `base64`-adjacent encoding approach — final call left to the implementer, `hex` is more conventional for a fixed-length digest).
- Every test call site (~14 files) gets a shared `common::TEST_API_KEY_PEPPER` constant, matching the existing `TEST_METRICS_TOKEN` pattern in `tests/common/mod.rs`.
- `.env.example` gets `API_KEY_PEPPER=` documented alongside `METRICS_TOKEN`.

### 2. Permission escalation: `db:sync` implies `db:query` (Important)

**Problem:** `proxy_handler` (`src/proxy.rs:193-197`) picks the required permission from the *client's chosen HTTP version*, not from what's actually being requested. sqld's Hrana/SQL endpoints work over both HTTP/1.1 and HTTP/2; its gRPC replication endpoints work over HTTP/2 only. A member holding only `db:sync` can send a Hrana SQL request over h2c and the gate — seeing HTTP/2 — asks for `db:sync`, which they have, granting full read/write SQL despite never being granted `db:query`.

**Fix:** gate on the actual outbound protocol (the request path), not the transport version:
- A small constant listing sqld's gRPC path prefixes (`/wal_log.`, `/proxy.` — verified against the real container in this project's earlier work).
- If the path matches a gRPC prefix, required permission is `db:sync`; otherwise `db:query`, regardless of HTTP version.
- A gRPC path arriving over HTTP/1.1 is rejected outright (400) rather than forwarded and left for sqld to reject — sqld already does reject it, but rejecting at the gateway avoids depending on that behavior remaining true across sqld versions.
- New regression test: a `db:sync`-only member sending a Hrana request over h2c must get 403.

### 3. `POST /users` hardening, part 1: email validation + normalization (Important)

**Problem:** `req.email` reaches the `INSERT` with no format check, no length cap, no case normalization. `users.email`'s `UNIQUE` constraint is case-sensitive, which is also the root cause of the case-variant-account ambiguity `org/members.rs`'s `add_member` already has to defend against with a `fetch_all` + refuse-to-guess branch (from the org-membership sub-project's own final review).

**Fix:**
- Validate before the insert: non-empty, `len() <= 254` (RFC 5321), exactly one `@` with non-empty local/domain parts, no control characters or whitespace. Reject with 400 on failure.
- Normalize to lowercase at the same point.
- Migration: `CREATE UNIQUE INDEX ... ON users (lower(email))`, replacing (or alongside — implementer's call based on what the existing constraint actually is) the current case-sensitive `UNIQUE`.
- This also lets `org/members.rs`'s `add_member` simplify back to a direct `fetch_optional` lookup instead of its current `fetch_all` + 3-way match, since case-variant duplicate accounts become impossible going forward — but only for *new* registrations; the implementer should check whether any case-variant duplicates already exist in the dev DB before tightening the constraint, and leave `add_member`'s existing defensive code in place rather than removing it (removing a known-good defensive check isn't part of this fix; it can be revisited once the invariant is actually guaranteed at the DB level for all rows, not just new ones).

### 4. `POST /users`/`POST /orgs` hardening, part 2: rate limiting + org quota (Important)

**Problem:** No rate limit anywhere in the gateway. `POST /users` is public and each call provisions a real sqld namespace (disk) plus an argon2/HMAC hash (CPU) plus an org quota of zero (unlimited orgs per user, since `create_org` has no cap).

**Fix:**
- A per-source-IP rate limit on `POST /users`, via `tower_governor` (the standard `tower`-ecosystem choice for this — new dependency). Limit: 5 requests/hour/IP, matching the audit's suggestion. **Not applied to `POST /orgs`**: that endpoint is authenticated, so a per-user quota (below) is the more precise defense — an IP-based limit would also throttle legitimate customers sharing a NAT/office IP, which the quota doesn't, since it keys on identity rather than network address.
- A cap on orgs created per user — a straightforward `COUNT(*)` check against `org_members` (owner role) before `create_org` proceeds, returning 429 once a small fixed ceiling is hit (implementer picks a reasonable default, e.g. 10, since this project has no plan/billing tiers yet to derive the number from). This is `/orgs`'s only rate-limiting defense — see above.
- Global concurrency isn't capped by this task — that's a broader "add a load-shedding layer to the whole gateway" concern the audit didn't scope tightly enough to fix here without guessing at a number; left as a future consideration.

### 5. Metrics cardinality: drop `namespace` label from proxy metrics (Important)

**Problem:** `gateway_proxy_requests_total` and `gateway_proxy_request_duration_seconds` (`src/observability.rs`) carry a `namespace` label. Every namespace is permanently retained in the Prometheus recorder's memory with no eviction, and every namespace is created by a `POST /users` call — which, before finding #4's rate limit, was uncapped. This reverses the observability sub-project's original explicit choice to include this label.

**Fix:**
- `record_proxy_metrics` drops the `namespace` parameter and label entirely from both the counter and the histogram. Per-tenant attribution stays available through the `usage`-target tracing events (`src/proxy.rs`'s `emit_usage_event`), which already carry `namespace`/`owner_id`/`org_id` and are designed for exactly this kind of high-cardinality data.
- The observability design spec (`docs/superpowers/specs/2026-08-12-observability-design.md`) gets a short amendment noting the reversal and pointing to this spec for why.
- Existing tests in `tests/observability_test.rs` that assert on the `namespace` label for these two metrics need updating to assert their absence instead (or just drop the namespace-specific assertion, keeping the rest of the test).

### 6. Provisioning idempotency (Important)

**Problem:** `attempt_provisioning` (`src/provisioning.rs`) treats any non-2xx from sqld's admin API as a failure. If the namespace already exists (sqld's create returns 400) but the `database_mappings` row was never written — a gateway restart between sqld's 200 and the transaction commit, a Postgres hiccup after a successful create, or the admin request timing out after sqld already finished — every retry fails identically forever, and the tenant's key resolves to a permanent 404.

**Fix:**
- On a non-success response, follow up with `GET /v1/namespaces/{ns}` against the admin API. A 200 there means the namespace genuinely exists; treat that the same as the original create having succeeded and proceed to the same transaction (mapping insert + outbox row marked `done`).
- That transaction's `INSERT INTO database_mappings` becomes `... ON CONFLICT (owner_type, owner_id) DO NOTHING`, so a retry after a partially-applied state doesn't fail the whole transaction on the existing `UNIQUE` constraint.
- A non-200 from the existence check falls through to the existing `record_failure` path unchanged.

### 7. Namespace isolation defense-in-depth (Important — combines two related findings)

Two related gaps in the same area, fixed together since they're both "the code trusts an invariant it doesn't verify":

**7a. `resolve_namespace` (`src/routing.rs`) has no permission check.** It's correct today only because every key currently minted has `owner_type = "user"` (hardcoded at both mint sites) — but the schema already permits `"workspace"`/`"org"`-owned keys, and `resolve_namespace` would happily resolve one straight to a shared namespace with the entire role/permission system bypassed, the moment such a key ever exists. Fix: reject anything that isn't `owner_type == "user"` with 403, forcing a conscious decision if org/workspace-owned keys are ever introduced instead of silently opening a hole.

**7b. The namespace selector fails open.** If `x-namespace-bin` were ever absent on the outbound h2 request (a future refactor bug, a header-size limit, an early-return regression), sqld's authority-based fallback resolves to a shared `default` namespace rather than erroring — silent cross-tenant contamination, not a rejected request. Fix: immediately before `client.request(outbound)` in `proxy_handler`, assert the outbound `HeaderMap` actually contains `x-namespace-bin` set to the expected encoding of the resolved namespace; return 500 if not. This turns a hypothetical silent leak into a loud, immediate failure if it's ever reintroduced. (The audit also suggested an sqld startup flag to disable the implicit `default` namespace entirely — that's a deployment/infra concern for `podman-compose.yml`/production config, not a code change, and depends on flag support in the pinned libsql-server version; add a one-line note to the README's deployment section rather than a guaranteed config change, since it can't be verified without testing against the exact pinned version.)

### 8. Last-admin lockout guard (Important)

**Problem:** `update_role` (`src/org/admin.rs`) lets any `org:manage_roles` holder strip that same permission from their own role, and `remove_member` (`src/org/members.rs`) lets any `org:manage_members` holder remove the org's last member — either way, permanently locking the org out of managing itself with no recovery path over the API (same unrecoverable-without-SQL tier this project has already accepted for the *initial* last-admin gap in org-roles, but this one is reachable through completely ordinary use, not just an edge case at org creation).

**Fix:** before committing `update_role`, `assign_member_role`, or `remove_member`, verify — inside the same transaction — that at least one org member will still hold `org:manage_roles` after the change. Return `409 CONFLICT` with a clear message if the change would leave zero. Query shape: join `org_members` to `role_permissions` (or the role being assigned/removed), counting distinct members (excluding the one being acted on, where applicable) who'd still hold the permission.

### 9. DB round-trip reduction (Important)

**Problem:** An org-scoped proxy request costs 3-4 Postgres round-trips before any byte reaches sqld (`find_api_key_by_prefix`, the member→role lookup, the role→permissions lookup, `find_database_mapping`), plus the now-cheap-but-still-present HMAC verify. `db::connect` uses sqlx's bare defaults (10 max connections, 30s acquire timeout) — ten concurrent DB-bound requests saturate the pool and the eleventh queues for up to 30 seconds before failing.

**Fix, scoped to what's safe without introducing new correctness risk (a caching layer's invalidation logic is explicitly out of scope for this pass — real value, real risk, deserves its own future sub-project rather than being rushed into a hardening pass):**
- Collapse the member→role and role→permissions queries (`src/roles.rs`) into one `LEFT JOIN`, distinguishing "not a member" (no rows) from "member with no permissions" (one row, `permission IS NULL`) to preserve the existing `Option<HashSet<Permission>>` contract.
- `db::connect` gets explicit pool configuration: `PgPoolOptions::new().max_connections(32).acquire_timeout(Duration::from_secs(3))` (or values the implementer judges reasonable for this project's current scale — the point is *explicit*, not the exact numbers) instead of sqlx's bare defaults, so saturation produces a fast, clear failure instead of a 30-second hang.

### 10. Constant-time comparison + minimum length for `METRICS_TOKEN` (Important)

**Problem:** `metrics_handler`'s bearer-token check (`src/observability.rs`) uses `!=` on `&str`, which short-circuits on the first differing byte — the one non-constant-time secret comparison in the codebase (everywhere else already handles this correctly). Separately, `config.rs`'s existing non-empty check lets `METRICS_TOKEN=a` boot successfully.

**Fix:**
- Add the `subtle` crate; compare `Sha256` digests of the presented and expected tokens via `ConstantTimeEq`, which also hides length differences (a raw byte-slice `ct_eq` on unequal-length inputs isn't meaningfully constant-time without the digest step).
- `config.rs`'s existing empty-string check gets a minimum-length requirement (implementer's call on the exact number — 32 is a reasonable floor for a bearer token) alongside it.

## Global Constraints (carried from the project's established conventions)

- Rust edition 2024, axum 0.8, sqlx 0.8 — runtime-checked queries only (`sqlx::query`/`query_as`, never `query!`/`query_as!`).
- `#[tokio::test]` stays the bare, single-threaded-runtime attribute in every test file.
- Integration tests exercise real Postgres/sqld — no mocks.
- New required env vars (`API_KEY_PEPPER`) follow the existing `Config::from_env()` `.context(...)`-required pattern, documented in `.env.example`.
- Any change touching `src/proxy.rs` (findings #2, #7b) gets the strictest review bar this project applies — that file has a documented history of two prior Critical cross-tenant vulnerabilities.

## Non-goals for this pass

- Minor/optimization findings from the audit (listed in Scope decisions above) — deferred, not fixed here.
- Finding #11 (user enumeration) — accepted risk, mitigated by finding #4's rate limit, not separately fixed.
- Caching layer for namespace/permission lookups — real future value, deliberately out of scope to avoid rushing an invalidation-correctness risk into a hardening pass.
- Global request concurrency limiting / load shedding — noted by the audit, not scoped tightly enough here to implement without guessing at numbers this project has no data to derive.
- sqld's `default`-namespace-disabling startup flag — deployment/infra note only, not a code change (see finding #7b).
- Any backward-compatibility path for API keys issued before finding #1's HMAC migration — explicitly not needed per the human's decision (no real issued keys exist yet).
