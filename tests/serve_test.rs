mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use wardn::{members, org, roles::Role, serve};

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn authorize_request(api_key: &str, action: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/authorize")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"api_key": api_key, "action": action}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn healthz_returns_ok() {
    let (_dir, db) = common::temp_db().await;
    let app = serve::app(serve::AppState::new(db.conn, wardn::db::now()));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn admin_key_is_authorized_to_read_and_write() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    let admin = members::invite(&db.conn, "admin@example.com", Role::Admin, None)
        .await
        .unwrap();
    let (_key, admin_key) = wardn::api_keys::create(&db.conn, &admin.id, None)
        .await
        .unwrap();
    let app = serve::app(serve::AppState::new(db.conn, wardn::db::now()));

    let response = app
        .oneshot(authorize_request(&admin_key, "write"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["allowed"], true);
    assert_eq!(body["role"], "admin");
}

#[tokio::test]
async fn read_only_key_cannot_write() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    let reader = members::invite(&db.conn, "reader@example.com", Role::ReadOnly, None)
        .await
        .unwrap();
    let (_key, reader_key) = wardn::api_keys::create(&db.conn, &reader.id, None)
        .await
        .unwrap();
    let app = serve::app(serve::AppState::new(db.conn, wardn::db::now()));

    let read_response = app
        .clone()
        .oneshot(authorize_request(&reader_key, "read"))
        .await
        .unwrap();
    assert_eq!(body_json(read_response).await["allowed"], true);

    let write_response = app
        .oneshot(authorize_request(&reader_key, "write"))
        .await
        .unwrap();
    assert_eq!(body_json(write_response).await["allowed"], false);
}

#[tokio::test]
async fn unknown_key_is_unauthorized() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    let app = serve::app(serve::AppState::new(db.conn, wardn::db::now()));

    let response = app
        .oneshot(authorize_request("wd_doesnotexist", "read"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn status_reports_org_name_and_member_count() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    members::invite(&db.conn, "one@example.com", Role::Member, None)
        .await
        .unwrap();
    let app = serve::app(serve::AppState::new(db.conn, wardn::db::now()));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["org_name"], "Acme");
    assert_eq!(body["member_count"], 1);
}
