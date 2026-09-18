use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uuid::Uuid;
use wardn::auth::AppState;
use wardn::db;

mod common;

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

fn test_state(pool: sqlx::PgPool) -> AppState {
    AppState::new(
        pool,
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
}

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

/// Registers a new user and returns `(user_id, api_key)` — same pattern as
/// the `register` helper in `org_members_test.rs`/`api_keys_test.rs`.
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

#[tokio::test]
async fn create_user_returns_a_working_key_and_provisions_a_namespace() {
    let pool = test_pool().await;
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let email = format!("register-{}@example.com", uuid::Uuid::new_v4());
    let resp = app
        .clone()
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
    let user_id = created["user_id"].as_str().unwrap().to_string();
    let api_key = created["api_key"].as_str().unwrap();
    assert!(api_key.starts_with(wardn::auth::KEY_MARKER));

    // Prove the namespace was actually provisioned: write through it, read
    // it back, via the real router, exactly the way a real client would.
    let secret = format!("REG-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "write through the new namespace failed"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT v FROM kv"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains(&secret),
        "read-back did not contain the written secret: {text}"
    );

    delete_namespace(&user_id).await;
}

#[tokio::test]
async fn create_user_still_succeeds_when_inline_provisioning_fails() {
    let pool = test_pool().await;
    // This is the one registration test that leaves a row `pending` and then
    // asserts on it, so it contends with `provisioning_test`'s worker test —
    // which runs in a *different* binary, hence a Postgres advisory lock
    // rather than a process-local `Mutex`. See `tests/common/mod.rs`.
    let _lock = common::lock_outbox(&pool).await;
    // Unreachable admin URL — the inline attempt inside create_user must
    // fail without failing the request itself.
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url("http://127.0.0.1:1".to_string());
    let app = wardn::app(state);

    let email = format!("register-fail-{}@example.com", uuid::Uuid::new_v4());
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
    let user_id: uuid::Uuid = created["user_id"].as_str().unwrap().parse().unwrap();

    let (outbox_id, status, attempts): (uuid::Uuid, String, i32) = sqlx::query_as(
        "SELECT id, status, attempts FROM namespace_provisioning_outbox
         WHERE owner_type = 'user' AND owner_id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(attempts, 1);

    // Deliberately not left behind: a surviving `pending` row is exactly what
    // a *later* run's worker test would sweep up and provision for real.
    common::delete_outbox_row(&pool, outbox_id).await;
}

#[tokio::test]
async fn create_user_with_an_already_registered_email_returns_409() {
    let pool = test_pool().await;
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let email = format!("dupe-{}@example.com", uuid::Uuid::new_v4());
    let register = || {
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                .unwrap(),
        )
    };

    let first = register().await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user_id = created["user_id"].as_str().unwrap().to_string();

    // `users.email` is UNIQUE: the second attempt is a client-correctable
    // conflict, not a gateway fault, and must not look like a 500.
    let second = register().await.unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(body.to_vec()).unwrap(),
        "email already registered"
    );

    delete_namespace(&user_id).await;
}

async fn seed_registered_user(app: axum::Router) -> (uuid::Uuid, String) {
    let email = format!("orgcreator-{}@example.com", uuid::Uuid::new_v4());
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
    let user_id: uuid::Uuid = created["user_id"].as_str().unwrap().parse().unwrap();
    let api_key = created["api_key"].as_str().unwrap().to_string();
    (user_id, api_key)
}

#[tokio::test]
async fn create_org_provisions_a_namespace_and_makes_the_creator_its_owner() {
    let pool = test_pool().await;
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    let (_user_id, api_key) = seed_registered_user(app.clone()).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"Acme-{}"}}"#,
                    uuid::Uuid::new_v4()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let org_id = created["org_id"].as_str().unwrap().to_string();

    // The creator's own key, plus X-Org-Id, must already reach the new
    // org's namespace — proving the bootstrap role + membership landed.
    let secret = format!("ORG-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "write into the new org's namespace failed"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id)
                .body(Body::from(r#"{"statements":["SELECT v FROM kv"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains(&secret),
        "read-back did not contain the written secret: {text}"
    );

    delete_namespace(&org_id).await;
}

#[tokio::test]
async fn create_user_rejects_invalid_email() {
    let pool = test_pool().await;
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    );
    let app = wardn::app(state);

    for bad_email in [
        "",
        "not-an-email",
        "@example.com",
        "foo@",
        "foo@@example.com",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/users")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"email":"{bad_email}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expected {bad_email:?} to be rejected"
        );
    }
}

#[tokio::test]
async fn create_user_normalizes_email_case_and_rejects_case_variant_duplicates() {
    let pool = test_pool().await;
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    );
    let app = wardn::app(state);
    let email = format!("MixedCase-{}@Example.com", uuid::Uuid::new_v4());

    let resp = app
        .clone()
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

    // Same address, different case — must be treated as the same account.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/users")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{}"}}"#,
                    email.to_lowercase()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn create_org_is_forbidden_for_a_non_user_owned_key() {
    let pool = test_pool().await;
    let state = AppState::new(
        pool.clone(),
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    )
    .with_sqld_admin_url(admin_url());
    let app = wardn::app(state);

    // A workspace-owned key: real row, valid hash, but owner_type != "user".
    let workspace_id = uuid::Uuid::new_v4();
    let user_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("wsowner-{user_id}@example.com"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces (id, owner_user_id, name) VALUES ($1, $2, 'ws')")
        .bind(workspace_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (full_key, prefix, hash) =
        wardn::auth::generate_api_key(common::TEST_API_KEY_PEPPER.as_bytes());
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, owner_type, owner_id, prefix, key_hash)
         VALUES ($1, $2, 'workspace', $3, $4, $5)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(user_id)
    .bind(workspace_id)
    .bind(&prefix)
    .bind(&hash)
    .execute(&pool)
    .await
    .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {full_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"Nope-{}"}}"#,
                    uuid::Uuid::new_v4()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Looks up a user's pending outbox row, if any, and deletes it —
/// `test_state`'s `AppState` has no `sqld_admin_url`, so inline provisioning
/// never succeeds and every user/org created against it leaves a `pending`
/// row behind. Callers must hold `common::lock_outbox` for the duration of
/// any test that uses this, per this file's convention (see
/// `create_user_still_succeeds_when_inline_provisioning_fails` above).
async fn delete_pending_outbox_row(pool: &sqlx::PgPool, owner_type: &str, owner_id: Uuid) {
    let row: Result<(Uuid,), sqlx::Error> = sqlx::query_as(
        "SELECT id FROM namespace_provisioning_outbox WHERE owner_type = $1 AND owner_id = $2",
    )
    .bind(owner_type)
    .bind(owner_id)
    .fetch_one(pool)
    .await;
    if let Ok((outbox_id,)) = row {
        common::delete_outbox_row(pool, outbox_id).await;
    }
}

/// Real server, real TCP client — `tower_governor`'s IP extraction needs a
/// genuine `ConnectInfo`, which `oneshot` never provides (see
/// `tests/org_proxy_test.rs`'s `spawn_gateway` for the same pattern used for
/// h2c tests). Sends more requests than the configured limit from one
/// address and asserts the first 5 (the configured burst) succeed and the
/// next 2 are rejected — not just that the *last* one is, which wouldn't
/// distinguish real per-IP burst-then-limit behavior from a degenerate
/// "everything is 429" or "nothing is ever limited" implementation.
#[tokio::test]
async fn post_users_rate_limits_by_ip() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = wardn::app(test_state(pool.clone()))
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let mut statuses = Vec::new();
    let mut created_user_ids = Vec::new();
    // One more than the configured burst — see src/lib.rs's governor config
    // for the exact number this must exceed.
    for _ in 0..7 {
        let resp = client
            .post(format!("http://{addr}/users"))
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"email":"ratelimit-{}@example.com"}}"#,
                Uuid::new_v4()
            ))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        if status == 201 {
            let body = resp.bytes().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let user_id: Uuid = body["user_id"].as_str().unwrap().parse().unwrap();
            created_user_ids.push(user_id);
        }
        statuses.push(status);
    }

    assert_eq!(
        statuses[..5],
        [201, 201, 201, 201, 201],
        "expected the burst of 5 to succeed: {statuses:?}"
    );
    assert_eq!(
        statuses[5..],
        [429, 429],
        "expected the requests past the burst to be rejected: {statuses:?}"
    );

    for user_id in created_user_ids {
        delete_pending_outbox_row(&pool, "user", user_id).await;
        delete_namespace(&user_id.to_string()).await;
    }
}

#[tokio::test]
async fn create_org_is_capped_per_user() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let app = wardn::app(test_state(pool.clone()));
    let (user_id, api_key) = register(
        app.clone(),
        &format!("orgquota-{}@example.com", Uuid::new_v4()),
    )
    .await;

    let mut org_ids = Vec::new();
    // The cap is 10 (see src/registration.rs's ORG_QUOTA_PER_USER) — create
    // exactly that many, all of which must succeed, then confirm the next
    // one is rejected.
    for i in 0..10 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/orgs")
                    .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"name":"org-{i}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "org {i} should have succeeded"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let org_id: Uuid = created["org_id"].as_str().unwrap().parse().unwrap();
        org_ids.push(org_id);
    }

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/orgs")
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"one-too-many"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    delete_pending_outbox_row(&pool, "user", user_id).await;
    delete_namespace(&user_id.to_string()).await;
    for org_id in org_ids {
        delete_pending_outbox_row(&pool, "org", org_id).await;
        delete_namespace(&org_id.to_string()).await;
    }
}

/// Regression test for the org-creation quota (`ORG_QUOTA_PER_USER`,
/// `src/registration.rs`): the quota must count orgs the caller actually
/// *created* (`orgs.created_by`), not orgs where they merely hold a role
/// happening to be named `owner`. Role names are assignable by any org
/// `org:manage_members` holder via `add_member`/`assign_member_role` — if
/// the quota were still keyed on role name, an attacker could create 10
/// orgs of their own (within their own quota) and add a victim to each as
/// an `owner`-named role, permanently exhausting the victim's quota using
/// nothing but the victim's email address, without the victim ever
/// creating anything.
#[tokio::test]
async fn org_membership_via_an_owner_named_role_does_not_count_against_the_quota() {
    let pool = test_pool().await;
    let _lock = common::lock_outbox(&pool).await;
    let app = wardn::app(test_state(pool.clone()));

    // The "victim": a plain registered user who never creates anything.
    let (victim_id, victim_key) = register(
        app.clone(),
        &format!("victim-{}@example.com", Uuid::new_v4()),
    )
    .await;

    // Seed an org the victim did not create, and drop them into it under a
    // role literally named `owner` — the same shape `add_member` +
    // `assign_member_role` would produce, done directly via SQL here the
    // way `tests/org_proxy_test.rs`'s `seed_org_member` seeds roles.
    let attacker_org_id = Uuid::new_v4();
    let role_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orgs (id, name, created_by) VALUES ($1, $2, NULL)")
        .bind(attacker_org_id)
        .bind(format!("attacker-org-{attacker_org_id}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO roles (id, org_id, name) VALUES ($1, $2, 'owner')")
        .bind(role_id)
        .bind(attacker_org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO org_members (org_id, user_id, role_id) VALUES ($1, $2, $3)")
        .bind(attacker_org_id)
        .bind(victim_id)
        .bind(role_id)
        .execute(&pool)
        .await
        .unwrap();

    // The victim can still create their own full quota of 10 orgs — being
    // an `owner`-role member of an org they didn't create must not count.
    let mut org_ids = Vec::new();
    for i in 0..10 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/orgs")
                    .header(header::AUTHORIZATION, format!("Bearer {victim_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"name":"victim-org-{i}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::CREATED,
            "org {i} should have succeeded — victim's own quota is unaffected by attacker-assigned membership"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let org_id: Uuid = created["org_id"].as_str().unwrap().parse().unwrap();
        org_ids.push(org_id);
    }

    delete_pending_outbox_row(&pool, "user", victim_id).await;
    delete_namespace(&victim_id.to_string()).await;
    for org_id in org_ids {
        delete_pending_outbox_row(&pool, "org", org_id).await;
        delete_namespace(&org_id.to_string()).await;
    }
}
