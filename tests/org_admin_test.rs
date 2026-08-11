use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivemind_gateway::auth::{self, AppState};
use hivemind_gateway::db;
use hivemind_gateway::roles::{self, Permission};
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
    let (full_key, prefix, hash) = auth::generate_api_key();
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
    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

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
    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

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

#[tokio::test]
async fn create_role_rejects_an_unknown_permission_string() {
    let pool = test_pool().await;
    let (org_id, key) = seed_admin(&pool, &[Permission::OrgManageRoles]).await;
    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

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
    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

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
    let app = hivemind_gateway::app(AppState::new(pool.clone(), test_sqld_url()));

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
    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

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
    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

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

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
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

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
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
