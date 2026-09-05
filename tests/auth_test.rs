mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use axum::{Extension, Router};
use wardn::auth::{self, AppState, AuthedOwner};
use wardn::db;
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

async fn whoami(Extension(owner): Extension<AuthedOwner>) -> String {
    format!("{}:{}", owner.owner_type, owner.owner_id)
}

fn test_app(pool: sqlx::PgPool) -> Router {
    Router::new()
        .route("/whoami", get(whoami))
        .layer(axum::middleware::from_fn_with_state(
            AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()),
            wardn::auth::auth_middleware,
        ))
        .with_state(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()))
}

#[tokio::test]
async fn valid_key_resolves_owner() {
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("valid-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        format!("user:{user_id}")
    );
}

#[tokio::test]
async fn missing_key_returns_401() {
    let pool = test_pool().await;
    let app = test_app(pool);
    let resp = app
        .oneshot(Request::builder().uri("/whoami").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_key_returns_401() {
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("wrong-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (_full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whoami")
                // A token whose *prefix* really is in the database (so the
                // lookup hits a row) but whose secret half is wrong — this is
                // the hash-mismatch path, not the unknown-prefix path.
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}{prefix}wrongsuffixwrongsuffix", auth::KEY_MARKER),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revoked_key_returns_401() {
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("revoked-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash, revoked_at)
         VALUES ($1, $2, 'user', $2, $3, $4, now())",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whoami")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Proves the pepper is actually load-bearing in `verify_key` — every other
/// test in this suite uses exactly one pepper value throughout, so on its
/// own the suite would still pass even if `hash_key` silently ignored its
/// `pepper` argument. Also covers two rejection paths no other test hits: a
/// well-formed-length-but-wrong hex hash, and a hash string that isn't hex
/// at all (`hex::decode` failure).
#[tokio::test]
async fn a_key_does_not_verify_under_a_different_pepper() {
    let (full_key, _, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    assert!(auth::verify_key(
        common::TEST_API_KEY_PEPPER.as_bytes(),
        &full_key,
        &hash
    ));
    assert!(!auth::verify_key(
        b"a-different-pepper-at-least-32-chars-long",
        &full_key,
        &hash
    ));
    assert!(!auth::verify_key(
        common::TEST_API_KEY_PEPPER.as_bytes(),
        &full_key,
        "deadbeef"
    ));
    assert!(!auth::verify_key(
        common::TEST_API_KEY_PEPPER.as_bytes(),
        &full_key,
        "not-hex-at-all"
    ));
}
