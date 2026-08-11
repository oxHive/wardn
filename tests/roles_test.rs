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

#[tokio::test]
async fn create_role_creates_a_role_with_its_permissions() {
    let pool = test_pool().await;
    let org_id = seed_bare_org(&pool).await;
    let role_name = format!("analyst-{}", Uuid::new_v4());

    let role_id = roles::create_role(&pool, org_id, &role_name, &[Permission::DbQuery])
        .await
        .unwrap();

    let all_roles = roles::list_roles(&pool, org_id).await.unwrap();
    let created = all_roles
        .iter()
        .find(|r| r.id == role_id)
        .expect("role should exist");
    assert_eq!(created.name, role_name);
    assert_eq!(created.permissions, vec!["db:query".to_string()]);
}

#[tokio::test]
async fn set_role_permissions_replaces_the_permission_set() {
    let pool = test_pool().await;
    let (org_id, _) = seed_member_with_role(&pool, &[Permission::DbQuery]).await;
    let role_id = roles::list_roles(&pool, org_id).await.unwrap()[0].id;

    roles::set_role_permissions(
        &pool,
        role_id,
        &[Permission::DbSync, Permission::OrgManageRoles],
    )
    .await
    .unwrap();

    let all_roles = roles::list_roles(&pool, org_id).await.unwrap();
    let updated = all_roles.iter().find(|r| r.id == role_id).unwrap();
    let mut permissions = updated.permissions.clone();
    permissions.sort();
    assert_eq!(
        permissions,
        vec!["db:sync".to_string(), "org:manage_roles".to_string()]
    );
}

#[tokio::test]
async fn delete_role_removes_an_unused_role() {
    let pool = test_pool().await;
    let org_id = seed_bare_org(&pool).await;
    let role_name = format!("temp-{}", Uuid::new_v4());
    let role_id = roles::create_role(&pool, org_id, &role_name, &[]).await.unwrap();

    roles::delete_role(&pool, org_id, role_id).await.unwrap();

    let all_roles = roles::list_roles(&pool, org_id).await.unwrap();
    assert!(!all_roles.iter().any(|r| r.id == role_id));
}

#[tokio::test]
async fn delete_role_rejects_a_role_still_held_by_a_member() {
    let pool = test_pool().await;
    let (org_id, _) = seed_member_with_role(&pool, &[Permission::DbQuery]).await;
    let role_id = roles::list_roles(&pool, org_id).await.unwrap()[0].id;

    let result = roles::delete_role(&pool, org_id, role_id).await;
    assert!(matches!(result, Err(roles::DeleteRoleError::InUse)));
}

#[tokio::test]
async fn delete_role_rejects_an_unknown_role_id() {
    let pool = test_pool().await;
    let org_id = seed_bare_org(&pool).await;

    let result = roles::delete_role(&pool, org_id, Uuid::new_v4()).await;
    assert!(matches!(result, Err(roles::DeleteRoleError::NotFound)));
}

#[tokio::test]
async fn assign_member_role_updates_an_existing_members_role() {
    let pool = test_pool().await;
    let (org_id, user_id) = seed_member_with_role(&pool, &[Permission::DbQuery]).await;
    let new_role_name = format!("sync-only-{}", Uuid::new_v4());
    let new_role_id = roles::create_role(&pool, org_id, &new_role_name, &[Permission::DbSync])
        .await
        .unwrap();

    let updated = roles::assign_member_role(&pool, org_id, user_id, new_role_id)
        .await
        .unwrap();
    assert!(updated);

    let permissions = roles::member_permissions(&pool, org_id, user_id)
        .await
        .unwrap()
        .unwrap();
    assert!(permissions.contains(&Permission::DbSync));
    assert!(!permissions.contains(&Permission::DbQuery));
}

#[tokio::test]
async fn assign_member_role_returns_false_for_a_role_from_another_org() {
    let pool = test_pool().await;
    let (org_a, user_id) = seed_member_with_role(&pool, &[Permission::DbQuery]).await;
    let org_b = seed_bare_org(&pool).await;
    let role_name = format!("other-org-role-{}", Uuid::new_v4());
    let role_in_b = roles::create_role(&pool, org_b, &role_name, &[Permission::DbSync])
        .await
        .unwrap();

    let updated = roles::assign_member_role(&pool, org_a, user_id, role_in_b)
        .await
        .unwrap();
    assert!(!updated);
}

#[tokio::test]
async fn delete_role_rejects_a_role_in_use_in_a_different_org() {
    let pool = test_pool().await;
    let (org_a, _) = seed_member_with_role(&pool, &[Permission::DbQuery]).await;
    let org_b = seed_bare_org(&pool).await;
    // Get the role_id from org_a (which is in use there)
    let role_id_in_use = roles::list_roles(&pool, org_a).await.unwrap()[0].id;

    // Try to delete it from org_b (wrong org) — should return NotFound, not InUse
    let result = roles::delete_role(&pool, org_b, role_id_in_use).await;
    assert!(matches!(result, Err(roles::DeleteRoleError::NotFound)));
}
