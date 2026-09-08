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

/// Guards Decision 5's "'CLI Only' Does Not Mean 'No API'" section:
/// `wardn serve`'s API is narrowly authorization checks + a health/status
/// readout, never a general org/member/role/key admin surface — that stays
/// CLI/direct-DB only. These are exactly the routes the pre-Wardn
/// `hivemind-gateway` design exposed (`/orgs`, `/orgs/{id}/members`,
/// `/orgs/{id}/roles`, `/api-keys`, ...) plus the natural `/v1`-versioned
/// equivalents one might reach for next to `/v1/authorize` — if any of
/// these ever starts responding, an admin endpoint has crept back into
/// this service and this test should fail.
#[tokio::test]
async fn no_admin_management_endpoints_exist() {
    let (_dir, db) = common::temp_db().await;
    org::create(&db.conn, "Acme").await.unwrap();
    let app = serve::app(serve::AppState::new(db.conn, wardn::db::now()));

    let forbidden_paths = [
        // The old hivemind-gateway's actual admin API shape.
        "/orgs",
        "/orgs/org-id",
        "/orgs/org-id/members",
        "/orgs/org-id/members/user-id",
        "/orgs/org-id/members/user-id/role",
        "/orgs/org-id/roles",
        "/orgs/org-id/roles/role-id",
        "/api-keys",
        "/api-keys/key-id",
        "/users",
        // Natural `/v1`-versioned admin-API guesses, alongside the one
        // narrow endpoint (/v1/authorize) that legitimately exists.
        "/v1/org",
        "/v1/orgs",
        "/v1/members",
        "/v1/members/member-id",
        "/v1/roles",
        "/v1/keys",
        "/v1/keys/key-id",
    ];

    for path in forbidden_paths {
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{method} {path} should not exist on wardn serve's API"
            );
        }
    }
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
