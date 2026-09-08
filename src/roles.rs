use std::fmt;
use std::str::FromStr;

/// The three roles from the spec's Role Semantics section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Member,
    ReadOnly,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
            Role::ReadOnly => "read_only",
        }
    }

    /// admin — full control: invite/remove members, change roles,
    /// read + write org-layer memory, rename/delete the org.
    pub fn can_manage_members(&self) -> bool {
        matches!(self, Role::Admin)
    }

    /// admin and member can read + write org-layer memory; read_only can
    /// only read.
    pub fn can_write(&self) -> bool {
        matches!(self, Role::Admin | Role::Member)
    }

    pub fn can_read(&self) -> bool {
        // Every known role can at least read; only membership/role changes
        // are gated more tightly (see `can_manage_members`).
        matches!(self, Role::Admin | Role::Member | Role::ReadOnly)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid role {0:?} — expected one of admin, member, read_only")]
pub struct InvalidRole(pub String);

impl FromStr for Role {
    type Err = InvalidRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            "read_only" | "read-only" | "readonly" => Ok(Role::ReadOnly),
            other => Err(InvalidRole(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_as_str() {
        for role in [Role::Admin, Role::Member, Role::ReadOnly] {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), role);
        }
    }

    #[test]
    fn rejects_unknown_role() {
        assert!("owner".parse::<Role>().is_err());
    }

    #[test]
    fn only_admin_manages_members() {
        assert!(Role::Admin.can_manage_members());
        assert!(!Role::Member.can_manage_members());
        assert!(!Role::ReadOnly.can_manage_members());
    }

    #[test]
    fn read_only_cannot_write() {
        assert!(Role::Admin.can_write());
        assert!(Role::Member.can_write());
        assert!(!Role::ReadOnly.can_write());
    }
}
