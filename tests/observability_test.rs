mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivemind_gateway::auth::AppState;
use hivemind_gateway::db;
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

fn admin_url() -> String {
    std::env::var("SQLD_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".to_string())
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text_unauthenticated() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool, test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle);
    let app = hivemind_gateway::app(state);

    // No Authorization header — proves this route sits outside auth_middleware.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp.headers().get(header::CONTENT_TYPE).unwrap();
    assert!(content_type.to_str().unwrap().starts_with("text/plain"));
}

async fn register(app: axum::Router, email: &str) -> (Uuid, String) {
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
    let user_id: Uuid = created["user_id"].as_str().unwrap().parse().unwrap();
    let api_key = created["api_key"].as_str().unwrap().to_string();
    (user_id, api_key)
}

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

#[tokio::test]
async fn successful_request_increments_counter_and_histogram() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle.clone());
    let app = hivemind_gateway::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("obs-{}@example.com", Uuid::new_v4())).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let rendered = handle.render();
    let namespace_label = format!("namespace=\"{user_id}\"");
    assert!(
        common::has_labeled_metric(
            &rendered,
            "gateway_proxy_requests_total",
            &["protocol=\"query\"", "status_class=\"2xx\"", &namespace_label],
        ),
        "missing counter sample: {rendered}"
    );
    assert!(
        common::has_labeled_metric(
            &rendered,
            "gateway_proxy_request_duration_seconds_count",
            &["protocol=\"query\"", &namespace_label],
        ),
        "missing histogram sample: {rendered}"
    );

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn in_flight_gauge_returns_to_baseline_after_request_completes() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle.clone());
    let app = hivemind_gateway::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("obs-{}@example.com", Uuid::new_v4())).await;

    let baseline =
        common::extract_unlabeled_metric(&handle.render(), "gateway_proxy_requests_in_flight");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        common::extract_unlabeled_metric(&handle.render(), "gateway_proxy_requests_in_flight"),
        baseline,
        "gauge did not return to baseline after a successful request"
    );

    // Rejected before reaching sqld (no Authorization header) — the guard
    // must still fire even on a path that never calls record_proxy_metrics.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        common::extract_unlabeled_metric(&handle.render(), "gateway_proxy_requests_in_flight"),
        baseline,
        "gauge did not return to baseline after a rejected request"
    );

    delete_namespace(&user_id.to_string()).await;
}
