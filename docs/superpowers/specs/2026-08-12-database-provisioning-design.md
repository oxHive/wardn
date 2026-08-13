# hivewarden Database Provisioning Automation — Design

**Status:** Approved. Third sub-project of `hivewarden`, built on the walking skeleton (`docs/superpowers/specs/2026-08-11-walking-skeleton-design.md`) and org roles (`docs/superpowers/specs/2026-08-11-org-roles-design.md`).

## Goal

Replace the manual "curl sqld's admin API, then hand-insert `database_mappings` via psql" workflow with real endpoints: `POST /users` (register — also mints the first API key) and `POST /orgs` (create an org, authenticated). Each provisions its own sqld namespace automatically. This directly enables a future gateway UI (deferred, its own sub-project) — right now there is no way to get a working account without shell access to the dev Postgres.

## Scope decisions (from brainstorming)

- **Users and orgs only — no workspace provisioning.** hivemind core's `Layer::Workspace` (memory-scoping tag) has no `[workspace_sync]` client config; it never syncs to a separate remote database. Gateway's `workspaces` table has nothing real to provision a namespace *for* right now, so it's excluded entirely, not merely deferred.
- **`POST /orgs` mints no second API key.** Org-roles' `X-Org-Id` mechanism means the creator's existing personal key, plus `X-Org-Id: <org id>`, already reaches the org's namespace once they're a member. `POST /orgs` makes the caller a member with a bootstrap `owner` role (all 4 permissions from the org-roles catalog) instead.
- **Namespace name = the owner's own UUID, verbatim.** Already satisfies `migrations/0002_namespace_format_constraint.sql`'s CHECK, globally unique by construction, no slugification or collision handling needed.
- **Outbox pattern for the Postgres-write / sqld-admin-call split**, not a synchronous two-phase attempt or compensating rollback. The account/org row and the outbox row are inserted in one Postgres transaction (atomic — no partial-failure window on the Postgres side). Namespace creation against sqld is a separate, retryable step; `database_mappings` only gets its row once that step actually succeeds.
- **Inline attempt first, in-process background retry as the durability path.** The request handler tries the sqld call synchronously right after committing the account/outbox row (fast path — works whenever sqld is healthy). If that fails, the request still succeeds (the account and key are already real); a `tokio::time::interval` loop inside the gateway process retries pending outbox rows. No external cron, no separate worker binary.
- **No password/session infrastructure.** The only auth primitive in this system is API keys; login/session auth is entirely the future gateway-UI sub-project's concern, and adding an unused `password_hash` column now would be guessing at that sub-project's actual requirements (password vs. OAuth vs. magic link) before it's been brainstormed.

**Explicitly out of scope for this slice** (each is a future sub-project or explicitly deferred):
- Workspace provisioning (see above — not applicable today, not merely postponed).
- A gateway UI/dashboard.
- Password/session auth of any kind.
- Signup abuse / rate-limiting on the now-public `POST /users` — deferred to the rate-limiting sub-project. This slice makes registration open to anyone who can reach the gateway; that is a known, accepted gap until rate limiting exists.
- Email verification.
- Compensating rollback (deleting an orphaned sqld namespace if the Postgres write somehow failed after a successful namespace creation — can't happen with the chosen ordering, since the namespace call only ever happens *after* the Postgres transaction commits, never before).

## Data model

```sql
namespace_provisioning_outbox (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_type     TEXT NOT NULL CHECK (owner_type IN ('user', 'org')),
    owner_id       UUID NOT NULL,
    sqld_namespace TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('pending', 'done', 'failed')) DEFAULT 'pending',
    attempts       INT NOT NULL DEFAULT 0,
    last_error     TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
)
```

Named specifically (not `provisioning_outbox` or `outbox`) so a future unrelated use of the outbox pattern (e.g. billing events, emails) gets its own table rather than overloading this one.

`database_mappings` is unchanged — a row for a given owner only ever gets inserted once provisioning succeeds, so the existing `routing::resolve_namespace`'s "no mapping → 404" behavior already means "not provisioned yet" with zero new error-handling anywhere downstream of it.

Exact column types/constraints beyond what's shown, and migration numbering, are plan-level detail.

## Architecture

### Shared provisioning function

One function, two callers (the inline attempt and the background worker), so "call sqld, insert the mapping, mark done" exists exactly once:

```
attempt_provisioning(pool, sqld_admin_url, outbox_row) -> Result<(), Error>
```

Calls sqld's admin API (`POST /v1/namespaces/{name}/create`) to create the namespace. On success: in one transaction, insert `database_mappings` and mark the outbox row `done`. On failure: bump `attempts`, record `last_error`, leave the row `pending` (or flip to `failed` once a max-attempt cap is reached — exact cap is plan-level, e.g. 10).

### `POST /users` (public)

No `Authorization` header — this is how a caller gets their first API key at all. Registered in the router *after* `.layer(auth_middleware)`, the same place `/healthz` lives, so it bypasses the auth layer entirely.

Request: `{ "email": string }`. Handler: one transaction inserts `users` + a fresh personal `api_keys` row (via the existing `auth::generate_api_key()`) + a `namespace_provisioning_outbox` row (`owner_type = 'user'`, `sqld_namespace` = the new user's own id). Commits, then calls `attempt_provisioning` inline, best-effort — its outcome does not affect the response. Returns `201 { user_id, api_key }` regardless; the account and key are real either way, and the namespace either lands synchronously or gets picked up by the background worker shortly after.

### `POST /orgs` (authenticated)

Requires a valid personal API key (goes through `auth_middleware` normally, reads the caller via the existing `AuthedOwner` extension — same as the org-roles admin endpoints). Registered alongside those endpoints, before `.layer(auth_middleware)`.

Request: `{ "name": string }`. Handler: one transaction inserts `orgs` + a bootstrap role named `owner` with all 4 org-roles permissions attached + an `org_members` row linking the caller to that role + a `namespace_provisioning_outbox` row (`owner_type = 'org'`, `sqld_namespace` = the new org's own id). Commits, then calls `attempt_provisioning` inline, same as above. Returns `201 { org_id }` — no second API key. From then on, the caller's existing personal key plus `X-Org-Id: <org_id>` reaches the org's namespace, gated by their `owner` role.

### Background worker

A `tokio::time::interval` loop spawned alongside `axum::serve` in `main.rs` (in-process, no external cron or separate binary — matches this project's single-binary deployment via podman-compose). Every N seconds: selects `pending` outbox rows under the attempt cap, calls `attempt_provisioning` on each. A row that exhausts its attempts flips to `failed` and is logged — matches the existing "manual escape hatch, not full automation" tier this project already uses for `scripts/reset-dev-db.sh`. Exact interval and cap values are plan-level detail.

### New dependency

`reqwest` moves from `[dev-dependencies]` to a real dependency — it's already in the dependency tree for the test suite's admin-API calls, and hand-rolling the admin API's simple JSON POST on raw `hyper` isn't worth a second, more primitive HTTP client stack. `Config` gains a required `SQLD_ADMIN_URL` (currently only read ad hoc by tests via an env-var default).

## Testing

- Unit tests on `attempt_provisioning`'s two outcomes (success → mapping inserted + outbox `done`; sqld failure → outbox stays `pending` with `attempts` incremented and `last_error` set), against a real sqld and real Postgres, no mocks.
- Integration test: `POST /users` end-to-end — real HTTP call through the full router, assert `201` with a real `api_key`, then assert (either immediately or after the background worker's next tick) that a proxied request using that key actually reaches a real, isolated sqld namespace.
- Integration test: `POST /orgs` end-to-end — same shape, but additionally asserts the caller can immediately use `X-Org-Id` with their existing personal key to reach the new org's namespace (proving the bootstrap role + membership insert worked), following the same write-then-read-back pattern org-roles used, not just a `200`.
- A test proving the inline-attempt-fails-but-request-still-succeeds path: point `SQLD_ADMIN_URL` at an unreachable address for one request, assert `201` still comes back with a real key/org, assert the outbox row is `pending`, then point a real sqld back and confirm the background worker's next tick provisions it.

## Non-goals / risks carried forward

- Registration is open to anyone who can reach the gateway until the rate-limiting sub-project exists — an accepted, explicit gap, not an oversight.
- No compensating rollback exists or is needed, because the chosen ordering (Postgres commits first, sqld call happens after) means a failure can only ever leave behind an unprovisioned account, never an orphaned sqld namespace pointing at nothing in Postgres.
- The background worker retries forever up to its attempt cap; a `failed` row needs manual intervention (same tier as this project's other manual escape hatches) — no alerting/paging exists yet, that's part of the future observability sub-project.
