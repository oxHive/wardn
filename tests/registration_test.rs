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

    let (status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempts FROM namespace_provisioning_outbox
         WHERE owner_type = 'user' AND owner_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(attempts, 1);
}
