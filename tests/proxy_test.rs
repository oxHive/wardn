mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use hivewarden::auth::{self, AppState};
use hivewarden::db;
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

/// Creates a sqld namespace via the admin API (`sqld --admin-listen-addr`,
/// wired up in `podman-compose.yml` for this test). Verified directly
/// against the running container: `POST /v1/namespaces/{name}/create`
/// returns 200 with an empty body on success.
async fn create_namespace(client: &reqwest::Client, name: &str) {
    let resp = client
        .post(format!("{}/v1/namespaces/{name}/create", admin_url()))
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .expect("admin API create-namespace request failed");
    assert!(
        resp.status().is_success(),
        "failed to create namespace {name}: {}",
        resp.status()
    );
}

/// Deletes a sqld namespace via the admin API. Best-effort test cleanup —
/// panics are avoided so a prior assertion failure doesn't mask itself.
async fn delete_namespace(client: &reqwest::Client, name: &str) {
    let _ = client
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

/// Seeds a fresh user + api_key + database_mappings row pointing at
/// `namespace`. Returns the full (unhashed) API key.
async fn seed_owner_with_namespace(pool: &sqlx::PgPool, namespace: &str) -> String {
    let owner_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("isolation-{owner_id}@example.com"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO database_mappings (id, owner_type, owner_id, sqld_namespace)
         VALUES ($1, 'user', $2, $3)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(namespace)
    .execute(pool)
    .await
    .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'user', $2, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(pool)
    .await
    .unwrap();
    full_key
}

/// Sends a query through the real gateway router (auth + namespace
/// resolution + proxy_handler, exactly as a real client would), returning
/// the raw response body text.
async fn query_through_gateway(
    app: axum::Router,
    full_key: &str,
    statements_json: &str,
) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(statements_json.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
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
    let (full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
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

    let state = AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string());
    let app = hivewarden::app(state);

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

/// Proves actual namespace isolation through the full router — not just
/// "a 200 came back" (that's `valid_key_reaches_sqld_and_gets_a_real_response`
/// above, which deliberately hits the non-namespace-scoped `/version`
/// endpoint and can't distinguish correct namespace routing from broken
/// namespace routing). Two owners, each mapped to a distinct real sqld
/// namespace (created via sqld's admin API — `--admin-listen-addr`, wired
/// up in `podman-compose.yml`). Each writes a value containing their own
/// uuid through their own key via `app(state)`, then each reads back
/// through their own key and must see only their own value.
#[tokio::test]
async fn namespace_isolation_through_full_router() {
    let pool = test_pool().await;
    let http = reqwest::Client::new();

    let owner_a_id = Uuid::new_v4();
    let owner_b_id = Uuid::new_v4();
    let namespace_a = format!("isotest-a-{owner_a_id}");
    let namespace_b = format!("isotest-b-{owner_b_id}");

    create_namespace(&http, &namespace_a).await;
    create_namespace(&http, &namespace_b).await;

    let key_a = seed_owner_with_namespace(&pool, &namespace_a).await;
    let key_b = seed_owner_with_namespace(&pool, &namespace_b).await;

    let secret_a = format!("A-secret-{owner_a_id}");
    let secret_b = format!("B-secret-{owner_b_id}");

    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    // Owner A creates their table and writes their secret.
    let create_and_insert_a = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret_a}')"]}}"#
    );
    let (status, _) = query_through_gateway(app.clone(), &key_a, &create_and_insert_a).await;
    assert_eq!(status, StatusCode::OK, "owner A's write failed");

    // Owner B creates their (separate-namespace) table and writes their
    // secret — same table name, different namespace.
    let create_and_insert_b = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret_b}')"]}}"#
    );
    let (status, _) = query_through_gateway(app.clone(), &key_b, &create_and_insert_b).await;
    assert_eq!(status, StatusCode::OK, "owner B's write failed");

    // Owner A reads back through their own key/routing — must see only
    // their own secret, never B's.
    let select_all = r#"{"statements":["SELECT v FROM kv"]}"#;
    let (status, body_a) = query_through_gateway(app.clone(), &key_a, select_all).await;
    assert_eq!(status, StatusCode::OK, "owner A's read failed");
    assert!(
        body_a.contains(&secret_a),
        "owner A's read did not contain their own secret: {body_a}"
    );
    assert!(
        !body_a.contains(&secret_b),
        "owner A's read leaked owner B's secret (namespace isolation broken): {body_a}"
    );

    // Owner B reads back through their own key/routing — must see only
    // their own secret, never A's.
    let (status, body_b) = query_through_gateway(app.clone(), &key_b, select_all).await;
    assert_eq!(status, StatusCode::OK, "owner B's read failed");
    assert!(
        body_b.contains(&secret_b),
        "owner B's read did not contain their own secret: {body_b}"
    );
    assert!(
        !body_b.contains(&secret_a),
        "owner B's read leaked owner A's secret (namespace isolation broken): {body_b}"
    );

    delete_namespace(&http, &namespace_a).await;
    delete_namespace(&http, &namespace_b).await;
}

/// Sends a query through the gateway with attacker-chosen extra headers.
async fn query_with_headers(
    app: axum::Router,
    full_key: &str,
    statements_json: &str,
    extra: &[(&str, String)],
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/")
        .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in extra {
        builder = builder.header(*name, value);
    }
    let resp = app
        .oneshot(builder.body(Body::from(statements_json.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// sqld honours a second namespace selector besides `Host`:
/// `x-namespace-bin`, carrying unpadded base64 of the namespace name — and it
/// *overrides* `Host`. Verified directly against the container: a request with
/// `Host: a.local` and `x-namespace-bin: <base64 of b>` reads namespace `b`.
///
/// So it is not enough for the gateway to set `Host`; it must also strip and
/// replace any client-supplied `x-namespace-bin`, or an authenticated tenant
/// reads any other tenant's database with its own valid key. This test runs
/// the real attack through the real router and asserts the caller only ever
/// sees their own data.
#[tokio::test]
async fn client_cannot_smuggle_a_namespace_selector() {
    let pool = test_pool().await;
    let http = reqwest::Client::new();

    let attacker_id = Uuid::new_v4();
    let victim_id = Uuid::new_v4();
    let attacker_ns = format!("smug-att-{attacker_id}");
    let victim_ns = format!("smug-vic-{victim_id}");
    create_namespace(&http, &attacker_ns).await;
    create_namespace(&http, &victim_ns).await;

    let attacker_key = seed_owner_with_namespace(&pool, &attacker_ns).await;
    let victim_key = seed_owner_with_namespace(&pool, &victim_ns).await;

    let victim_secret = format!("VICTIM-{victim_id}");
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));

    let (status, _) = query_through_gateway(
        app.clone(),
        &victim_key,
        &format!(
            r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{victim_secret}')"]}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "victim's seed write failed");

    // The attacker points every selector they control at the victim.
    let victim_bin = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&victim_ns);
    let (_, body) = query_with_headers(
        app.clone(),
        &attacker_key,
        r#"{"statements":["SELECT v FROM kv"]}"#,
        &[
            ("x-namespace-bin", victim_bin),
            (header::HOST.as_str(), format!("{victim_ns}.local")),
        ],
    )
    .await;
    assert!(
        !body.contains(&victim_secret),
        "a client-supplied namespace selector reached another tenant's data: {body}"
    );

    delete_namespace(&http, &attacker_ns).await;
    delete_namespace(&http, &victim_ns).await;
}

#[tokio::test]
async fn missing_key_never_reaches_sqld() {
    let pool = test_pool().await;
    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));
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
    let (full_key, prefix, hash) = auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
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

    let app = hivewarden::app(AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()));
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
