//! Proves the gateway can carry hivemind's real embedded-replica sync
//! traffic: gRPC over cleartext HTTP/2 (h2c), on the same sqld HTTP port the
//! Hrana/JSON endpoints live on.
//!
//! This is the failure the whole-branch review reproduced: the original
//! `reqwest`-based proxy buffered both bodies and only spoke HTTP/1.1
//! outbound, so a gRPC request that succeeded against sqld directly came back
//! `400 Bad Request` when proxied. `sqld_rejects_grpc_over_http1` below pins
//! that root cause in place so it cannot silently regress, and
//! `grpc_over_h2c_survives_the_proxy` asserts the proxied call now matches the
//! direct one — including the `grpc-status` that arrives in HTTP/2 *trailers*,
//! not headers.

mod common;

use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use uuid::Uuid;
use wardn::auth::{self, AppState};
use wardn::db;

/// A gRPC method on sqld's replication service — the same service hivemind's
/// embedded-replica sync client calls. An empty length-prefixed message is
/// enough: we care about the transport, not the payload.
const GRPC_METHOD: &str = "/wal_log.ReplicationLog/Hello";
const EMPTY_GRPC_MESSAGE: [u8; 5] = [0, 0, 0, 0, 0];

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

/// Seeds a user + mapping + API key pointing at `namespace`, returning the
/// full (unhashed) key.
async fn seed_owner(pool: &sqlx::PgPool, namespace: &str) -> String {
    let owner_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_id)
        .bind(format!("grpc-{owner_id}@example.com"))
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

/// Boots the real gateway on an ephemeral port with `axum::serve`, so the
/// server side of the h2c connection is genuinely exercised (axum's
/// `oneshot` test helper never touches the wire, and so can never prove
/// anything about HTTP/2 framing). Returns its base URL.
async fn spawn_gateway(pool: sqlx::PgPool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = wardn::app(AppState::new(
        pool,
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    ));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

struct GrpcOutcome {
    status: StatusCode,
    headers: HeaderMap,
    trailers: HeaderMap,
}

impl GrpcOutcome {
    /// gRPC puts its status in trailers on a normal call, but collapses to a
    /// "trailers-only" response (status in the *headers*) on an early
    /// failure. Both must survive a proxy, so look in both.
    fn grpc_status(&self) -> Option<String> {
        let name = HeaderName::from_static("grpc-status");
        self.trailers
            .get(&name)
            .or_else(|| self.headers.get(&name))
            .map(|v| v.to_str().unwrap().to_string())
    }
}

/// Sends a gRPC request. `http2` selects prior-knowledge h2c (what a real
/// gRPC client does over cleartext) versus plain HTTP/1.1.
async fn grpc_call(base: &str, http2: bool, extra: &[(&str, &str)]) -> GrpcOutcome {
    let mut builder = Client::builder(TokioExecutor::new());
    if http2 {
        builder.http2_only(true);
    }
    let client: Client<HttpConnector, Full<Bytes>> = builder.build(HttpConnector::new());

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

    let response = client.request(request).await.expect("gRPC request failed");
    let status = response.status();
    let headers = response.headers().clone();
    let collected = response
        .into_body()
        .collect()
        .await
        .expect("reading gRPC response body failed");
    GrpcOutcome {
        status,
        headers,
        trailers: collected.trailers().cloned().unwrap_or_default(),
    }
}

fn namespace_bin(namespace: &str) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(namespace)
}

/// The root cause of the review's finding, pinned: sqld rejects gRPC framed as
/// HTTP/1.1 outright. Any proxy that downgrades the outbound leg to HTTP/1.1
/// — as the original `reqwest` implementation did — breaks sync, full stop.
#[tokio::test]
async fn sqld_rejects_grpc_over_http1() {
    let namespace = format!("h1probe-{}", Uuid::new_v4());
    create_namespace(&namespace).await;

    let outcome = grpc_call(
        &test_sqld_url(),
        false,
        &[("x-namespace-bin", &namespace_bin(&namespace))],
    )
    .await;

    assert_eq!(
        outcome.status,
        StatusCode::BAD_REQUEST,
        "expected sqld to reject HTTP/1.1-framed gRPC; the proxy must speak h2c"
    );

    delete_namespace(&namespace).await;
}

/// The fix: the same gRPC call succeeds through the gateway exactly as it does
/// straight to sqld — same HTTP status, same `grpc-status`, and the trailer
/// actually arrives (it is what carries `grpc-status` on a successful call).
#[tokio::test]
async fn grpc_over_h2c_survives_the_proxy() {
    let pool = test_pool().await;
    let namespace = format!("grpcns-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let key = seed_owner(&pool, &namespace).await;
    let gateway = spawn_gateway(pool).await;

    // Baseline: straight to sqld, selecting the namespace ourselves.
    let direct = grpc_call(
        &test_sqld_url(),
        true,
        &[("x-namespace-bin", &namespace_bin(&namespace))],
    )
    .await;
    assert_eq!(
        direct.status,
        StatusCode::OK,
        "direct gRPC call to sqld failed"
    );
    assert_eq!(
        direct.grpc_status().as_deref(),
        Some("0"),
        "direct gRPC call did not return grpc-status 0"
    );
    assert!(
        direct.trailers.contains_key("grpc-status"),
        "expected sqld to deliver grpc-status in HTTP/2 trailers on a successful call"
    );

    // Through the gateway: no namespace selector of our own, just the API
    // key. The gateway resolves the namespace and sets the selector itself.
    let proxied = grpc_call(
        &gateway,
        true,
        &[(header::AUTHORIZATION.as_str(), &format!("Bearer {key}"))],
    )
    .await;
    assert_eq!(
        proxied.status, direct.status,
        "proxied gRPC status did not match the direct call"
    );
    assert_eq!(
        proxied.grpc_status(),
        direct.grpc_status(),
        "proxied grpc-status did not match the direct call"
    );
    assert!(
        proxied.trailers.contains_key("grpc-status"),
        "the proxy dropped the HTTP/2 trailers carrying grpc-status"
    );

    delete_namespace(&namespace).await;
}

/// `x-namespace-bin` is sqld's *authoritative* namespace selector: on the
/// gRPC endpoints it is the only one that works, and on the Hrana/HTTP
/// endpoints it overrides `Host`. A client that could smuggle its own through
/// the gateway would reach any namespace it liked with its own valid key, so
/// the proxy must overwrite it rather than forward it.
///
/// The discriminator is a selector naming a namespace that does not exist: if
/// the gateway forwarded it, sqld could not resolve it and the call would
/// fail; because the gateway overwrites it with the caller's own namespace,
/// the call succeeds.
#[tokio::test]
async fn smuggled_namespace_selector_is_overwritten() {
    let pool = test_pool().await;
    let mine = format!("grpcmine-{}", Uuid::new_v4());
    let nonexistent = format!("grpcnope-{}", Uuid::new_v4());
    create_namespace(&mine).await;
    let key = seed_owner(&pool, &mine).await;
    let gateway = spawn_gateway(pool).await;

    // Baseline: that selector really is unresolvable straight to sqld, so
    // "it worked" below can only mean the gateway replaced it.
    let unresolvable = grpc_call(
        &test_sqld_url(),
        true,
        &[("x-namespace-bin", &namespace_bin(&nonexistent))],
    )
    .await;
    assert_ne!(
        unresolvable.grpc_status().as_deref(),
        Some("0"),
        "test setup is broken: sqld accepted a nonexistent namespace"
    );

    let attack = grpc_call(
        &gateway,
        true,
        &[
            (header::AUTHORIZATION.as_str(), &format!("Bearer {key}")),
            ("x-namespace-bin", &namespace_bin(&nonexistent)),
            ("host", &format!("{nonexistent}.local")),
        ],
    )
    .await;
    assert_eq!(
        attack.grpc_status().as_deref(),
        Some("0"),
        "the proxy forwarded a client-supplied namespace selector instead of \
         replacing it with the caller's own namespace"
    );

    delete_namespace(&mine).await;
}
