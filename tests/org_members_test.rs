use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivemind_gateway::auth::AppState;
use hivemind_gateway::db;
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
    let app = hivemind_gateway::app(state);

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
    let app = hivemind_gateway::app(state);

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
    let app = hivemind_gateway::app(state);

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
    let app = hivemind_gateway::app(state);

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
    let app = hivemind_gateway::app(state);

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
