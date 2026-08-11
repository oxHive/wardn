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
/// Returns (org_id, full_key) — see [`seed_org_member_full`] when the test
/// also needs the member's `user_id`.
async fn seed_org_member(
    pool: &sqlx::PgPool,
    namespace: &str,
    permissions: &[Permission],
) -> (Uuid, String) {
    let (org_id, _, full_key) = seed_org_member_full(pool, namespace, permissions).await;
    (org_id, full_key)
}

/// As [`seed_org_member`], but also returns the seeded member's `user_id` —
/// needed by tests that drive `PUT /orgs/:org_id/members/:user_id/role`.
/// Returns (org_id, user_id, full_key).
async fn seed_org_member_full(
    pool: &sqlx::PgPool,
    namespace: &str,
    permissions: &[Permission],
) -> (Uuid, Uuid, String) {
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
    (org_id, user_id, full_key)
}

/// Sends a Hrana query through the real router with `X-Org-Id` set, returning
/// the status *and* the response body — the body is what distinguishes
/// "reached the org's actual namespace" from "reached some other namespace
/// that also returned 200".
async fn query_org_with_body(
    app: axum::Router,
    full_key: &str,
    org_id: Uuid,
    statements_json: &str,
) -> (StatusCode, String) {
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
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn query_org_via_gateway(
    app: axum::Router,
    full_key: &str,
    org_id: Uuid,
    statements_json: &str,
) -> StatusCode {
    query_org_with_body(app, full_key, org_id, statements_json)
        .await
        .0
}

/// Proves the `X-Org-Id` path reaches the org's *actual* namespace, not
/// merely that something returned 200 — same style as the walking skeleton's
/// `namespace_isolation_through_full_router` (`tests/proxy_test.rs`). A
/// status-only assertion cannot tell correct namespace routing from broken
/// namespace routing, and this project's prior Critical vulnerability was
/// exactly a wrong-namespace bug that still returned 200.
///
/// Writes a per-run-unique secret through the org path, then reads it back
/// through the same org path, and separately confirms the org's namespace is
/// *not* the member's own personal one (they have no personal mapping at
/// all, so a fallback to it would 404 rather than silently succeed).
#[tokio::test]
async fn member_with_db_query_reaches_the_orgs_namespace() {
    let pool = test_pool().await;
    let namespace = format!("orgqns-{}", Uuid::new_v4());
    create_namespace(&namespace).await;
    let (org_id, key) = seed_org_member(&pool, &namespace, &[Permission::DbQuery]).await;
    let secret = format!("ORG-SECRET-{}", Uuid::new_v4());

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

    let (status, body) = query_org_with_body(
        app.clone(),
        &key,
        org_id,
        &format!(
            r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "org write failed: {body}");

    let (status, body) =
        query_org_with_body(app, &key, org_id, r#"{"statements":["SELECT v FROM kv"]}"#).await;
    assert_eq!(status, StatusCode::OK, "org read failed: {body}");
    assert!(
        body.contains(&secret),
        "read through X-Org-Id did not return the value written through it — \
         the request reached the wrong namespace: {body}"
    );

    delete_namespace(&namespace).await;
}

/// The spec's cross-org isolation requirement: a member of org A holding a
/// perfectly valid role and a technically-valid personal key cannot reach org
/// B's namespace by pointing `X-Org-Id` at it.
///
/// Distinct from `non_member_is_forbidden`, whose user has no membership
/// anywhere — that only exercises the `user_id` half of the membership
/// lookup's `WHERE`. Here the user *is* a member, just of the wrong org, so
/// the `org_id` half is what has to do the work.
#[tokio::test]
async fn member_of_one_org_cannot_use_x_org_id_for_a_different_org() {
    let pool = test_pool().await;

    let namespace_a = format!("orgxa-{}", Uuid::new_v4());
    let namespace_b = format!("orgxb-{}", Uuid::new_v4());
    create_namespace(&namespace_a).await;
    create_namespace(&namespace_b).await;

    // Org A's member: a real role granting db:query, in org A.
    let (_org_a, key_a) = seed_org_member(&pool, &namespace_a, &[Permission::DbQuery]).await;
    // Org B: a separate org with its own real namespace and mapping, whose
    // membership org A's user has nothing to do with.
    let (org_b, _key_b) = seed_org_member(&pool, &namespace_b, &[Permission::DbQuery]).await;

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
    let (status, body) =
        query_org_with_body(app, &key_a, org_b, r#"{"statements":["SELECT 1"]}"#).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "org A's member reached org B via X-Org-Id: {body}"
    );

    delete_namespace(&namespace_a).await;
    delete_namespace(&namespace_b).await;
}

/// Spans the admin API (Task 4) and the proxy (Task 3) in one test: a role
/// created *over HTTP* and assigned *over HTTP* must actually authorize a
/// proxied request. Every other org-proxy test seeds its role with raw SQL,
/// so nothing else covers this seam.
#[tokio::test]
async fn a_role_created_and_assigned_over_http_authorizes_the_proxy() {
    let pool = test_pool().await;
    let namespace = format!("orge2e-{}", Uuid::new_v4());
    create_namespace(&namespace).await;

    // Bootstrap: the seeded role can manage roles and members, but grants no
    // db access at all — so the proxied request at the end can only succeed
    // via the role created and assigned below.
    let (org_id, user_id, key) = seed_org_member_full(
        &pool,
        &namespace,
        &[Permission::OrgManageRoles, Permission::OrgManageMembers],
    )
    .await;

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));

    // Before: the bootstrap role has no db:query, so the proxy path is shut.
    let status =
        query_org_via_gateway(app.clone(), &key, org_id, r#"{"statements":["SELECT 1"]}"#).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bootstrap role should not grant db:query"
    );

    let role_name = format!("querier-{}", Uuid::new_v4());
    let create_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/roles"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"{role_name}","permissions":["db:query"]}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(create_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let new_role_id = created["id"].as_str().unwrap().to_string();

    let assign_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/orgs/{org_id}/members/{user_id}/role"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"role_id":"{new_role_id}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(assign_resp.status(), StatusCode::NO_CONTENT);

    // After: the same key, same header, now authorized by the HTTP-created
    // role — and it reaches the org's real namespace, not just any 200.
    let secret = format!("E2E-SECRET-{}", Uuid::new_v4());
    let (status, body) = query_org_with_body(
        app.clone(),
        &key,
        org_id,
        &format!(
            r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "write after reassignment failed: {body}"
    );

    let (status, body) =
        query_org_with_body(app, &key, org_id, r#"{"statements":["SELECT v FROM kv"]}"#).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "read after reassignment failed: {body}"
    );
    assert!(
        body.contains(&secret),
        "the HTTP-assigned role reached the wrong namespace: {body}"
    );

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

/// A workspace-owned key's `owner_id` names a workspace, not a user — it can
/// never legitimately match an `org_members.user_id` row. This must be
/// rejected explicitly in `proxy_handler`, not merely as a side effect of the
/// `org_members.user_id` foreign key elsewhere: the key itself is otherwise
/// perfectly valid, and `X-Org-Id` access is only meaningful for a personal
/// (`owner_type = 'user'`) key.
#[tokio::test]
async fn workspace_owned_key_is_forbidden_from_org_namespace_access() {
    let pool = test_pool().await;
    let owner_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(owner_user_id)
        .bind(format!("wsowner-{owner_user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    let workspace_id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces (id, owner_user_id, name) VALUES ($1, $2, $3)")
        .bind(workspace_id)
        .bind(owner_user_id)
        .bind(format!("workspace-{workspace_id}"))
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) = auth::generate_api_key();
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'workspace', $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(owner_user_id)
    .bind(workspace_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let app = hivemind_gateway::app(AppState::new(pool, test_sqld_url()));
    let status = query_org_via_gateway(
        app,
        &full_key,
        Uuid::new_v4(),
        r#"{"statements":["SELECT 1"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
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
