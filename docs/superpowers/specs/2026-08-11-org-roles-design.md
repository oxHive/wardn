# hivemind-gateway Org Membership + Custom Roles — Design

**Status:** Approved. Second sub-project of `hivemind-gateway`, built on the walking skeleton (`docs/superpowers/specs/2026-08-11-walking-skeleton-design.md`).

## Goal

Give an org admin AWS-IAM-style role management: define named roles from a fixed permission catalog, assign them to org members, and have the gateway enforce them at the proxy boundary. This also closes a real functional gap in the walking skeleton — there is currently no path for a user's personal API key to reach an org's shared namespace at all; `api_keys` are strictly 1:1 with an owner.

## Scope decisions (from brainstorming)

- **Granularity: coarse, not content-level.** No parsing of Hrana query bodies to classify individual statements as read/write/delete. Permissions gate whole request *paths* (which endpoint/protocol a request uses), never request *content*. This keeps the gateway's "never parses payload" boundary intact — an earlier option (fine-grained read/write/delete enforced on memory content) was explicitly considered and rejected as premature: it would require SQL-statement classification inside the proxy, with no confirmed demand yet.
- **Custom roles, fixed permission catalog.** Org admins name and compose roles; they cannot invent new permission types. The catalog is a hardcoded enum, extended only by a future migration.
- **Includes a minimal admin API.** Role CRUD and member-role assignment ship as real HTTP endpoints in this slice, not SQL-seed-only — this is a deliberate, scoped pull-forward of what would otherwise be part of a separate future Admin API sub-project. Org/user/database-mapping bootstrap itself stays SQL-seed, unchanged from the walking skeleton.

**Explicitly out of scope for this slice** (each is a future sub-project or explicitly deferred):
- Content-level read/write/delete permissions on memory data.
- User-definable/custom permission types (the catalog is fixed).
- Org creation, user creation, or database-mapping provisioning via HTTP — still SQL-seed, per the walking skeleton.
- Billing/plan-based permission limits.
- Workspace-level roles (this slice is org-scoped only; `database_mappings.owner_type = 'workspace'` access is unaffected and still resolves the same way it does today).

## Permission catalog

Fixed, hardcoded (Rust-level enum + a DB `CHECK` constraint mirroring it — not a Postgres table, since it isn't user-extensible):

| Permission | Gates |
|---|---|
| `org:manage_members` | Add/remove org members, assign a role to a member |
| `org:manage_roles` | Create/edit/delete roles, attach/detach permissions on a role |
| `db:query` | Proxy access to an org namespace over the Hrana/HTTP query path |
| `db:sync` | Proxy access to an org namespace over the WAL/gRPC replication (embedded-replica sync) path |

The `db:query` / `db:sync` split mirrors the proxy's existing HTTP/1.1-vs-h2c fork (`proxy.rs`'s `for_version`) — no new protocol detection is needed, only a permission check gating the fork that already exists.

## Data model

```sql
roles             (id, org_id FK, name, created_at)         -- UNIQUE(org_id, name)
role_permissions  (role_id FK, permission,                  -- permission CHECK IN the 4 catalog values
                    PRIMARY KEY (role_id, permission))
org_members       (org_id FK, user_id FK, role_id FK -> roles(id), created_at)
                    -- replaces org_members.role TEXT CHECK IN (admin/member/read_only)
```

`org_members.role_id` is `NOT NULL` — every member has exactly one role. There is no "no role" state; an org with members always has at least the bootstrap `owner` role (below).

Exact column types, index names, and the migration numbering are plan-level detail.

### Bootstrap

Org creation remains SQL-seed-only (unchanged walking-skeleton constraint). The seed that creates an org now also inserts one default role named `owner` with all four catalog permissions attached, and assigns it to the org's first member. This is the same "seeded by hand once" pattern the walking skeleton already uses for the first user/API key — it resolves the chicken-and-egg of "who has `org:manage_roles` to create the first role" without adding an org-creation endpoint.

## The org-shared-namespace gap this closes

Today, `resolve_namespace(pool, owner)` (`src/routing.rs`) looks up `database_mappings` keyed directly on the authenticated `AuthedOwner`'s `(owner_type, owner_id)` — a key only ever reaches the namespace it was personally issued against. There is no route by which a user's own personal key reaches an *org's* namespace.

This slice adds that route: a request targeting an org resource carries an `X-Org-Id: <uuid>` header. When present:
1. Look up `org_members` for `(org_id, authed user_id)`. No row → 403 (not a member).
2. Load the member's `role_id` → its permissions via `role_permissions`.
3. Confirm the required permission for this request is present (below) → 403 if not.
4. Resolve the namespace via `database_mappings(owner_type = 'org', owner_id = org_id)` instead of the authed owner's own mapping.

No `X-Org-Id` header present → behavior is unchanged from the walking skeleton (personal key → personal namespace, no org/role lookup at all). This keeps the common case (non-org traffic) on the exact same fast path it uses today.

## Enforcement points

**Proxy path** (`src/proxy.rs` / a new middleware ahead of it): if `X-Org-Id` is present, required permission is `db:sync` when the outbound leg will be the HTTP/2-only client (h2c/gRPC replication), `db:query` otherwise (Hrana/HTTP1.1). Checked before the namespace is resolved — a member without the right permission gets 403, never reaches sqld.

**New admin endpoints** (new router, mounted alongside the existing proxy routes):
- `POST /orgs/:org_id/roles` — create a role (`{ "name": string, "permissions": [string] }`). Requires `org:manage_roles`.
- `PATCH /orgs/:org_id/roles/:role_id` — replace a role's permission set. Requires `org:manage_roles`.
- `DELETE /orgs/:org_id/roles/:role_id` — delete a role. Requires `org:manage_roles`. Rejects (409) if any member still holds it — admin must reassign members first, no cascading role deletion that would silently strand a member without a role.
- `PUT /orgs/:org_id/members/:user_id/role` — assign a role to an existing member (`{ "role_id": uuid }`). Requires `org:manage_members`.
- `GET /orgs/:org_id/roles` — list an org's roles and their permissions. Requires `org:manage_roles` (read access to role definitions is treated as a role-management capability, not a general membership one).

All five require the caller to themselves be a member of `:org_id` with the stated permission — the same `org_members` + `role_permissions` lookup the proxy path uses, factored into one shared helper rather than duplicated.

Adding/removing *members themselves* (as opposed to assigning a role to an existing one) is not part of this slice's endpoint set — org membership rows, like org/user creation, are still seeded directly. Only role CRUD and role *assignment* to an already-existing member are exposed over HTTP. This keeps the admin-API pull-forward narrowly scoped to what the IAM-roles feature actually needs.

## Error handling

- No `X-Org-Id` on an org-scoped admin endpoint (`/orgs/:org_id/...`) is not applicable — those endpoints take `org_id` from the path, not the header; the header is only for the proxy path's org-namespace selection.
- Non-member calling any org-scoped endpoint (proxy or admin) → 403, indistinguishable from "member but lacks permission" (don't leak org membership existence to non-members).
- Unknown `role_id` on assignment/delete → 404.
- Deleting a role still held by a member → 409 (see above).
- All authorization checks happen after the existing `auth_middleware` (API key validation unchanged) and before any sqld/Postgres write — a rejected request never mutates state.

## Testing

- Unit tests on the permission-check helper: role has permission → pass, role lacks it → fail, non-member → fail.
- Integration test proving cross-org isolation: a member of org A with a valid role cannot reach org B's namespace or org B's roles/members via `X-Org-Id`, even with a technically-valid personal key.
- Integration test proving the full `X-Org-Id` path end-to-end through the real router: personal key + `X-Org-Id` header + sufficient role → request reaches the org's actual sqld namespace (same style as the walking skeleton's `namespace_isolation_through_full_router` test).
- Integration tests for each new admin endpoint: happy path, wrong-permission 403, non-member 403, unknown-role 404, delete-role-in-use 409.

## Non-goals / risks carried forward

- This remains "dumb" about memory content — permissions gate *which path* a request takes, never *what's in it*. If a future need requires content-level enforcement, that's a new, separately-scoped sub-project, not an extension bolted onto this one.
- No permission caching/invalidation design here — every check is a live Postgres lookup. If this becomes a latency concern under load, that's a future optimization, not a walking-skeleton-tier concern.
