use hivemind_gateway::db;
use hivemind_gateway::roles::{self, Permission};
use uuid::Uuid;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

/// Seeds an org, a role with the given permissions, a user, and an
/// org_members row assigning that role. Returns (org_id, user_id).
async fn seed_member_with_role(pool: &sqlx::PgPool, permissions: &[Permission]) -> (Uuid, Uuid) {
    let org_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let role_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
        .bind(user_id)
        .bind(format!("member-{user_id}@example.com"))
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
    (org_id, user_id)
}

async fn seed_bare_org(pool: &sqlx::PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orgs (id, name) VALUES ($1, $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

#[tokio::test]
async fn member_permissions_returns_the_roles_permissions() {
    let pool = test_pool().await;
    let (org_id, user_id) =
        seed_member_with_role(&pool, &[Permission::DbQuery, Permission::DbSync]).await;

    let permissions = roles::member_permissions(&pool, org_id, user_id)
        .await
        .unwrap()
        .expect("should be a member");
    assert!(permissions.contains(&Permission::DbQuery));
    assert!(permissions.contains(&Permission::DbSync));
    assert!(!permissions.contains(&Permission::OrgManageRoles));
}

#[tokio::test]
async fn member_permissions_returns_none_for_a_non_member() {
    let pool = test_pool().await;
    let org_id = seed_bare_org(&pool).await;

    let result = roles::member_permissions(&pool, org_id, Uuid::new_v4())
        .await
        .unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn require_permission_allows_a_member_with_the_permission() {
    let pool = test_pool().await;
    let (org_id, user_id) = seed_member_with_role(&pool, &[Permission::DbQuery]).await;

    let result = roles::require_permission(&pool, org_id, user_id, Permission::DbQuery).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn require_permission_rejects_a_member_without_the_permission() {
    let pool = test_pool().await;
    let (org_id, user_id) = seed_member_with_role(&pool, &[Permission::DbQuery]).await;

    let result = roles::require_permission(&pool, org_id, user_id, Permission::DbSync).await;
    assert_eq!(result.unwrap_err(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn require_permission_rejects_a_non_member() {
    let pool = test_pool().await;
    let org_id = seed_bare_org(&pool).await;

    let result =
        roles::require_permission(&pool, org_id, Uuid::new_v4(), Permission::DbQuery).await;
    assert_eq!(result.unwrap_err(), axum::http::StatusCode::FORBIDDEN);
}
