use anyhow::{Result, bail};
use libsql::{Connection, params};
use serde::Serialize;

use crate::db;
use crate::roles::Role;

#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub id: String,
    pub email: String,
    pub role: Role,
    pub invited_by: Option<String>,
    pub joined_at: i64,
    pub removed_at: Option<i64>,
}

fn row_to_member(row: &libsql::Row) -> Result<Member> {
    let role_str: String = row.get(2)?;
    Ok(Member {
        id: row.get(0)?,
        email: row.get(1)?,
        role: role_str
            .parse()
            .map_err(|e: crate::roles::InvalidRole| anyhow::anyhow!(e))?,
        invited_by: row.get(3)?,
        joined_at: row.get(4)?,
        removed_at: row.get(5)?,
    })
}

const SELECT_COLUMNS: &str = "id, email, role, invited_by, joined_at, removed_at";

/// Adds a member to the org. `invited_by` is the inviting member's id, or
/// `None` when invited directly by whoever is running the CLI (the open
/// source CLI has no separate login of its own — anyone who can run `wardn`
/// against this database is already trusted with full local access to it).
pub async fn invite(
    conn: &Connection,
    email: &str,
    role: Role,
    invited_by: Option<&str>,
) -> Result<Member> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        bail!("email must not be empty");
    }
    let existing = find_by_email(conn, &email).await?;
    if let Some(existing) = existing
        && existing.removed_at.is_none()
    {
        bail!("{email} is already a member of this org");
    }
    let member = Member {
        id: db::new_id(),
        email,
        role,
        invited_by: invited_by.map(str::to_string),
        joined_at: db::now(),
        removed_at: None,
    };
    conn.execute(
        "INSERT INTO members (id, email, role, invited_by, joined_at, removed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
        params![
            member.id.clone(),
            member.email.clone(),
            member.role.as_str(),
            member.invited_by.clone(),
            member.joined_at
        ],
    )
    .await?;
    Ok(member)
}

/// Lists active (non-removed) members, oldest first.
pub async fn list(conn: &Connection) -> Result<Vec<Member>> {
    let mut rows = conn
        .query(
            &format!(
                "SELECT {SELECT_COLUMNS} FROM members WHERE removed_at IS NULL ORDER BY joined_at, id"
            ),
            (),
        )
        .await?;
    let mut members = Vec::new();
    while let Some(row) = rows.next().await? {
        members.push(row_to_member(&row)?);
    }
    Ok(members)
}

pub async fn find_by_id(conn: &Connection, id: &str) -> Result<Option<Member>> {
    let mut rows = conn
        .query(
            &format!("SELECT {SELECT_COLUMNS} FROM members WHERE id = ?1"),
            params![id.to_string()],
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(row_to_member(&row)?)),
        None => Ok(None),
    }
}

pub async fn find_by_email(conn: &Connection, email: &str) -> Result<Option<Member>> {
    let email = email.trim().to_lowercase();
    let mut rows = conn
        .query(
            &format!("SELECT {SELECT_COLUMNS} FROM members WHERE email = ?1"),
            params![email],
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(row_to_member(&row)?)),
        None => Ok(None),
    }
}

/// Soft-deletes a member (`removed_at`, per the schema) and revokes every
/// API key they hold — a removed member's old keys must stop authorizing
/// immediately, not linger until they happen to expire or get noticed.
pub async fn remove(conn: &Connection, id: &str) -> Result<()> {
    let Some(member) = find_by_id(conn, id).await? else {
        bail!("no member with id {id}");
    };
    if member.removed_at.is_some() {
        bail!("member {id} has already been removed");
    }
    conn.execute(
        "UPDATE members SET removed_at = ?1 WHERE id = ?2",
        params![db::now(), id.to_string()],
    )
    .await?;
    conn.execute(
        "UPDATE api_keys SET revoked_at = ?1 WHERE member_id = ?2 AND revoked_at IS NULL",
        params![db::now(), id.to_string()],
    )
    .await?;
    Ok(())
}

pub async fn set_role(conn: &Connection, id: &str, role: Role) -> Result<Member> {
    let Some(member) = find_by_id(conn, id).await? else {
        bail!("no member with id {id}");
    };
    if member.removed_at.is_some() {
        bail!("member {id} has been removed and cannot be assigned a role");
    }
    conn.execute(
        "UPDATE members SET role = ?1 WHERE id = ?2",
        params![role.as_str(), id.to_string()],
    )
    .await?;
    Ok(Member { role, ..member })
}
