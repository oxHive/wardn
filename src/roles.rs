use std::collections::HashSet;

use axum::http::StatusCode;
use sqlx::PgPool;
use uuid::Uuid;

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
/// endpoints: does this user, in this org, hold a role granting `required`?
/// Non-member and member-without-permission both return the same 403 —
/// distinguishing them would leak an org's membership to non-members.
pub async fn require_permission(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    required: Permission,
) -> Result<(), StatusCode> {
    let permissions = match member_permissions(pool, org_id, user_id).await {
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
