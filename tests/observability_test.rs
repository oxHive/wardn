mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;
use wardn::auth::AppState;
use wardn::db;
use wardn::observability;

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
    let state = AppState::new(
        pool,
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url())
    .with_metrics_handle(handle)
    .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = wardn::app(state);

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

// Pins the fail-closed default itself: builds an `AppState` via `AppState::new`
// alone, without `.with_metrics_token(...)`, so `metrics_token` is left at its
// empty-string default. Every other test in this file calls
// `.with_metrics_token(...)`, so none of them actually prove that the default
// rejects requests — including the specific bypass risk that `Authorization:
// Bearer ` (trailing space, empty credential) parses to `Some("")`, which
// would compare equal to an empty `state.metrics_token` if the `is_empty()`
// guard in `metrics_handler` were ever accidentally removed. Without this
// test, deleting that guard would leave every other test in this file green.
#[tokio::test]
async fn metrics_endpoint_rejects_when_token_unset() {
    let pool = test_pool().await;
    let state = AppState::new(
        pool,
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    // No Authorization header at all.
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

    // `Authorization: Bearer ` — trailing space, empty credential after
    // stripping the "Bearer " prefix. This parses to `Some("")`, which must
    // still be rejected even though `state.metrics_token` is also `""`.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(header::AUTHORIZATION, "Bearer ")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url())
    .with_metrics_handle(handle.clone())
    .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = wardn::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("obs-{}@example.com", Uuid::new_v4())).await;

    // The recorder is a shared process-wide static (`common::metrics_handle()`),
    // so a bare presence check after the request can't prove *this* request
    // caused it — a sibling test hitting the same `protocol`/`status_class`
    // combination (e.g. `in_flight_gauge_returns_to_baseline_...` below) can
    // satisfy a presence check on its own even if this request recorded
    // nothing. Snapshotting the counter/histogram-count under the lock,
    // before the request, and asserting an exact +1 delta afterward proves
    // the request itself is what moved them.
    let _guard = common::proxy_metrics_lock();
    let before = handle.render();
    let counter_before = common::extract_labeled_metric(
        &before,
        "gateway_proxy_requests_total",
        &["protocol=\"query\"", "status_class=\"2xx\""],
    )
    .unwrap_or(0.0);
    let histogram_count_before = common::extract_labeled_metric(
        &before,
        "gateway_proxy_request_duration_seconds_count",
        &["protocol=\"query\""],
    )
    .unwrap_or(0.0);

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
    let counter_after = common::extract_labeled_metric(
        &rendered,
        "gateway_proxy_requests_total",
        &["protocol=\"query\"", "status_class=\"2xx\""],
    )
    .unwrap_or(0.0);
    assert_eq!(
        counter_after,
        counter_before + 1.0,
        "gateway_proxy_requests_total{{protocol=\"query\",status_class=\"2xx\"}} did not \
         increase by exactly 1: {rendered}"
    );
    assert!(
        !rendered.contains("gateway_proxy_requests_total{")
            || !rendered.lines().any(|line| {
                line.starts_with("gateway_proxy_requests_total{") && line.contains("namespace=")
            }),
        "gateway_proxy_requests_total must not carry a namespace label: {rendered}"
    );
    let histogram_count_after = common::extract_labeled_metric(
        &rendered,
        "gateway_proxy_request_duration_seconds_count",
        &["protocol=\"query\""],
    )
    .unwrap_or(0.0);
    assert_eq!(
        histogram_count_after,
        histogram_count_before + 1.0,
        "gateway_proxy_request_duration_seconds_count{{protocol=\"query\"}} did not increase \
         by exactly 1: {rendered}"
    );
    drop(_guard);

    delete_namespace(&user_id.to_string()).await;
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn in_flight_gauge_returns_to_baseline_after_request_completes() {
    let pool = test_pool().await;
    let handle = common::metrics_handle();
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url())
    .with_metrics_handle(handle.clone())
    .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = wardn::app(state);

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
    let state = AppState::new(
        pool,
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url())
    .with_metrics_handle(handle)
    .with_metrics_token(common::TEST_METRICS_TOKEN.to_string());
    let app = wardn::app(state);

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
