# hivemind-gateway Org Membership Management + Self-Service API Keys — Design

**Status:** Approved. Fourth sub-project of `hivemind-gateway`, built on the walking skeleton, org roles, and database provisioning automation.

## Goal

Close two remaining gaps in the Admin API backlog item: there is currently no way to add or remove a member from an org at all (org-roles only reassigns an *existing* member's role), and no way for a user to manage their own API keys beyond the single key minted at registration (issue an additional one, list what they have, revoke one). Both are self-contained, independently useful, and ship together as one slice since neither needs the other.

## Scope decisions (from brainstorming)

- **"Invite" means adding an already-registered user, not a pending-invitation flow.** `POST /orgs/:org_id/members` looks up a user by email and inserts them directly into `org_members` with a specified role. Inviting someone who hasn't registered yet (email-token flow, accepted on eventual signup) is out of scope — it needs an `invitations` table and, eventually, actual email-sending infrastructure that doesn't exist anywhere in this project.
- **Key management is self-service only.** A user manages their own keys (list, issue, revoke) via their own authenticated request — never another user's. An earlier option (letting an org admin revoke a member's key for offboarding) was explicitly considered and rejected: a personal API key isn't org-scoped — it also reaches the holder's personal namespace and any *other* org they belong to via `X-Org-Id` — so "revoke a member's key" would have a blast radius far beyond the org doing the revoking. Offboarding is handled by removing the member from the org instead (see below), which only affects this org's access.
- **Member removal, not key revocation, is the org-admin-facing offboarding tool.** `DELETE /orgs/:org_id/members/:user_id` deletes the `org_members` row. This cuts off `X-Org-Id` access to this org's namespace only; the removed user's personal key and any other org membership are untouched.
- **File structure: nest org-scoped admin surface under `src/org/`.** The existing `src/org_admin.rs` (role management, shipped by org-roles) moves to `src/org/admin.rs` as-is (no logic changes), and this slice's new member-management handlers live alongside it at `src/org/members.rs`. `src/api_keys.rs` stays flat at the top level since it isn't org-scoped at all.

**Explicitly out of scope for this slice:**
- Inviting an unregistered person by email (needs an invitations table + eventual email sending).
- Org-admin visibility or control over a member's personal keys.
- Any limit on how many keys a user may hold (no rate limiting exists yet in this project).
- A "last admin" guard preventing an org from removing its only `org:manage_members`/`org:manage_roles` holder — same accepted limitation org-roles already has for role deletion.

## Architecture

### File structure

```
src/org.rs           -- pub mod admin; pub mod members;  (new, thin)
src/org/admin.rs      -- moved from src/org_admin.rs verbatim (git mv, no logic changes)
src/org/members.rs    -- new: add_member, remove_member handlers
src/api_keys.rs        -- new: list_keys, create_key, revoke_key handlers (self-service, not org-scoped)
```

`src/lib.rs` changes `pub mod org_admin;` to `pub mod org;`, and every route registration referencing `org_admin::...` becomes `org::admin::...`. Handlers in `src/org/members.rs` are referenced as `org::members::...`.

### Endpoints

**`POST /orgs/:org_id/members`** — authenticated, requires `org:manage_members` (via the existing `roles::require_permission`, same pattern every org-admin endpoint already uses). Request: `{ "email": string, "role_id": uuid }`. Looks up `users` by `email` (case-insensitively — an exact match would `404` "not registered" for a differently-cased form of an address that *is* registered; and because `users.email`'s `UNIQUE` is case-*sensitive*, an address matching two case-variant accounts is a logged `500` rather than a silent guess at which account was meant); `404` if no such user exists (this slice never creates a user as a side effect of inviting them — they must already be registered). Inserts an `org_members` row for `(org_id, that user's id, role_id)`. `org_members`'s existing `PRIMARY KEY (org_id, user_id)` means re-inviting an existing member is a unique violation — caught the same way `roles`'s duplicate-name case already is, returned as `409` rather than `500`. An unknown or wrong-org `role_id` is `404` (mirroring `set_role_permissions`'s existing existence check pattern). Returns `201` with the new `org_members` row's basics on success.

**`GET /orgs/:org_id/members`** — authenticated, requires `org:manage_members` (same permission as add/remove). Returns the org's members as `[{ user_id, email, role_id }]`, ordered by `email`. Added during final review: without it the removal endpoint below is undrivable from this feature's own API surface, since the add endpoint only ever takes an *email* and an admin holding a departing member's email would otherwise have no HTTP route to their `user_id`.

**`DELETE /orgs/:org_id/members/:user_id`** — authenticated, requires `org:manage_members`. Deletes the `org_members` row for `(org_id, user_id)`. `404` if no such membership exists. `204` on success. Does not touch `users`, `api_keys`, or any other org's membership — the removed user's account and personal key are completely unaffected.

**`GET /api-keys`** — authenticated, `owner_type == "user"` only (the same guard `POST /orgs` already uses — a workspace/org-owned key has no notion of "its own" additional keys in this model). Returns the caller's own `api_keys` rows: `[{ id, prefix, created_at, revoked_at }]`, ordered by `created_at`. Never returns `key_hash` or anything that could reconstruct the full key.

**`POST /api-keys`** — authenticated, `owner_type == "user"` only. Mints an additional personal key for the caller via the existing `auth::generate_api_key()`, inserted with the same `owner_type = 'user', owner_id = caller`. Returns `201` with `{ id, api_key }` — the full key is shown exactly once, exactly like `POST /users`'s registration response.

**`DELETE /api-keys/:id`** — authenticated, `owner_type == "user"` only. Sets `revoked_at = now()` on the given `api_keys` row, but *only* if `user_id` matches the caller — a key id that exists but belongs to someone else is `404`, not `403` (don't confirm another user's key id exists). `204` on success. A key that's already revoked is idempotently `204` again (setting `revoked_at` on an already-revoked row is harmless), not an error.

### Data model

No new tables and no migration. `org_members`, `roles`, `users`, `api_keys` all already have everything needed:
- Member add/remove operate directly on the existing `org_members(org_id, user_id, role_id)`.
- Key list/create/revoke operate directly on the existing `api_keys(id, user_id, owner_type, owner_id, prefix, key_hash, revoked_at)`.

### Error handling

- Non-member calling an org-scoped endpoint here → `403`, identical to every other org-admin endpoint's existing "member without permission" response — no new leak surface.
- `POST /orgs/:org_id/members` with an email that isn't registered → `404` (distinguishable from "not a member of this org," since that's a `403` — this is intentional: an org admin needs to know "that email hasn't signed up yet" is a different problem than "you lack permission").
- Every "does this row belong to the caller/this org" check follows the same shape already established by `roles::delete_role` and `roles::set_role_permissions`: scope the query to the owner, return `NotFound` for anything that doesn't match — never distinguish "exists elsewhere" from "doesn't exist" in the response.

## Testing

- `POST /orgs/:org_id/members` end-to-end: register two users via `POST /users`, create an org via `POST /orgs` as user A, invite user B by email with a `db:query`-only role, then prove user B's *own* personal key plus `X-Org-Id` now reaches the org's namespace (write-then-read-back, same pattern used throughout this project — not a status-only check).
- `DELETE /orgs/:org_id/members/:user_id`: same setup, remove user B, then prove their key plus `X-Org-Id` now gets `403` — while a fresh proxied request with no `X-Org-Id` (their own personal namespace) still works, proving removal didn't touch their account.
- Duplicate invite (already a member) → `409`. Invite by unregistered email → `404`. Invite/remove by a non-member caller → `403`.
- `GET`/`POST`/`DELETE /api-keys`: issue a second key, list shows both with correct prefixes and no hash; use the second key on a real proxied request to prove it's genuinely independent of the first; revoke the first, prove it now `401`s while the second still works.
- Revoking a key that isn't the caller's own (seed a second user's key, try to revoke it as the first user) → `404`.

## Non-goals / risks carried forward

- No last-admin guard, same as org-roles' existing role-deletion behavior — an org can remove its only `org:manage_members` holder and lock itself out of further membership changes, recoverable only via direct SQL. Consistent with this project's current tier (SQL-seed bootstrap already required for the very first org anyway).
- No limit on API keys per user — deferred to the future rate-limiting/billing sub-project, same as every other unbounded-today surface in this project.
- No guard against a user revoking their own last live API key. `DELETE /api-keys/:id` will happily revoke the key the request is authenticated with, and there is no recovery path over HTTP once it's gone — the only unauthenticated endpoint, `POST /users`, `409`s on an already-registered email — so getting back in requires direct SQL. Accepted deliberately rather than fixed, same tier and same reasoning as the last-admin gap above: this project's other unrecoverable-without-SQL states are all in the same place, and a guard here would be the only one of its kind. Documented in the README instead, next to the revoke example.
