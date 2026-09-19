use anyhow::{Result, bail};
use libsql::{Connection, params};
use serde::Serialize;

use crate::db;

#[derive(Debug, Clone, Serialize)]
pub struct Org {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

/// Each `wardn` instance manages exactly one org (Decision 2) — there is no
/// id/lookup parameter anywhere in this module, only "the org," if one
/// exists yet.
pub async fn get(conn: &Connection) -> Result<Option<Org>> {
    let mut rows = conn
        .query("SELECT id, name, created_at FROM org LIMIT 1", ())
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(Org {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
        })),
        None => Ok(None),
    }
}

/// Creates the org. Fails if one already exists — a running `wardn`
/// instance manages exactly one org for its whole lifetime; a second org
/// belongs in a second instance against a second database file, not a
/// second row here.
pub async fn create(conn: &Connection, name: &str) -> Result<Org> {
    if get(conn).await?.is_some() {
        bail!(
            "an org already exists in this database — each wardn instance manages exactly one org"
        );
    }
    let org = Org {
        id: db::new_id(),
        name: name.to_string(),
        created_at: db::now(),
    };
    conn.execute(
        "INSERT INTO org (id, name, created_at) VALUES (?1, ?2, ?3)",
        params![org.id.clone(), org.name.clone(), org.created_at],
    )
    .await?;
    Ok(org)
}

pub async fn rename(conn: &Connection, name: &str) -> Result<Org> {
    let Some(org) = get(conn).await? else {
        bail!("no org exists yet — run `wardn init` first");
    };
    conn.execute(
        "UPDATE org SET name = ?1 WHERE id = ?2",
        params![name.to_string(), org.id.clone()],
    )
    .await?;
    Ok(Org {
        name: name.to_string(),
        ..org
    })
}

/// Deletes the org and every member/api_key row along with it — this is the
/// one place Wardn deviates from soft-delete, since deleting the org itself
/// means there is nothing left to keep a record of.
pub async fn delete(conn: &Connection) -> Result<()> {
    let Some(org) = get(conn).await? else {
        bail!("no org exists yet — run `wardn init` first");
    };
    conn.execute(
        "DELETE FROM api_keys WHERE member_id IN (SELECT id FROM members)",
        (),
    )
    .await?;
    conn.execute("DELETE FROM members", ()).await?;
    conn.execute("DELETE FROM org WHERE id = ?1", params![org.id])
        .await?;
    Ok(())
}
