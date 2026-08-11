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

**Status:** walking skeleton. Org role enforcement, billing, rate limiting,
an admin API, and provisioning automation don't exist yet — see the memory
entries tagged `project:hivemind-gateway` for the current state and plan, or
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

### Seeding a test user, API key, and namespace

There's deliberately no admin API in this slice (see the design spec's
Global Constraints) — seed data directly:

```sh
# 1. Create a namespace in sqld
curl -X POST http://127.0.0.1:8090/v1/namespaces/myns/create -d '{}'

# 2. Insert a user + database_mapping via psql
podman exec -it hivemind-gateway_postgres_1 psql -U gateway -d gateway
```
```sql
INSERT INTO users (id, email) VALUES (gen_random_uuid(), 'me@example.com') RETURNING id;
-- use the returned id below
INSERT INTO database_mappings (owner_type, owner_id, sqld_namespace)
  VALUES ('user', '<id from above>', 'myns');
```

3. Generate an API key. There's no CLI for this yet — `hivemind_gateway::auth::generate_api_key()`
   is the function that does it (argon2-hashes it, returns `(full_key, prefix, hash)`);
   for now, calling it means a one-off `cargo test`-style scratch binary or REPL,
   the same way the integration tests do it (see `tests/auth_test.rs`). Insert
   the returned `prefix`/`hash` into `api_keys` (with the same `owner_type`/`owner_id`
   as the mapping above), and keep `full_key` — it's shown once, only the hash
   is stored.

```sh
curl -H "Authorization: Bearer <full_key>" http://127.0.0.1:8787/some/sqld/path
```

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
| `migrations/` | Postgres schema, applied automatically via `sqlx::migrate!` |
| `docs/superpowers/` | Design specs and implementation plans for each slice built so far |
