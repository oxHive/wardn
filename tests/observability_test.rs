mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivemind_gateway::auth::AppState;
use hivemind_gateway::db;
use hivemind_gateway::observability;
use std::time::Duration;
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
async fn metrics_endpoint_requires_valid_bearer_token() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool, test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle)
        .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = hivemind_gateway::app(state);

    // No Authorization header at all — rejected by metrics_handler's own
    // token check (this route sits outside auth_middleware, so it must
    // check for itself).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong token.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(header::AUTHORIZATION, "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Correct token.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", common::TEST_METRICS_TOKEN),
                )
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

// Holds a `std::sync::MutexGuard` (`common::proxy_metrics_lock()`) across
// `.await` below — safe here, and in `in_flight_gauge_returns_to_baseline_...`
// further down, for the same reason `capture_usage_events` in
// `tests/common/mod.rs` documents: `#[tokio::test]` defaults to a
// single-threaded runtime, so this task never moves to a different OS thread
// mid-poll and there is no sibling task on this runtime the lock could block.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn successful_request_increments_counter_and_histogram() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle.clone())
        .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = hivemind_gateway::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("obs-{}@example.com", Uuid::new_v4())).await;

    let _guard = common::proxy_metrics_lock();
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
    drop(_guard);

    delete_namespace(&user_id.to_string()).await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn in_flight_gauge_returns_to_baseline_after_request_completes() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool.clone(), test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle.clone())
        .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = hivemind_gateway::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("obs-{}@example.com", Uuid::new_v4())).await;

    let _guard = common::proxy_metrics_lock();
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

    // Authenticated, but rejected *inside* `proxy_handler` itself (a
    // malformed `x-org-id` hits the `Some(Err(())) => BAD_REQUEST` arm in
    // `src/proxy.rs`, after `InFlightGuard::new()` has already run) — proves
    // the guard decrements on an early-return path inside the handler, not
    // just on the happy path. A request rejected by `auth_middleware` before
    // it ever reaches `proxy_handler` wouldn't exercise the guard at all, so
    // that case doesn't belong here.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header("x-org-id", "not-a-uuid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        common::extract_unlabeled_metric(&handle.render(), "gateway_proxy_requests_in_flight"),
        baseline,
        "gauge did not return to baseline after a request rejected inside proxy_handler"
    );
    drop(_guard);

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn metrics_endpoint_reports_pg_pool_gauges() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(pool, test_sqld_url())
        .with_sqld_admin_url(admin_url())
        .with_metrics_handle(handle)
        .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = hivemind_gateway::app(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", common::TEST_METRICS_TOKEN),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let rendered = String::from_utf8(body.to_vec()).unwrap();

    // `set()` registers the metric even at value 0 — presence, not
    // magnitude, is what a freshly connected pool can promise.
    assert!(rendered.contains("gateway_pg_pool_size "));
    assert!(rendered.contains("gateway_pg_pool_idle "));
}

#[tokio::test]
async fn check_sqld_up_true_for_reachable_sqld() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(observability::check_sqld_up(&client, &test_sqld_url()).await);
}

#[tokio::test]
async fn check_sqld_up_false_for_unreachable_sqld() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    assert!(!observability::check_sqld_up(&client, "http://127.0.0.1:1").await);
}
