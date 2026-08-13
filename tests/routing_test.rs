use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use axum::{Extension, Router};
use hivewarden::auth::{self, AppState, AuthedOwner};
use hivewarden::{db, routing};
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

async fn whereami(
    axum::extract::State(state): axum::extract::State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
) -> Result<String, StatusCode> {
    routing::resolve_namespace(&state.pool, &owner).await
}

fn test_app(pool: sqlx::PgPool) -> Router {
    Router::new()
        .route("/whereami", get(whereami))
        .layer(axum::middleware::from_fn_with_state(
            AppState::new(pool.clone(), test_sqld_url()),
            auth::auth_middleware,
        ))
        .with_state(AppState::new(pool, test_sqld_url()))
}

async fn seed_valid_key(pool: &sqlx::PgPool, owner_type: &str, owner_id: Uuid) -> String {
    let user_id = if owner_type == "user" { owner_id } else { Uuid::new_v4() };
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(user_id)
        .bind(format!("route-{user_id}@example.com"))
        .execute(pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(owner_type)
    .bind(owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(pool)
    .await
    .unwrap();
    full_key
}

#[tokio::test]
async fn resolves_namespace_for_mapped_owner() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'user', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(format!("ns-{owner_id}"))
    .execute(&pool)
    .await
    .unwrap();
    let full_key = seed_valid_key(&pool, "user", owner_id).await;

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whereami")
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
        format!("ns-{owner_id}")
    );
}

#[tokio::test]
async fn returns_404_when_owner_has_no_mapping() {
    let pool = test_pool().await;
    let owner_id = Uuid::new_v4();
    let full_key = seed_valid_key(&pool, "user", owner_id).await;

    let app = test_app(pool);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/whereami")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
