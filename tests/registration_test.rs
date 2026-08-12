use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivemind_gateway::auth::AppState;
use hivemind_gateway::db;
use tower::ServiceExt;

mod common;

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

#[tokio::test]
async fn create_user_returns_a_working_key_and_provisions_a_namespace() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = hivemind_gateway::app(state);

    let email = format!("register-{}@example.com", uuid::Uuid::new_v4());
    let resp = app
        .clone()
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
    let user_id = created["user_id"].as_str().unwrap().to_string();
    let api_key = created["api_key"].as_str().unwrap();
    assert!(api_key.starts_with(hivemind_gateway::auth::KEY_MARKER));

    // Prove the namespace was actually provisioned: write through it, read
    // it back, via the real router, exactly the way a real client would.
    let secret = format!("REG-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "write through the new namespace failed"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
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

    delete_namespace(&user_id).await;
}

#[tokio::test]
async fn create_user_still_succeeds_when_inline_provisioning_fails() {
    let pool = test_pool().await;
    // This is the one registration test that leaves a row `pending` and then
    // asserts on it, so it contends with `provisioning_test`'s worker test —
    // which runs in a *different* binary, hence a Postgres advisory lock
    // rather than a process-local `Mutex`. See `tests/common/mod.rs`.
    let _lock = common::lock_outbox(&pool).await;
    // Unreachable admin URL — the inline attempt inside create_user must
    // fail without failing the request itself.
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url("http://127.0.0.1:1".to_string());
    let app = hivemind_gateway::app(state);

    let email = format!("register-fail-{}@example.com", uuid::Uuid::new_v4());
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

    let (outbox_id, status, attempts): (uuid::Uuid, String, i32) = sqlx::query_as(
        "SELECT id, status, attempts FROM namespace_provisioning_outbox
         WHERE owner_type = 'user' AND owner_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(attempts, 1);

    // Deliberately not left behind: a surviving `pending` row is exactly what
    // a *later* run's worker test would sweep up and provision for real.
    common::delete_outbox_row(&pool, outbox_id).await;
}

#[tokio::test]
async fn create_user_with_an_already_registered_email_returns_409() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = hivemind_gateway::app(state);

    let email = format!("dupe-{}@example.com", uuid::Uuid::new_v4());
    let register = || {
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                .unwrap(),
        )
    };

    let first = register().await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id = created["user_id"].as_str().unwrap().to_string();

    // `users.email` is UNIQUE: the second attempt is a client-correctable
    // conflict, not a gateway fault, and must not look like a 500.
    let second = register().await.unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        "email already registered"
    );

    delete_namespace(&user_id).await;
}

async fn seed_registered_user(app: axum::Router) -> (uuid::Uuid, String) {
    let email = format!("orgcreator-{}@example.com", uuid::Uuid::new_v4());
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
async fn create_org_provisions_a_namespace_and_makes_the_creator_its_owner() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = hivemind_gateway::app(state);

    let (_user_id, api_key) = seed_registered_user(app.clone()).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"Acme-{}"}}"#,
                    uuid::Uuid::new_v4()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let org_id = created["org_id"].as_str().unwrap().to_string();

    // The creator's own key, plus X-Org-Id, must already reach the new
    // org's namespace — proving the bootstrap role + membership landed.
    let secret = format!("ORG-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "write into the new org's namespace failed"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
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

    delete_namespace(&org_id).await;
}

#[tokio::test]
async fn create_org_is_forbidden_for_a_non_user_owned_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = hivemind_gateway::app(state);

    // A workspace-owned key: real row, valid hash, but owner_type != "user".
    let workspace_id = uuid::Uuid::new_v4();
    let user_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("wsowner-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces (id, owner_user_id, name) VALUES ($1, $2, 'ws')")
        .bind(workspace_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = hivemind_gateway::auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'workspace', $3, $4, $5)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_id)
    .bind(workspace_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"Nope-{}"}}"#,
                    uuid::Uuid::new_v4()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
