# wardn Walking Skeleton — Design

**Status:** Approved. First sub-project of `wardn`. Decomposed from the full gateway scope described in `hivemind-vs-gateway-split.md` (the boundary-map doc — the full `HIVEMIND_GATEWAY_SPEC.md` it references does not exist on this machine; this spec works from the boundary map's "Gateway Development Checklist" and its own judgment for anything the checklist doesn't cover).

## Goal

The minimum gateway that can authenticate a request, resolve which tenant it belongs to, and proxy it to the right database — nothing else. This is the "authenticate → authorize → resolve database → proxy" loop the boundary-map doc calls the gateway's core design constraint, built as a real, testable walking skeleton rather than a stub.

Directly unblocks the org-layer feature just shipped in `hivemind` (`[org_sync]`): once this exists, a paid org has something real to point `remote_url` at.

## Scope

- Postgres control plane schema: `users`, `api_keys` (argon2-hashed, revocable, FK to user), `workspaces`, `orgs`, `org_members` (user_id, org_id, role — schema exists now; role *enforcement* is a later sub-project), `database_mappings` (owner type + id → sqld namespace name).
- Auth middleware: validates `Authorization: Bearer <api_key>` against the argon2 hash in Postgres. Invalid/revoked/missing key → 401.
- Routing: resolves the authenticated request's owner (user/workspace/org, whichever `database_mappings` row matches) to a sqld namespace name.
- Transparent reverse-proxy: forwards the request to the shared sqld instance with the namespace header set (sqld's native multi-tenancy — one sqld process, many logical databases selected per-request via a namespace header). Gateway never parses libsql/Hrana payloads; it reads only enough of the request (the auth header) to authenticate, then streams the rest through untouched in both directions.

**Explicitly out of scope for this slice** (each is a future sub-project):
- Org role enforcement (admin/member/read_only checks) — schema supports it, nothing checks it yet.
- Rate limiting, billing/plan enforcement, Stripe integration.
- Admin API / user creation endpoints — test data goes in via SQL migration seeds / `psql` directly, not via any gateway endpoint.
- Database provisioning automation (creating new sqld namespaces on signup) — namespaces for this slice's tests are created manually.
- Usage metering, observability beyond basic request logging.
- Multi-gateway client support (v2, per `hivemind-v2-multi-gateway.md` — requires zero gateway changes anyway).

## Architecture

### Tech stack

- **axum** (0.8, matching `hivemind`'s own dependency) for the HTTP server and middleware.
- **sqlx** (Postgres, async, compile-time-checked queries) for the control plane.
- **argon2** crate for api_key hashing — keys are generated as `hm_live_<random>`/`hm_test_<random>` (following the `api_key = "hm_live_..."` format already shown in `hivemind-vs-gateway-split.md`), hashed at creation time, only the hash stored.
- A minimal HTTP client (`hyper`/`reqwest`, whichever pairs more simply with axum's body streaming) for the outbound proxy leg to sqld — this is a byte-for-byte forward, not a libsql client.

### Request flow

1. Client (`hivemind`'s libsql embedded-replica sync, or a raw Hrana client for testing) sends an HTTPS request to the gateway with `Authorization: Bearer hm_live_...`.
2. Auth middleware extracts the bearer token, looks up its argon2 hash in `api_keys`, rejects (401) if missing/revoked/hash-mismatch. On success, attaches the resolved `user_id` (and `owner_type`/`owner_id` — see below) to the request's extensions for downstream handlers.
3. Routing looks up `database_mappings` for the authenticated owner (a key belongs to exactly one owner: a user directly, or — for org-layer traffic — an org, determined by which `remote_url`/key the client used; `hivemind`'s existing `[sync]` vs `[org_sync]` split on the client side already keeps these as separate keys/connections, so the gateway doesn't need to disambiguate an ambiguous key — each key maps to exactly one namespace). Missing mapping → 404 (or 403 — decided in the plan, not load-bearing here).
4. Proxy layer rewrites the request's namespace header/path to the resolved sqld namespace, forwards the request body as-is to the shared sqld instance, streams the response back to the client unmodified (status, headers, body).

### Data model

```sql
users            (id, email, created_at)
api_keys         (id, user_id FK, key_hash, prefix, revoked_at NULL, created_at)
workspaces       (id, owner_user_id FK, name, created_at)
orgs             (id, name, created_at)
org_members      (org_id FK, user_id FK, role, created_at)   -- role: admin | member | read_only, unenforced this slice
database_mappings (id, owner_type ('user'|'workspace'|'org'), owner_id, sqld_namespace, created_at)
```

Exact column types/constraints/indexes are a plan-level detail, not a design-level one — the plan writes the real migration SQL.

### Testing

Integration tests run against a real local sqld (self-hosted, matching how `hivemind`'s own org-layer testing was done this session — no gateway needed for that, but this slice needs one) and a real test Postgres (via a `sqlx` test pool / testcontainers-style setup, decided in the plan). No mocking the proxy path — a test that asserts "request with a valid key for namespace X reaches sqld's namespace X and gets a real response back" is the actual thing this slice needs to prove. Auth accept/reject (valid, revoked, malformed, missing key) tested against the middleware directly.

### Non-goals / risks carried forward

- This gateway is intentionally "dumb" about memory content — if a future need requires inspecting or transforming memory data in flight, that's a signal the design has drifted into `hivemind`'s territory (per the boundary-map doc's explicit warning) and should be reconsidered, not built here.
- No HA/failover design in this slice — single gateway instance, single sqld instance. Load test + DR validation is explicitly listed in the boundary-map checklist as a pre-launch gate, not a walking-skeleton concern.
