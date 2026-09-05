mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use wardn::auth::AppState;
use wardn::db;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
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

async fn create_org(app: axum::Router, owner_key: &str, name: &str) -> String {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    created["org_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn successful_query_request_emits_a_usage_event() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("usage-{}@example.com", Uuid::new_v4())).await;

    let (resp, events) = common::capture_usage_events(|| {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
    })
    .await;
    let resp = resp.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        events.len(),
        1,
        "expected exactly one usage event: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["owner_type"], "user");
    assert_eq!(event["owner_id"], user_id.to_string());
    assert_eq!(event["org_id"], "");
    assert_eq!(event["namespace"], user_id.to_string());
    assert_eq!(event["protocol"], "query");
    assert_eq!(event["status"], "200");
    let ms: u64 = event
        .get("duration_ms")
        .unwrap_or_else(|| panic!("expected a duration_ms field: {event:?}"))
        .parse()
        .unwrap_or_else(|e| panic!("duration_ms should parse as a number: {e}"));
    assert!(
        ms < 30_000,
        "duration_ms should be well under the 30s response-head timeout: {ms}"
    );

    delete_namespace(&user_id.to_string()).await;
}

/// Pairs the rejected request with an accepted one inside the *same* capture:
/// asserting only `events.is_empty()` would pass just as happily if the
/// capture machinery itself were broken (a multi-threaded test runtime, a
/// drifted target string), which would make the "only requests that reach sqld
/// are billable" property untested rather than proven.
#[tokio::test]
async fn rejected_request_does_not_emit_a_usage_event() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("usage-{}@example.com", Uuid::new_v4())).await;

    let ((rejected, accepted), events) = common::capture_usage_events(|| async {
        let rejected = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let accepted = app
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
        (rejected, accepted)
    })
    .await;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(accepted.status(), StatusCode::OK);

    assert_eq!(
        events.len(),
        1,
        "only the accepted request may emit a usage event: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["owner_id"], user_id.to_string());
    assert_eq!(event["namespace"], user_id.to_string());
    assert_eq!(event["status"], "200");

    delete_namespace(&user_id.to_string()).await;
}

/// Accepts TCP connections and then holds them open forever, never writing a
/// byte back. That is precisely the shape `RESPONSE_HEAD_TIMEOUT` exists for:
/// the connection to sqld succeeds (so this is *not* the transport-error
/// `BAD_GATEWAY` arm), and hyper then waits for a response head that never
/// arrives.
///
/// Returns its URL plus a receiver that fires once per accepted connection —
/// the test needs to know when the proxy has got that far, see below.
async fn spawn_black_hole_sqld() -> (String, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
            let _ = accepted_tx.send(());
        }
    });
    (format!("http://{addr}"), accepted_rx)
}

/// A response-head timeout still reached sqld and consumed real time there, so
/// it is metered — see the emission comment in `src/proxy.rs`.
#[tokio::test]
async fn response_head_timeout_emits_a_504_usage_event() {
    let pool = test_pool().await;
    // Only the *proxy* leg points at the black hole; `sqld_admin_url` stays
    // real, since registration provisions the namespace through it.
    let (black_hole, mut accepted) = spawn_black_hole_sqld().await;
    let state = AppState::new(pool.clone(), black_hole, common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (user_id, api_key) =
        register(app.clone(), &format!("usage-{}@example.com", Uuid::new_v4())).await;

    let (resp, events) = common::capture_usage_events(|| async {
        let request = tokio::spawn(app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        ));

        // Wait until the proxy has actually opened its connection to the black
        // hole: that means auth and namespace resolution — real Postgres I/O —
        // are done, and `RESPONSE_HEAD_TIMEOUT`'s timer is armed.
        accepted.recv().await.expect("proxy never connected to sqld");

        // Only now switch to virtual time. From here the runtime has nothing
        // left to poll (the black hole will never answer), so tokio jumps the
        // clock straight to that 30s deadline and the real timeout arm runs in
        // milliseconds instead of sitting out half a minute. Pausing any
        // earlier makes tokio auto-advance through the *Postgres* wait too,
        // firing sqlx's own timeouts and failing the request with a 500.
        tokio::time::pause();

        request.await.unwrap().unwrap()
    })
    .await;
    assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);

    assert_eq!(
        events.len(),
        1,
        "a response-head timeout must emit exactly one usage event: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["status"], "504");
    assert_eq!(event["owner_id"], user_id.to_string());
    assert_eq!(event["namespace"], user_id.to_string());
    assert_eq!(event["protocol"], "query");

    delete_namespace(&user_id.to_string()).await;
}

#[tokio::test]
async fn org_shared_request_emits_org_id_field() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", Uuid::new_v4())).await;
    let org_id = create_org(app.clone(), &owner_key, &format!("Org-{}", Uuid::new_v4())).await;

    let (resp, events) = common::capture_usage_events(|| {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
    })
    .await;
    let resp = resp.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(events.len(), 1, "expected exactly one usage event: {events:?}");
    let event = &events[0];
    assert_eq!(event["org_id"], org_id);
    assert_eq!(event["namespace"], org_id);
    assert_eq!(event["protocol"], "query");

    delete_namespace(&org_id).await;
}

const GRPC_METHOD: &str = "/wal_log.ReplicationLog/Hello";
const EMPTY_GRPC_MESSAGE: [u8; 5] = [0, 0, 0, 0, 0];

/// Boots the real gateway on an ephemeral port with `axum::serve`, so the
/// server side of the h2c connection is genuinely exercised (`oneshot`
/// never touches the wire).
async fn spawn_gateway(pool: sqlx::PgPool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = wardn::app(
        AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url()),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Sends a minimal h2c gRPC call and returns its HTTP status.
async fn grpc_call(base: &str, api_key: &str) -> StatusCode {
    let mut builder = Client::builder(TokioExecutor::new());
    builder.http2_only(true);
    let client: Client<HttpConnector, Full<Bytes>> = builder.build(HttpConnector::new());

    let request = hyper::Request::builder()
        .method("POST")
        .uri(format!("{base}{GRPC_METHOD}"))
        .header(header::CONTENT_TYPE, "application/grpc")
        .header("te", "trailers")
        .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
        .body(Full::new(Bytes::from_static(&EMPTY_GRPC_MESSAGE)))
        .unwrap();

    let response = client.request(request).await.expect("gRPC request failed");
    let status = response.status();
    let _ = response.into_body().collect().await;
    status
}

#[tokio::test]
async fn sync_request_emits_protocol_sync() {
    let pool = test_pool().await;
    let namespace = format!("usagesync-{}", Uuid::new_v4());
    create_namespace(&namespace).await;

    let owner_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("usagesync-{owner_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
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
    let (full_key, prefix, hash) = wardn::auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
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

    let (status, events) = common::capture_usage_events(|| async move {
        let gateway = spawn_gateway(pool.clone()).await;
        grpc_call(&gateway, &full_key).await
    })
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        events.len(),
        1,
        "expected exactly one usage event: {events:?}"
    );
    assert_eq!(events[0]["protocol"], "sync");
    assert_eq!(events[0]["namespace"], namespace);

    delete_namespace(&namespace).await;
}
