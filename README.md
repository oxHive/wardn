# hivemind-gateway

The commercial/paid layer for [`hivemind`](https://github.com/oxhive/hivemind) — a
memory MCP server for AI coding agents. `hivemind` never knows this exists: the
same client binary runs for free (local/self-hosted) and paid (gateway-fronted)
users, differing only in `remote_url`/`api_key` config.

**What this does, and only this:** authenticate an API key → resolve the
authenticated owner (user/workspace/org) to a sqld namespace → transparently
proxy the request there. It never parses memory content or MCP protocol —
see `docs/superpowers/specs/2026-08-11-walking-skeleton-design.md` for the
full design and the boundary this is built to respect.

**Status:** walking skeleton, plus org role enforcement and self-service
database provisioning (`POST /users` and `POST /orgs` now create their own
sqld namespaces — no more hand-seeding). Billing, rate limiting, a general
admin API, and observability/alerting don't exist yet — see the memory entries
tagged `project:hivemind-gateway` for the current state and plan, or
`docs/superpowers/plans/2026-08-11-walking-skeleton.md` for how this was built.

## Quickstart

```sh
podman-compose -f podman-compose.yml up -d --build
```

Brings up Postgres, sqld (with namespaces + admin API enabled), and the
gateway itself, all bound to `127.0.0.1` only. The gateway applies its
Postgres migrations automatically on first connect — nothing else to run.

```sh
curl http://127.0.0.1:8787/healthz   # -> ok
```

### Registering a user, key, and namespace

`POST /users` is public — no `Authorization` header, because this is how a
caller gets their first API key at all. One call creates the user, mints a
personal API key, and provisions that user's own sqld namespace:

```sh
curl -X POST http://127.0.0.1:8787/users \
  -H "Content-Type: application/json" \
  -d '{"email":"me@example.com"}'
# -> 201 {"user_id":"<uuid>","api_key":"hm_live_..."}
```

The `api_key` is shown exactly once — only its argon2 hash is stored — and
proxies straight through from then on:

```sh
curl -H "Authorization: Bearer hm_live_..." http://127.0.0.1:8787/some/sqld/path
```

Re-registering an address that already exists returns `409`, not `500`.

The account and key are committed *before* the sqld call, so a `201` is real
even if sqld is briefly down. In that case the namespace lands on a later tick
of the in-process background worker instead, and requests with the new key get
the usual "no mapping" `404` until it does — see
`docs/superpowers/specs/2026-08-12-database-provisioning-design.md` for the
outbox design behind that.

Hand-seeding `database_mappings`/`api_keys` via psql still works and is still
the only route for a `workspace`-owned mapping (deliberately not provisioned —
see the design spec), but it's no longer needed for the ordinary case.

### Org-shared namespaces and custom roles

`POST /orgs` creates an org, provisions its namespace, and makes the caller its
`owner` with every permission in the catalog. It's authenticated, and only a
*personal* key may call it:

```sh
curl -X POST http://127.0.0.1:8787/orgs \
  -H "Authorization: Bearer hm_live_..." \
  -H "Content-Type: application/json" \
  -d '{"name":"Acme Inc"}'
# -> 201 {"org_id":"<uuid>"}
```

No second API key is minted: a personal API key reaches an org's shared
namespace by sending an `X-Org-Id: <org uuid>` header with the request, and
the gateway checks the caller's role in that org before proxying.

```sh
curl -H "Authorization: Bearer hm_live_..." -H "X-Org-Id: <org id>" \
  http://127.0.0.1:8787/some/sqld/path
```

From there, that user's own personal key can manage roles over HTTP:

```sh
curl -X POST http://127.0.0.1:8787/orgs/<org id>/roles \
  -H "Authorization: Bearer hm_live_..." \
  -H "Content-Type: application/json" \
  -d '{"name":"analyst","permissions":["db:query"]}'
```

The permission catalog is fixed: `org:manage_members`, `org:manage_roles`,
`db:query`, `db:sync`. See
`docs/superpowers/specs/2026-08-11-org-roles-design.md` for the full design.

Note that the `db:query`/`db:sync` split is **transport-shaped, not
intent-shaped**: `db:query` is required for HTTP/1.1 requests and `db:sync`
for HTTP/2 requests, regardless of what the request actually does — mirroring
the proxy's existing per-connection HTTP/1.1-vs-h2c fork. So an HTTP/2 Hrana
client holding only `db:query` gets a 403; that is the documented, intentional
behaviour rather than a permissions bug. Grant a role both permissions if its
holders will use both transports.

## Development

```sh
cargo build
cargo test                    # needs the dev services running (see Quickstart)
cargo clippy --all-targets
```

Tests default `DATABASE_URL`/`SQLD_URL`/`SQLD_ADMIN_URL` to the podman-compose
ports above if unset, so a bare `cargo test` works once the services are up.

The dev Postgres container isn't reset between test runs, so rows accumulate
over time. `scripts/reset-dev-db.sh` truncates everything if that becomes a
problem, or if migration `0002`'s namespace-format check ever rejects a
pre-existing bad row on connect.

## Project layout

| Path | Responsibility |
|---|---|
| `src/auth.rs` | API key generation/hashing, the `auth_middleware` extractor |
| `src/db.rs` | Postgres connection + typed control-plane accessors |
| `src/routing.rs` | Resolves an authenticated owner to a sqld namespace |
| `src/proxy.rs` | Streaming reverse-proxy to sqld (HTTP/1.1 and h2c) |
| `src/roles.rs` | Permission catalog, role CRUD, the org-membership permission check |
| `src/org_admin.rs` | HTTP endpoints for role management and member-role assignment |
| `src/registration.rs` | `POST /users` and `POST /orgs` — the account/org creation endpoints |
| `src/provisioning.rs` | Namespace provisioning: the outbox row, `attempt_provisioning`, the background retry worker |
| `migrations/` | Postgres schema, applied automatically via `sqlx::migrate!` |
| `docs/superpowers/` | Design specs and implementation plans for each slice built so far |
