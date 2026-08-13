mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivewarden::auth::{self, AppState};
use hivewarden::db;
use hivewarden::roles::{self, Permission};
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

fn test_sqld_url() -> String {
    std::env::var("SQLD_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string())
}

/// Seeds an org, a role granting `permissions`, a user holding that role,
/// and their API key. Returns (org_id, full_key).
async fn seed_admin(pool: &sqlx::PgPool, permissions: &[Permission]) -> (Uuid, String) {
    let org_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let role_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("admin-{user_id}@example.com"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO roles (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(role_id)
        .bind(org_id)
        .bind(format!("role-{role_id}"))
        .execute(pool)
        .await
        .unwrap();
    for permission in permissions {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, $2)")
            .bind(role_id)
            .bind(permission.as_db_str())
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(role_id)
        .execute(pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(pool)
    .await
    .unwrap();
    (org_id, full_key)
}

#[tokio::test]
async fn create_role_succeeds_for_an_org_manage_roles_holder() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"analyst","permissions":["db:query"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_role_is_forbidden_without_org_manage_roles() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::DbQuery]).await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"analyst","permissions":["db:query"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// The `owner_type != "user"` guard lives inside `roles::require_permission`,
/// so it covers all five admin endpoints as well as the proxy path — a
/// workspace-owned key's `owner_id` names a workspace, not a user, and must
/// never be looked up as an `org_members.user_id`. Mirrors
/// `workspace_owned_key_is_forbidden_from_org_namespace_access` in
/// `tests/org_proxy_test.rs`.
#[tokio::test]
async fn workspace_owned_key_is_forbidden_from_the_admin_api() {
    let pool = test_pool().await;
    let (org_id, _) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;

    let owner_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_user_id)
        .bind(format!("wsadmin-{owner_user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let workspace_id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces (id, owner_user_id, name) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(owner_user_id)
        .bind(format!("workspace-{workspace_id}"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'workspace', $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_user_id)
    .bind(workspace_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"smuggled","permissions":["db:query"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// `roles` is `UNIQUE (org_id, name)`. A client re-using a name is a
/// correctable conflict, not a gateway fault — it must not surface as a 500.
#[tokio::test]
async fn create_role_rejects_a_duplicate_name_with_409() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));
    let name = format!("dupe-{}", Uuid::new_v4());
    let body = format!(r#"{{"name":"{name}","permissions":["db:query"]}}"#);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
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
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

/// `role_permissions` is `PRIMARY KEY (role_id, permission)`, so a repeated
/// permission in the request body used to reach the insert loop and 500.
/// It's collapsed before the insert instead — and the 201 body reports what
/// was actually stored, not what was asked for.
#[tokio::test]
async fn duplicate_permissions_in_the_body_are_deduped_not_a_500() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));
    let name = format!("deduped-{}", Uuid::new_v4());

    let create_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"{name}","permissions":["db:query","db:query"]}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(create_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        created["permissions"].as_array().unwrap().len(),
        1,
        "201 body must report the stored (deduped) permission set: {created}"
    );
    let role_id = created["id"].as_str().unwrap().to_string();

    let update_resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/orgs/{org_id}/roles/{role_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"permissions":["db:sync","db:sync"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_resp.status(), StatusCode::NO_CONTENT);

    let stored = roles::list_roles(&test_pool().await, org_id).await.unwrap();
    let role = stored
        .iter()
        .find(|r| r.id.to_string() == role_id)
        .expect("role should exist");
    assert_eq!(role.permissions, vec!["db:sync".to_string()]);
}

#[tokio::test]
async fn create_role_rejects_an_unknown_permission_string() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"bad","permissions":["db:delete"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_roles_returns_created_roles() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    // seed_admin's own bootstrap role shows up in the list.
    assert!(text.contains("org:manage_roles"));
}

#[tokio::test]
async fn update_role_replaces_its_permissions() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivewarden::app(AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let create_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"editable","permissions":["db:query"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(create_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let role_id = created["id"].as_str().unwrap();

    let update_resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/orgs/{org_id}/roles/{role_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"permissions":["db:sync"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn update_role_returns_404_for_an_unknown_role_id() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/orgs/{org_id}/roles/{}", Uuid::new_v4()))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"permissions":["db:sync"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_role_rejects_a_role_still_in_use() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    // seed_admin's own bootstrap role is held by the admin themselves.
    let role_id = roles::list_roles(&pool, org_id).await.unwrap()[0].id;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/orgs/{org_id}/roles/{role_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn assign_member_role_succeeds_for_an_org_manage_members_holder() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(
        &pool,
        &[Permission::OrgManageMembers, Permission::OrgManageRoles],
    )
    .await;
    let new_role_id = roles::create_role(&pool, org_id, "new-role", &[Permission::DbQuery])
        .await
        .unwrap();
    let bootstrap_role_id = roles::list_roles(&pool, org_id)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.id != new_role_id)
        .unwrap()
        .id;
    let target_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(target_user_id)
        .bind(format!("target-{target_user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(target_user_id)
        .bind(bootstrap_role_id)
        .execute(&pool)
        .await
        .unwrap();

    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/orgs/{org_id}/members/{target_user_id}/role"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"role_id":"{new_role_id}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn assign_member_role_returns_404_for_an_unknown_member() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(
        &pool,
        &[Permission::OrgManageMembers, Permission::OrgManageRoles],
    )
    .await;
    let role_id = roles::create_role(&pool, org_id, "role-x", &[Permission::DbQuery])
        .await
        .unwrap();

    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/orgs/{org_id}/members/{}/role", Uuid::new_v4()))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"role_id":"{role_id}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
