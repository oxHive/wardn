mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use wardn::AppState;
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

#[tokio::test]
async fn allowed_origin_gets_the_cors_header() {
    let pool = test_pool().await;
    let app = wardn::app(
        AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string())
            .with_cors_origins(vec!["http://localhost:5173".to_string()]),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("origin", "http://localhost:5173")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "http://localhost:5173"
    );
}

#[tokio::test]
async fn disallowed_origin_gets_no_cors_header() {
    let pool = test_pool().await;
    let app = wardn::app(
        AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string())
            .with_cors_origins(vec!["http://localhost:5173".to_string()]),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("origin", "http://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn request_with_no_origin_is_unaffected() {
    let pool = test_pool().await;
    let app = wardn::app(AppState::new(
        pool,
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    ));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}
