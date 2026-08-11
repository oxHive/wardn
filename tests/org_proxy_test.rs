use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use hivemind_gateway::auth::{self, AppState};
use hivemind_gateway::db;
use hivemind_gateway::roles::Permission;
use http_body_util::Full;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
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

async fn create_namespace(name: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/namespaces/{name}/create", admin_url()))
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .expect("admin API create-namespace request failed");
    assert!(resp.status().is_success(), "failed to create {name}");
}

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

/// Seeds an org mapped to `namespace`, a user with a personal key, a role
/// granting `permissions`, and an org_members row assigning that role.
/// Returns (org_id, full_key).
async fn seed_org_member(
    pool: &sqlx::PgPool,
    namespace: &str,
    permissions: &[Permission],
) -> (Uuid, String) {
    let org_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let role_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'org', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(org_id)
    .bind(namespace)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("orgmember-{user_id}@example.com"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO roles (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(role_id)
        .bind(org_id)
        .bind(format!("role-{role_id}"))
        .execute(pool)
        .await
        .unwrap();
    for permission in permissions {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, $2)")
            .bind(role_id)
            .bind(permission.as_db_str())
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(org_id)
        .bind(user_id)
        .bind(role_id)
        .execute(pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(pool)
    .await
    .unwrap();
    (org_id, full_key)
}

async fn query_org_via_gateway(
    app: axum::Router,
    full_key: &str,
    org_id: Uuid,
    statements_json: &str,
) -> StatusCode {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", org_id.to_string())
                .body(Body::from(statements_json.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn member_with_db_query_reaches_the_orgs_namespace() {
    let pool = test_pool().await;
    let namespace = format!("orgqns-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let (org_id, key) = seed_org_member(&pool, &namespace, &[Permission::DbQuery]).await;

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
    let status = query_org_via_gateway(app, &key, org_id, r#"{"statements":["SELECT 1"]}"#).await;
    assert_eq!(status, StatusCode::OK);

    delete_namespace(&namespace).await;
}

#[tokio::test]
async fn member_without_db_query_is_forbidden() {
    let pool = test_pool().await;
    let namespace = format!("orgnq-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let (org_id, key) = seed_org_member(&pool, &namespace, &[Permission::DbSync]).await;

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
    let status = query_org_via_gateway(app, &key, org_id, r#"{"statements":["SELECT 1"]}"#).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    delete_namespace(&namespace).await;
}

#[tokio::test]
async fn non_member_is_forbidden() {
    let pool = test_pool().await;
    let namespace = format!("orgnm-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'org', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(org_id)
    .bind(&namespace)
    .execute(&pool)
    .await
    .unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("outsider-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
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

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
    let status =
        query_org_via_gateway(app, &full_key, org_id, r#"{"statements":["SELECT 1"]}"#).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    delete_namespace(&namespace).await;
}

#[tokio::test]
async fn malformed_org_id_header_is_a_bad_request() {
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("malformed-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
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

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .header("x-org-id", "not-a-uuid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Boots the real gateway with `axum::serve` so the h2c leg is genuinely
/// exercised (`oneshot` never touches the wire — see grpc_proxy_test.rs).
async fn spawn_gateway(pool: sqlx::PgPool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

const GRPC_METHOD: &str = "/wal_log.ReplicationLog/Hello";
const EMPTY_GRPC_MESSAGE: [u8; 5] = [0, 0, 0, 0, 0];

/// Sends a minimal h2c gRPC call and returns the HTTP status. A permission
/// rejection never reaches sqld at all (the gateway returns 403 directly),
/// so the plain HTTP status — not any gRPC-level status — is what
/// distinguishes "blocked" from "proxied through".
async fn grpc_status_via_h2c(base: &str, extra: &[(&str, &str)]) -> StatusCode {
    let mut builder = HyperClient::builder(TokioExecutor::new());
    builder.http2_only(true);
    let client: HyperClient<HttpConnector, Full<Bytes>> = builder.build(HttpConnector::new());

    let mut request = hyper::Request::builder()
        .method("POST")
        .uri(format!("{base}{GRPC_METHOD}"))
        .header(header::CONTENT_TYPE, "application/grpc")
        .header("te", "trailers");
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let request = request
        .body(Full::new(Bytes::from_static(&EMPTY_GRPC_MESSAGE)))
        .unwrap();
    client
        .request(request)
        .await
        .expect("gRPC request failed")
        .status()
}

#[tokio::test]
async fn member_with_db_sync_reaches_the_orgs_namespace_over_h2c() {
    let pool = test_pool().await;
    let namespace = format!("orgsyncns-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let (org_id, key) = seed_org_member(&pool, &namespace, &[Permission::DbSync]).await;
    let gateway = spawn_gateway(pool).await;

    let status = grpc_status_via_h2c(
        &gateway,
        &[
            (header::AUTHORIZATION.as_str(), &format!("Bearer {key}")),
            ("x-org-id", &org_id.to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    delete_namespace(&namespace).await;
}

#[tokio::test]
async fn member_without_db_sync_is_forbidden_over_h2c() {
    let pool = test_pool().await;
    let namespace = format!("orgnosyncns-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let (org_id, key) = seed_org_member(&pool, &namespace, &[Permission::DbQuery]).await;
    let gateway = spawn_gateway(pool).await;

    let status = grpc_status_via_h2c(
        &gateway,
        &[
            (header::AUTHORIZATION.as_str(), &format!("Bearer {key}")),
            ("x-org-id", &org_id.to_string()),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    delete_namespace(&namespace).await;
}
