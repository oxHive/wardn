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

#[tokio::test]
async fn list_keys_shows_the_registration_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url()).with_sqld_admin_url(admin_url());
    let app = hivemind_gateway::app(state);

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
    let app = hivemind_gateway::app(state);

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
    let app = hivemind_gateway::app(state);

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
    let app = hivemind_gateway::app(state);

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
