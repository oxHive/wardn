use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivemind_gateway::auth::{self, AppState};
use hivemind_gateway::db;
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

#[tokio::test]
async fn valid_key_reaches_sqld_and_gets_a_real_response() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    let namespace = format!("proxytest-{owner_id}");
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'user', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&namespace)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("proxy-{owner_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let state = AppState { pool };
    let app = hivemind_gateway::app(state);

    // sqld exposes a version endpoint at GET /version on its default HTTP
    // listener — proxying it through confirms the request actually reached
    // sqld (not a gateway-side stub). /version itself isn't namespace-scoped
    // in this sqld build (verified empirically — it returns 200 regardless
    // of Host header), so this test's job is to confirm end-to-end reach,
    // not namespace routing correctness (that's exercised at the Postgres
    // resolution layer by the "no mapping" test below, and was verified
    // separately for the Host-header rewrite mechanism itself; see
    // task-5-report.md).
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/version")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn missing_key_never_reaches_sqld() {
    let pool = test_pool().await;
    let app = hivemind_gateway::app(AppState { pool });
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn valid_key_with_no_mapping_returns_404() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("nomap-{owner_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = hivemind_gateway::app(AppState { pool });
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/version")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
