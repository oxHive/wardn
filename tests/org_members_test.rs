mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hivewarden::auth::AppState;
use hivewarden::db;
use tower::ServiceExt;

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

async fn delete_namespace(name: &str) {
    let _ = reqwest::Client::new()
        .delete(format!("{}/v1/namespaces/{name}", admin_url()))
        .send()
        .await;
}

async fn register(app: axum::Router, email: &str) -> (uuid::Uuid, String) {
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

/// Fetches the org's bootstrap `owner` role id directly from Postgres — the
/// only role that exists right after `POST /orgs`, used as the role to
/// invite a second member with in tests that don't care which specific
/// permissions the invited member ends up with.
async fn bootstrap_role_id(pool: &sqlx::PgPool, org_id: uuid::Uuid) -> uuid::Uuid {
    let (role_id,): (uuid::Uuid,) = sqlx::query_as("SELECT id FROM roles WHERE org_id = $1 LIMIT 1")
        .bind(org_id)
        .fetch_one(pool)
        .await
        .unwrap();
    role_id
}

#[tokio::test]
async fn add_member_lets_the_invited_user_reach_the_orgs_namespace() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (_invitee_id, invitee_key) = register(app.clone(), &invitee_email).await;
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{invitee_email}","role_id":"{role_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The invitee's OWN personal key + X-Org-Id must now reach the org's
    // namespace — write-then-read-back, not just a status check.
    let secret = format!("MEMBER-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('x', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "invited member's write into the org's namespace failed"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
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

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn list_members_shows_the_owner_and_the_invited_member() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let owner_email = format!("owner-{}@example.com", uuid::Uuid::new_v4());
    let (owner_id, owner_key) = register(app.clone(), &owner_email).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (invitee_id, _invitee_key) = register(app.clone(), &invitee_email).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{invitee_email}","role_id":"{role_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let members: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let members = members.as_array().unwrap();
    assert_eq!(members.len(), 2, "expected the owner plus the invitee: {members:?}");

    let find = |id: uuid::Uuid| {
        members
            .iter()
            .find(|m| m["user_id"].as_str().unwrap() == id.to_string())
            .unwrap_or_else(|| panic!("member {id} missing from listing: {members:?}"))
    };
    let owner_row = find(owner_id);
    assert_eq!(owner_row["email"].as_str().unwrap(), owner_email);
    // The org creator holds the bootstrap `owner` role.
    assert_eq!(owner_row["role_id"].as_str().unwrap(), role_id.to_string());
    let invitee_row = find(invitee_id);
    assert_eq!(invitee_row["email"].as_str().unwrap(), invitee_email);
    assert_eq!(invitee_row["role_id"].as_str().unwrap(), role_id.to_string());

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn list_members_rejects_a_caller_without_permission() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();

    // A registered user who was never invited to this org at all.
    let (_outsider_id, outsider_key) =
        register(app.clone(), &format!("outsider-{}@example.com", uuid::Uuid::new_v4())).await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {outsider_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn add_member_accepts_a_differently_cased_email() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (invitee_id, _invitee_key) = register(app.clone(), &invitee_email).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{}","role_id":"{role_id}"}}"#,
                    invitee_email.to_uppercase()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "an uppercased form of a registered address should not 404"
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(created["user_id"].as_str().unwrap(), invitee_id.to_string());

    delete_namespace(&org_id_str).await;
}

/// `users.email`'s UNIQUE constraint is case-*sensitive* and registration
/// doesn't normalize, so two accounts differing only in case are a real,
/// reachable state — both `POST /users` calls below succeed. The
/// case-insensitive invite lookup therefore matches two rows, and the only
/// safe answer is to refuse: nothing in the request says which of the two
/// accounts was meant, and guessing would attach org membership to an
/// account the admin never intended to invite.
#[tokio::test]
async fn add_member_refuses_an_email_matching_two_case_variant_accounts() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    // Same address, two casings, two separate accounts.
    let lower_email = format!("dupe-{}@example.com", uuid::Uuid::new_v4());
    let upper_email = lower_email.to_uppercase();
    let (lower_id, _lower_key) = register(app.clone(), &lower_email).await;
    let (upper_id, _upper_key) = register(app.clone(), &upper_email).await;
    assert_ne!(
        lower_id, upper_id,
        "case-variant registrations should be two distinct accounts"
    );

    // Either casing is now ambiguous — neither may resolve to a guess.
    for email in [&lower_email, &upper_email] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/orgs/{org_id}/members"))
                    .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(
                        r#"{{"email":"{email}","role_id":"{role_id}"}}"#
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "an ambiguous case-insensitive email match must not silently pick an account ({email})"
        );
    }

    // And nothing was written: neither account became a member.
    let (member_count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM org_members WHERE org_id = $1 AND user_id = ANY($2)")
            .bind(org_id)
            .bind(vec![lower_id, upper_id])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(member_count, 0, "the refused invite must not have added anyone");

    delete_namespace(&org_id_str).await;
}

/// The Task 2 (org membership) × Task 3 (self-service keys) seam, which
/// neither feature's own tests cover: a member minted key — not the one from
/// registration — must reach the org's namespace, and revoking it must kill
/// that key alone without disturbing the member's org access via their other
/// key.
#[tokio::test]
async fn a_members_self_minted_key_reaches_the_org_until_it_is_revoked() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let member_email = format!("member-{}@example.com", uuid::Uuid::new_v4());
    let (member_id, registration_key) = register(app.clone(), &member_email).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{member_email}","role_id":"{role_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The member mints themselves a *second* personal key.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api-keys")
                .header(header::AUTHORIZATION, format!("Bearer {registration_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let second_key = created["api_key"].as_str().unwrap().to_string();
    let second_key_id = created["id"].as_str().unwrap().to_string();
    assert_ne!(second_key, registration_key);

    // That second key + X-Org-Id reaches the org's namespace: write, read back.
    let secret = format!("SEAM-SECRET-{}", uuid::Uuid::new_v4());
    let create_and_insert = format!(
        r#"{{"statements":["CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)","INSERT INTO kv (k, v) VALUES ('seam', '{secret}')"]}}"#
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {second_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(create_and_insert))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the member's self-minted key should reach the org's namespace"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {second_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(r#"{"statements":["SELECT v FROM kv WHERE k = 'seam'"]}"#))
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
        "read-back through the self-minted key did not contain the written secret: {text}"
    );

    // The member revokes their second key.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api-keys/{second_key_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {registration_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // It can no longer authenticate at all — 401 before any org check runs.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {second_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "the revoked key should not authenticate anywhere"
    );

    // The org access story is unaffected for the member's other, live key —
    // and it still reads back the row the revoked key wrote.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {registration_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(r#"{"statements":["SELECT v FROM kv WHERE k = 'seam'"]}"#))
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
        "the member's non-revoked key lost org access after the other key was revoked: {text}"
    );

    delete_namespace(&org_id_str).await;
    delete_namespace(&member_id.to_string()).await;
}

#[tokio::test]
async fn add_member_rejects_an_unregistered_email() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"nobody-{}@example.com","role_id":"{role_id}"}}"#,
                    uuid::Uuid::new_v4()
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn add_member_rejects_a_duplicate_invite() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (_invitee_id, _invitee_key) = register(app.clone(), &invitee_email).await;

    let body = format!(r#"{{"email":"{invitee_email}","role_id":"{role_id}"}}"#);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn remove_member_revokes_org_access_but_not_the_personal_key() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();
    let role_id = bootstrap_role_id(&pool, org_id).await;

    let invitee_email = format!("invitee-{}@example.com", uuid::Uuid::new_v4());
    let (invitee_id, invitee_key) = register(app.clone(), &invitee_email).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/orgs/{org_id}/members"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{invitee_email}","role_id":"{role_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/orgs/{org_id}/members/{invitee_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Org access via X-Org-Id is gone.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-org-id", &org_id_str)
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "removed member should no longer reach the org's namespace"
    );

    // The invitee's own personal namespace still works — removal didn't
    // touch their account.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::AUTHORIZATION, format!("Bearer {invitee_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"statements":["SELECT 1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "removed member's own personal namespace should still work"
    );

    delete_namespace(&org_id_str).await;
}

#[tokio::test]
async fn remove_member_returns_404_for_a_non_member() {
    let pool = test_pool().await;
    let state = AppState::new(pool.clone(), test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string()).with_sqld_admin_url(admin_url());
    let app = hivewarden::app(state);

    let (_owner_id, owner_key) =
        register(app.clone(), &format!("owner-{}@example.com", uuid::Uuid::new_v4())).await;
    let org_id_str = create_org(app.clone(), &owner_key, &format!("Org-{}", uuid::Uuid::new_v4())).await;
    let org_id: uuid::Uuid = org_id_str.parse().unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/orgs/{org_id}/members/{}", uuid::Uuid::new_v4()))
                .header(header::AUTHORIZATION, format!("Bearer {owner_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    delete_namespace(&org_id_str).await;
}
