#![cfg(feature = "observability")]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use wardn::{org, serve};

/// `metrics_exporter_prometheus::PrometheusBuilder::install_recorder` sets
/// a process-global recorder and errors if called twice, so every test in
/// this file that needs a handle must build one locally (never installed)
/// rather than sharing a real installed one across tests.
fn local_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

#[tokio::test]
async fn metrics_route_404s_when_not_configured() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    let app = serve::app(serve::AppState::new(db.conn, wardn::db::now()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metrics_route_requires_the_configured_token() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    let state = serve::AppState::new(db.conn, wardn::db::now())
        .with_metrics(local_handle(), "s3cret".to_string());
    let app = serve::app(state);

    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let authenticated = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header("authorization", "Bearer s3cret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authenticated.status(), StatusCode::OK);
}
