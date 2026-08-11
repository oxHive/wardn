use std::collections::HashSet;

use axum::http::StatusCode;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::auth::AuthedOwner;

/// The fixed, hardcoded permission catalog — org admins compose named roles
/// from this set, but cannot invent new permission types (that needs a
/// migration). See docs/superpowers/specs/2026-08-11-org-roles-design.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    OrgManageMembers,
    OrgManageRoles,
    DbQuery,
    DbSync,
}

pub const ALL_PERMISSIONS: [Permission; 4] = [
    Permission::OrgManageMembers,
    Permission::OrgManageRoles,
    Permission::DbQuery,
    Permission::DbSync,
];

impl Permission {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Permission::OrgManageMembers => "org:manage_members",
            Permission::OrgManageRoles => "org:manage_roles",
            Permission::DbQuery => "db:query",
            Permission::DbSync => "db:sync",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        ALL_PERMISSIONS.into_iter().find(|p| p.as_db_str() == s)
    }
}

/// Looks up an org member's effective permissions via their assigned role.
/// `Ok(None)` means the user is not a member of this org at all — distinct
/// from `Ok(Some(empty set))`, which means a member holds a role with zero
/// permissions attached (allowed, just useless).
pub async fn member_permissions(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<Option<HashSet<Permission>>, sqlx::Error> {
    let role_id: Option<(Uuid,)> =
        sqlx::query_as("SELECT role_id FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    let Some((role_id,)) = role_id else {
        return Ok(None);
    };
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT permission FROM role_permissions WHERE role_id = $1")
            .bind(role_id)
            .fetch_all(pool)
            .await?;
    Ok(Some(
        rows.into_iter()
            .filter_map(|(p,)| Permission::from_db_str(&p))
            .collect(),
    ))
}

/// The shared authorization check for both the proxy path and the admin
/// endpoints: does this caller, in this org, hold a role granting `required`?
/// Non-member and member-without-permission both return the same 403 —
/// distinguishing them would leak an org's membership to non-members.
///
/// Takes the whole [`AuthedOwner`] rather than a bare `user_id` on purpose:
/// `org_members.user_id` is a *user* id, and `owner.owner_id` is only a user
/// id when `owner_type == "user"` (a workspace/org-owned key's `owner_id`
/// names a workspace/org, not a user, and could never legitimately match a
/// membership row). Doing that check here, once, rather than at each call
/// site, makes it structurally impossible for a caller to forget it — the
/// admin endpoints had already drifted from the proxy path on exactly this.
pub async fn require_permission(
    pool: &PgPool,
    org_id: Uuid,
    owner: &AuthedOwner,
    required: Permission,
) -> Result<(), StatusCode> {
    if owner.owner_type != "user" {
        return Err(StatusCode::FORBIDDEN);
    }
    let permissions = match member_permissions(pool, org_id, owner.owner_id).await {
        Ok(permissions) => permissions,
        Err(e) => {
            tracing::error!("org permission lookup failed: {e:#}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    match permissions {
        Some(permissions) if permissions.contains(&required) => Ok(()),
        _ => Err(StatusCode::FORBIDDEN),
    }
}

pub struct RoleWithPermissions {
    pub id: Uuid,
    pub name: String,
    pub permissions: Vec<String>,
}

pub async fn list_roles(pool: &PgPool, org_id: Uuid) -> Result<Vec<RoleWithPermissions>, sqlx::Error> {
    let roles: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM roles WHERE org_id = $1 ORDER BY name")
            .bind(org_id)
            .fetch_all(pool)
            .await?;
    let mut out = Vec::with_capacity(roles.len());
    for (id, name) in roles {
        let permission_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT permission FROM role_permissions WHERE role_id = $1 ORDER BY permission",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;
        out.push(RoleWithPermissions {
            id,
            name,
            permissions: permission_rows.into_iter().map(|(p,)| p).collect(),
        });
    }
    Ok(out)
}

/// Attaches `permissions` to `role_id` inside an existing transaction —
/// shared by `create_role` and `set_role_permissions`, which would otherwise
/// carry the identical loop twice.
///
/// `role_permissions` is `PRIMARY KEY (role_id, permission)`, so a repeated
/// `Permission` in `permissions` is a unique violation. Callers are expected
/// to hand over a deduplicated slice (`org_admin::parse_permissions` does);
/// this stays a plain insert rather than an `ON CONFLICT DO NOTHING` so that
/// a caller who *doesn't* dedup gets a loud error rather than a silent
/// mismatch between what it asked for and what was stored.
async fn insert_role_permissions(
    tx: &mut Transaction<'_, Postgres>,
    role_id: Uuid,
    permissions: &[Permission],
) -> Result<(), sqlx::Error> {
    for permission in permissions {
        sqlx::query("INSERT INTO role_permissions (role_id, permission) VALUES ($1, $2)")
            .bind(role_id)
            .bind(permission.as_db_str())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub async fn create_role(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    permissions: &[Permission],
) -> Result<Uuid, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let role_id = Uuid::new_v4();
    sqlx::query("INSERT INTO roles (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(role_id)
        .bind(org_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    insert_role_permissions(&mut tx, role_id, permissions).await?;
    tx.commit().await?;
    Ok(role_id)
}

#[derive(Debug)]
pub enum SetRolePermissionsError {
    NotFound,
    Db(sqlx::Error),
}

/// Replaces a role's whole permission set. Org-scoped like [`delete_role`]:
/// a `role_id` belonging to a different org is `NotFound`, not a silent
/// cross-tenant mutation. The existence check and the rewrite share one
/// transaction so a role deleted concurrently can't be resurrected with a
/// fresh permission set.
pub async fn set_role_permissions(
    pool: &PgPool,
    org_id: Uuid,
    role_id: Uuid,
    permissions: &[Permission],
) -> Result<(), SetRolePermissionsError> {
    let mut tx = pool.begin().await.map_err(SetRolePermissionsError::Db)?;
    let exists: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM roles WHERE id = $1 AND org_id = $2")
            .bind(role_id)
            .bind(org_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(SetRolePermissionsError::Db)?;
    if exists.is_none() {
        return Err(SetRolePermissionsError::NotFound);
    }
    sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(SetRolePermissionsError::Db)?;
    insert_role_permissions(&mut tx, role_id, permissions)
        .await
        .map_err(SetRolePermissionsError::Db)?;
    tx.commit().await.map_err(SetRolePermissionsError::Db)?;
    Ok(())
}

#[derive(Debug)]
pub enum DeleteRoleError {
    NotFound,
    InUse,
    Db(sqlx::Error),
}

/// Deletes a role and everything that hangs off it.
///
/// `role_permissions.role_id REFERENCES roles(id)` has **no**
/// `ON DELETE CASCADE` (see `migrations/0003_org_roles.sql`), so the children
/// must be deleted first or Postgres raises a foreign-key violation — which
/// is every role that has any permission attached, i.e. every real one. The
/// migration is checksummed by sqlx and already applied, so this is fixed
/// here in code rather than by editing `0003` in place.
///
/// Both deletes share one transaction: a half-deleted role (permissions gone,
/// row still present) would silently strip its holders' access instead of
/// failing cleanly.
pub async fn delete_role(
    pool: &PgPool,
    org_id: Uuid,
    role_id: Uuid,
) -> Result<(), DeleteRoleError> {
    // Verify the role exists *in this org* — a role id from another org is
    // NotFound, never InUse, so the outcome can't be used to probe for the
    // existence of roles in orgs the caller isn't a member of.
    let role_exists: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM roles WHERE id = $1 AND org_id = $2")
            .bind(role_id)
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(DeleteRoleError::Db)?;

    if role_exists.is_none() {
        return Err(DeleteRoleError::NotFound);
    }

    // A role still held by a member is 409, never a cascading delete that
    // would strand that member without a role (org_members.role_id is NOT
    // NULL). Scoped to this org for the same reason as the check above.
    let (in_use,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM org_members WHERE org_id = $1 AND role_id = $2")
            .bind(org_id)
            .bind(role_id)
            .fetch_one(pool)
            .await
            .map_err(DeleteRoleError::Db)?;
    if in_use > 0 {
        return Err(DeleteRoleError::InUse);
    }

    let mut tx = pool.begin().await.map_err(DeleteRoleError::Db)?;
    sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(DeleteRoleError::Db)?;
    let result = sqlx::query("DELETE FROM roles WHERE id = $1 AND org_id = $2")
        .bind(role_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .map_err(DeleteRoleError::Db)?;
    if result.rows_affected() == 0 {
        // Deleted by a concurrent caller between the check above and here.
        return Err(DeleteRoleError::NotFound);
    }
    tx.commit().await.map_err(DeleteRoleError::Db)?;
    Ok(())
}

/// Reassigns an existing member's role. Returns `Ok(false)` (not an error)
/// when there's nothing to update — either the member row doesn't exist, or
/// `role_id` doesn't belong to `org_id` (a cross-org role id is silently
/// rejected the same way a missing member is, rather than distinguished).
pub async fn assign_member_role(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    role_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE org_members SET role_id = $1
         WHERE org_id = $2 AND user_id = $3
           AND EXISTS (SELECT 1 FROM roles WHERE id = $1 AND org_id = $2)",
    )
    .bind(role_id)
    .bind(org_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_round_trips_through_its_db_string() {
        for permission in ALL_PERMISSIONS {
            let s = permission.as_db_str();
            assert_eq!(Permission::from_db_str(s), Some(permission));
        }
    }

    #[test]
    fn unknown_string_is_not_a_permission() {
        assert_eq!(Permission::from_db_str("db:delete"), None);
    }
}
