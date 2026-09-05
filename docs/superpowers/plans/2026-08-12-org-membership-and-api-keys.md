# Org Membership Management + Self-Service API Keys Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the two remaining Admin API gaps: adding/removing org members by email (`POST`/`DELETE /orgs/:org_id/members...`), and self-service API key management (`GET`/`POST /api-keys`, `DELETE /api-keys/:id`).

**Architecture:** Both pieces operate on existing tables (`org_members`, `api_keys`) — no new migration. Org-scoped admin code moves under `src/org/` (`admin.rs` = existing role management, moved as-is; `members.rs` = new). Key management is self-service only and lives flat at `src/api_keys.rs` since it isn't org-scoped.

**Tech Stack:** axum 0.8, sqlx 0.8 (Postgres, runtime-checked queries) — same as every prior slice.

## Global Constraints

- Rust edition 2024, axum 0.8, sqlx 0.8 Postgres. **Runtime-checked queries only** (`sqlx::query_as::<_, T>(...)`) — never the compile-time `query!`/`query_as!` macros.
- **No new migration.** Every operation in this plan uses `org_members`, `roles`, `users`, and `api_keys` exactly as they exist today.
- **"Invite" means adding an already-registered user by email.** No pending-invitation state, no email sending. An email that doesn't match any `users` row is `404`.
- **Key management is self-service only.** Every `/api-keys` endpoint operates on the caller's own keys (`owner_type == "user"`, matched against `owner.owner_id`) — there is no org-admin-facing key control anywhere in this plan.
- **Member removal is the offboarding tool, not key revocation.** `DELETE /orgs/:org_id/members/:user_id` only deletes the `org_members` row — it must never touch `api_keys` or `users`.
- **File structure:** `src/org_admin.rs` moves to `src/org/admin.rs` verbatim (pure `git mv`, zero content changes — Rust's `crate::`-rooted `use` paths are unaffected by module nesting depth). `src/org/members.rs` is new. `src/api_keys.rs` is new and flat (not under `src/org/`) since it isn't org-scoped.
- **All test seed values (UUIDs, emails, org names) must derive from a fresh `Uuid::new_v4()` per test run** — hardcoded literals collide with `UNIQUE` constraints on repeat runs against the persistent dev Postgres container.
- A caller who isn't a member of an org, and a member who lacks the required permission, both return the identical `403` — the existing `roles::require_permission` behavior, unchanged by this plan.
- An email that doesn't match a registered user is `404`, distinguishable from "you lack permission" (`403`) — an org admin needs to know "that email hasn't signed up" is a different problem than a permission error.
- Revoking a key that exists but isn't the caller's own is `404`, never `403` — don't confirm to a caller that a key id they don't own actually exists.

---

### Task 1: Move `src/org_admin.rs` to `src/org/admin.rs`

**Files:**
- Move: `src/org_admin.rs` → `src/org/admin.rs` (no content changes)
- Create: `src/org.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: nothing new — this is a pure reorganization of already-shipped code.
- Produces (for Tasks 2, 3): the `src/org/` directory exists, ready for `src/org/members.rs`. Every existing `org_admin::X` caller becomes `org::admin::X`.

- [ ] **Step 1: Move the file**

```bash
mkdir -p src/org
git mv src/org_admin.rs src/org/admin.rs
```

No content changes — `src/org/admin.rs`'s contents are byte-for-byte identical to the old `src/org_admin.rs`. Every `use crate::...` inside it is already rooted at the crate, so nesting the file one directory deeper changes nothing about how those paths resolve.

- [ ] **Step 2: Create `src/org.rs`**

```rust
pub mod admin;
```

- [ ] **Step 3: Update `src/lib.rs`**

Replace the full contents of `src/lib.rs` with:

```rust
pub mod auth;
pub mod config;
pub mod db;
pub mod org;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/orgs", post(registration::create_org))
        .route(
            "/orgs/{org_id}/roles",
            post(org::admin::create_role).get(org::admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org::admin::update_role).delete(org::admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org::admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .route("/users", post(registration::create_user))
        .with_state(state)
}
```

- [ ] **Step 4: Verify nothing broke**

Run: `cargo build`
Expected: succeeds, no errors.

Run: `cargo test`
Expected: the full existing suite still passes bare, with zero test file changes — `tests/org_admin_test.rs` and every other test file only ever call `wardn::app(state)` over real HTTP; none of them import `org_admin`/`org::admin` directly, so this move is invisible to every existing test.

- [ ] **Step 5: Commit**

```bash
git add src/org.rs src/org/admin.rs src/lib.rs
git commit -m "refactor: move org_admin.rs under src/org/ ahead of org/members.rs"
```

---

### Task 2: Org membership management (`POST`/`DELETE /orgs/:org_id/members...`)

**Files:**
- Create: `src/org/members.rs`
- Modify: `src/org.rs`
- Modify: `src/lib.rs`
- Test: `tests/org_members_test.rs` (new)

**Interfaces:**
- Consumes: `roles::{require_permission, Permission}`, `org::admin::is_unique_violation` (already `pub(crate)`, shared the same way `roles::insert_role_permissions` was widened for `src/registration.rs`'s reuse), `auth::{AppState, AuthedOwner}`.
- Produces: `pub async fn add_member(...) -> Response`, `pub async fn remove_member(...) -> Response` — no later task consumes these directly.

- [ ] **Step 1: Write the failing tests**

Create `tests/org_members_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use wardn::auth::AppState;
use wardn::db;
use tower::ServiceExt;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

fn test_sqld_url() -> String {
    std::env::var("SQLD_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string())
}

fn admin_url() -> String {
    std::env::var("SQLD_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".to_string())
}

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

async fn register(app: axum::Router, email: &str) -> (uuid::Uuid, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id: uuid::Uuid = created["user_id"].as_str().unwrap().parse().unwrap();
    let api_key = created["api_key"].as_str().unwrap().to_string();
    (user_id, api_key)
}

async fn create_org(app: axum::Router, owner_key: &str, name: &str) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    created["org_id"].as_str().unwrap().to_string()
}

/// Fetches the org's bootstrap `owner` role id directly from Postgres — the
/// only role that exists right after `POST /orgs`, used as the role to
/// invite a second member with in tests that don't care which specific
/// permissions the invited member ends up with.
async fn bootstrap_role_id(pool: &sqlx::PgPool, org_id: uuid::Uuid) -> uuid::Uuid {
    let (role_id,): (uuid::Uuid,) = sqlx::query_as("SELECT id FROM roles WHERE org_id = $1 LIMIT 1")
        .bind(org_id)
        .fetch_one(pool)
        .await
        .unwrap();
    role_id
}

#[tokio::test]
async fn add_member_lets_the_invited_user_reach_the_orgs_namespace() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (_invitee_id, invitee_key) = register(app.clone(), &invitee_email).await;
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{invitee_email}","role_id":"{role_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The invitee's OWN personal key + X-Org-Id must now reach the org's
    // namespace — write-then-read-back, not just a status check.
    let secret = format!("MEMBER-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "invited member's write into the org's namespace failed"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(r#"{"statements":["SELECT v FROM kv"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains(&secret),
        "read-back did not contain the written secret: {text}"
    );

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn add_member_rejects_an_unregistered_email() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"nobody-{}@example.com","role_id":"{role_id}"}}"#,
                    uuid::Uuid::new_v4()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn add_member_rejects_a_duplicate_invite() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (_invitee_id, _invitee_key) = register(app.clone(), &invitee_email).await;

    let body = format!(r#"{{"email":"{invitee_email}","role_id":"{role_id}"}}"#);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn remove_member_revokes_org_access_but_not_the_personal_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (invitee_id, invitee_key) = register(app.clone(), &invitee_email).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{invitee_email}","role_id":"{role_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/orgs/{org_id}/members/{invitee_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Org access via X-Org-Id is gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "removed member should no longer reach the org's namespace"
    );

    // The invitee's own personal namespace still works — removal didn't
    // touch their account.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "removed member's own personal namespace should still work"
    );

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn remove_member_returns_404_for_a_non_member() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/orgs/{org_id}/members/{}", uuid::Uuid::new_v4()))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    delete_namespace(&org_id_str).await;
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test org_members_test`
Expected: compile failure — `/orgs/{org_id}/members` doesn't exist yet.

- [ ] **Step 3: Implement `src/org/members.rs`**

```rust
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::{AppState, AuthedOwner};
use crate::org::admin::is_unique_violation;
use crate::roles::{self, Permission};

#[derive(Deserialize)]
pub struct AddMemberRequest {
    pub email: String,
    pub role_id: Uuid,
}

#[derive(Serialize)]
pub struct MemberResponse {
    pub user_id: Uuid,
    pub role_id: Uuid,
}

/// `POST /orgs/:org_id/members` — adds an *already-registered* user to the
/// org by email. There is no pending-invitation state: the email must
/// already match a `users` row (`404` if not), and the caller must hold
/// `org:manage_members`.
pub async fn add_member(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(org_id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }

    let user_row: Result<Option<(Uuid,)>, sqlx::Error> =
        sqlx::query_as("SELECT id FROM users WHERE email = $1")
            .bind(&req.email)
            .fetch_optional(&state.pool)
            .await;
    let user_id = match user_row {
        Ok(Some((id,))) => id,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("user lookup by email failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let role_row: Result<Option<(Uuid,)>, sqlx::Error> =
        sqlx::query_as("SELECT id FROM roles WHERE id = $1 AND org_id = $2")
            .bind(req.role_id)
            .bind(org_id)
            .fetch_optional(&state.pool)
            .await;
    match role_row {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("role lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    match sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(req.role_id)
        .execute(&state.pool)
        .await
    {
        Ok(_) => (
            StatusCode::CREATED,
            Json(MemberResponse {
                user_id,
                role_id: req.role_id,
            }),
        )
            .into_response(),
        Err(e) if is_unique_violation(&e) => {
            (StatusCode::CONFLICT, "user is already a member of this org").into_response()
        }
        Err(e) => {
            tracing::error!("add member failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `DELETE /orgs/:org_id/members/:user_id` — the offboarding tool. Deletes
/// only the `org_members` row; never touches `users`/`api_keys`, so the
/// removed member's personal key and any other org membership are
/// untouched.
pub async fn remove_member(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
) -> Response {
    if let Err(status) =
        roles::require_permission(&state.pool, org_id, &owner, Permission::OrgManageMembers).await
    {
        return status.into_response();
    }

    let result = sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
        .bind(org_id)
        .bind(user_id)
        .execute(&state.pool)
        .await;
    match result {
        Ok(res) if res.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("remove member failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 4: Register the module and the routes**

In `src/org.rs`, add `pub mod members;`:

```rust
pub mod admin;
pub mod members;
```

Replace the full contents of `src/lib.rs` with:

```rust
pub mod auth;
pub mod config;
pub mod db;
pub mod org;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, delete, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/orgs", post(registration::create_org))
        .route("/orgs/{org_id}/members", post(org::members::add_member))
        .route(
            "/orgs/{org_id}/members/{user_id}",
            delete(org::members::remove_member),
        )
        .route(
            "/orgs/{org_id}/roles",
            post(org::admin::create_role).get(org::admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org::admin::update_role).delete(org::admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org::admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .route("/users", post(registration::create_user))
        .with_state(state)
}
```

Note this is the first route registered with the bare free function `delete(...)` (as opposed to the `.delete(...)` `MethodRouter` combinator already used elsewhere) — `/orgs/{org_id}/members/{user_id}` carries only `DELETE`, so it needs the free function, unlike `/orgs/{org_id}/roles/{role_id}` which combines `PATCH` and `DELETE` on one path via `patch(...).delete(...)`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test org_members_test`
Expected: all 5 tests PASS.

Run: `cargo test`
Expected: full suite still passes, no regressions.

- [ ] **Step 6: Commit**

```bash
git add src/org/members.rs src/org.rs src/lib.rs tests/org_members_test.rs
git commit -m "feat: add org membership management (invite/remove by email)"
```

---

### Task 3: Self-service API key management (`GET`/`POST /api-keys`, `DELETE /api-keys/:id`)

**Files:**
- Create: `src/api_keys.rs`
- Modify: `src/lib.rs`
- Test: `tests/api_keys_test.rs` (new)

**Interfaces:**
- Consumes: `auth::{generate_api_key, AppState, AuthedOwner}` (existing).
- Produces: `pub async fn list_keys(...) -> Response`, `pub async fn create_key(...) -> Response`, `pub async fn revoke_key(...) -> Response` — no later task consumes these directly.

- [ ] **Step 1: Write the failing tests**

Create `tests/api_keys_test.rs`:

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use wardn::auth::AppState;
use wardn::db;
use tower::ServiceExt;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

fn test_sqld_url() -> String {
    std::env::var("SQLD_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string())
}

fn admin_url() -> String {
    std::env::var("SQLD_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".to_string())
}

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

async fn register(app: axum::Router, email: &str) -> (uuid::Uuid, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id: uuid::Uuid = created["user_id"].as_str().unwrap().parse().unwrap();
    let api_key = created["api_key"].as_str().unwrap().to_string();
    (user_id, api_key)
}

#[tokio::test]
async fn list_keys_shows_the_registration_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("keys-{}@example.com", uuid::Uuid::new_v4())).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let keys: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let keys = keys.as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].get("revoked_at").unwrap().is_null());
    assert!(!keys[0].get("prefix").unwrap().as_str().unwrap().is_empty());

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn create_key_mints_an_independent_second_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_id, first_key) =
        register(app.clone(), &format!("keys-{}@example.com", uuid::Uuid::new_v4())).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {first_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let second_key = created["api_key"].as_str().unwrap().to_string();
    assert_ne!(second_key, first_key);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {first_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let keys: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(keys.as_array().unwrap().len(), 2);

    // Both keys independently reach the same personal namespace.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {second_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "second key should reach the same personal namespace"
    );

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn revoke_key_stops_only_that_key_from_authenticating() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_id, first_key) =
        register(app.clone(), &format!("keys-{}@example.com", uuid::Uuid::new_v4())).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {first_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let second_key = created["api_key"].as_str().unwrap().to_string();

    let first_key_id = {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api-keys")
                    .header(header::AUTHORIZATION, format!("Bearer {first_key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let keys: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Ordered by created_at — index 0 is the registration key itself.
        keys.as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api-keys/{first_key_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {first_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The revoked key no longer authenticates.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {first_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // The second key is untouched.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {second_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn revoke_key_returns_404_for_someone_elses_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_a_id, key_a) =
        register(app.clone(), &format!("keys-a-{}@example.com", uuid::Uuid::new_v4())).await;
    let (user_b_id, key_b) =
        register(app.clone(), &format!("keys-b-{}@example.com", uuid::Uuid::new_v4())).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {key_b}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let keys: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let key_b_id = keys.as_array().unwrap()[0]["id"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api-keys/{key_b_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {key_a}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    delete_namespace(&user_a_id.to_string()).await;
    delete_namespace(&user_b_id.to_string()).await;
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test api_keys_test`
Expected: compile failure — `/api-keys` doesn't exist yet.

- [ ] **Step 3: Implement `src/api_keys.rs`**

```rust
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::{self, AppState, AuthedOwner};

#[derive(Serialize, sqlx::FromRow)]
pub struct ApiKeySummary {
    pub id: Uuid,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct CreateKeyResponse {
    pub id: Uuid,
    pub api_key: String,
}

/// `GET /api-keys` — self-service only, `owner_type == "user"` (the same
/// guard `POST /orgs` already uses — a workspace/org-owned key has no
/// notion of "its own" additional keys in this model). Never returns
/// `key_hash` or anything that could reconstruct the full key.
pub async fn list_keys(State(state): State<AppState>, Extension(owner): Extension<AuthedOwner>) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let rows: Result<Vec<ApiKeySummary>, sqlx::Error> = sqlx::query_as::<_, ApiKeySummary>(
        "SELECT id, prefix, created_at, revoked_at FROM api_keys
         WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(owner.owner_id)
    .fetch_all(&state.pool)
    .await;
    match rows {
        Ok(keys) => Json(keys).into_response(),
        Err(e) => {
            tracing::error!("list api keys failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /api-keys` — mints an additional personal key for the caller. The
/// full key is shown exactly once, exactly like `POST /users`'s
/// registration response.
pub async fn create_key(State(state): State<AppState>, Extension(owner): Extension<AuthedOwner>) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (full_key, prefix, hash) = auth::generate_api_key();
    let id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(id)
    .bind(owner.owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&state.pool)
    .await;
    match result {
        Ok(_) => (
            StatusCode::CREATED,
            Json(CreateKeyResponse {
                id,
                api_key: full_key,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("create api key failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `DELETE /api-keys/:id` — revokes one of the caller's own keys. `404` for
/// a key id that exists but isn't the caller's own — never `403`, so a
/// caller can't use the response to confirm another user's key id exists.
/// Idempotent: revoking an already-revoked key still `204`s.
pub async fn revoke_key(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    Path(key_id): Path<Uuid>,
) -> Response {
    if owner.owner_type != "user" {
        return StatusCode::FORBIDDEN.into_response();
    }
    let result = sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND user_id = $2")
        .bind(key_id)
        .bind(owner.owner_id)
        .execute(&state.pool)
        .await;
    match result {
        Ok(res) if res.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!("revoke api key failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
```

- [ ] **Step 4: Wire the routes into `src/lib.rs`**

Replace the full contents of `src/lib.rs` with:

```rust
pub mod api_keys;
pub mod auth;
pub mod config;
pub mod db;
pub mod org;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, delete, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route(
            "/api-keys",
            get(api_keys::list_keys).post(api_keys::create_key),
        )
        .route("/api-keys/{id}", delete(api_keys::revoke_key))
        .route("/orgs", post(registration::create_org))
        .route("/orgs/{org_id}/members", post(org::members::add_member))
        .route(
            "/orgs/{org_id}/members/{user_id}",
            delete(org::members::remove_member),
        )
        .route(
            "/orgs/{org_id}/roles",
            post(org::admin::create_role).get(org::admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org::admin::update_role).delete(org::admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org::admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .route("/users", post(registration::create_user))
        .with_state(state)
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test api_keys_test`
Expected: all 4 tests PASS.

Run: `cargo test`
Expected: full suite still passes, no regressions.

- [ ] **Step 6: Commit**

```bash
git add src/api_keys.rs src/lib.rs tests/api_keys_test.rs
git commit -m "feat: add self-service API key management (list, create, revoke)"
```

---

## Self-Review Notes

- **Spec coverage:** file structure (`src/org/{admin.rs,members.rs}` + flat `src/api_keys.rs`, Task 1-3) — invite-by-email with 404/409 handling (Task 2) — member removal without touching keys/other memberships (Task 2, tested explicitly) — self-service-only key list/create/revoke with the `owner_type == "user"` guard on all three (Task 3) — 404-not-403 on both "unregistered email" and "someone else's key id" (Tasks 2 and 3, each tested) — no new migration (confirmed: every task only touches existing tables). No gaps found.
- **Placeholder scan:** none found — every step has complete, runnable code.
- **Type consistency:** `AddMemberRequest`, `MemberResponse`, `add_member`, `remove_member`, `ApiKeySummary`, `CreateKeyResponse`, `list_keys`, `create_key`, `revoke_key` are each defined exactly once and referenced identically in `src/lib.rs`'s route registrations and every test file. `org::admin::is_unique_violation`'s `pub(crate)` visibility (already shipped) is consumed as-is by Task 2, not redefined.
