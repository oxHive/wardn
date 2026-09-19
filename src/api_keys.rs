use anyhow::{Result, bail};
use argon2::password_hash::{SaltString, rand_core::OsRng};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use libsql::{Connection, params};
use rand::Rng;
use serde::Serialize;

use crate::db;
use crate::members::{self, Member};

/// Marker every issued key starts with, e.g. `wd_a1b2c3...` (spec's
/// `key_prefix` example).
pub const KEY_MARKER: &str = "wd_";

/// Length of the random part of a full key.
const RANDOM_LEN: usize = 32;

/// How many characters of the random part are kept, alongside `KEY_MARKER`,
/// as the stored `key_prefix` — long enough to make lookup-by-prefix
/// collisions practically impossible, short enough to still read as "for
/// display" per the spec.
const PREFIX_LEN: usize = 12;

#[derive(Debug, Clone, Serialize)]
pub struct ApiKey {
    pub id: String,
    pub member_id: String,
    pub label: Option<String>,
    pub key_prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

fn row_to_key(row: &libsql::Row) -> Result<ApiKey> {
    Ok(ApiKey {
        id: row.get(0)?,
        member_id: row.get(1)?,
        label: row.get(2)?,
        key_prefix: row.get(3)?,
        created_at: row.get(4)?,
        last_used_at: row.get(5)?,
        revoked_at: row.get(6)?,
    })
}

const SELECT_COLUMNS: &str =
    "id, member_id, label, key_prefix, created_at, last_used_at, revoked_at";

/// Generates a new key. Returns `(full_key, key_prefix)` — the caller shows
/// `full_key` to the operator exactly once and stores only its argon2 hash
/// plus `key_prefix`.
fn generate_key() -> (String, String) {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let random_part: String = (0..RANDOM_LEN)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let full_key = format!("{KEY_MARKER}{random_part}");
    let key_prefix: String = format!(
        "{KEY_MARKER}{}",
        random_part.chars().take(PREFIX_LEN).collect::<String>()
    );
    (full_key, key_prefix)
}

fn hash_key(full_key: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(full_key.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing api key: {e}"))?
        .to_string();
    Ok(hash)
}

fn verify_key(full_key: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(full_key.as_bytes(), &parsed)
        .is_ok()
}

/// Mints a new key for `member_id`. Returns the created row plus the full
/// key — shown to the operator exactly once, never recoverable afterwards.
pub async fn create(
    conn: &Connection,
    member_id: &str,
    label: Option<&str>,
) -> Result<(ApiKey, String)> {
    let Some(member) = members::find_by_id(conn, member_id).await? else {
        bail!("no member with id {member_id}");
    };
    if member.removed_at.is_some() {
        bail!("member {member_id} has been removed and cannot hold API keys");
    }
    let (full_key, key_prefix) = generate_key();
    let key_hash = hash_key(&full_key)?;
    let key = ApiKey {
        id: db::new_id(),
        member_id: member_id.to_string(),
        label: label.map(str::to_string),
        key_prefix,
        created_at: db::now(),
        last_used_at: None,
        revoked_at: None,
    };
    conn.execute(
        "INSERT INTO api_keys (id, member_id, label, key_hash, key_prefix, created_at, last_used_at, revoked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)",
        params![
            key.id.clone(),
            key.member_id.clone(),
            key.label.clone(),
            key_hash,
            key.key_prefix.clone(),
            key.created_at
        ],
    )
    .await?;
    Ok((key, full_key))
}

/// Lists every API key in the org, newest first — `wardn keys list` has no
/// per-member filter in the CLI surface.
pub async fn list(conn: &Connection) -> Result<Vec<ApiKey>> {
    let mut rows = conn
        .query(
            &format!("SELECT {SELECT_COLUMNS} FROM api_keys ORDER BY created_at DESC, id"),
            (),
        )
        .await?;
    let mut keys = Vec::new();
    while let Some(row) = rows.next().await? {
        keys.push(row_to_key(&row)?);
    }
    Ok(keys)
}

pub async fn find_by_id(conn: &Connection, id: &str) -> Result<Option<ApiKey>> {
    let mut rows = conn
        .query(
            &format!("SELECT {SELECT_COLUMNS} FROM api_keys WHERE id = ?1"),
            params![id.to_string()],
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(row_to_key(&row)?)),
        None => Ok(None),
    }
}

/// Idempotent: revoking an already-revoked key is not an error.
pub async fn revoke(conn: &Connection, id: &str) -> Result<()> {
    let Some(key) = find_by_id(conn, id).await? else {
        bail!("no api key with id {id}");
    };
    if key.revoked_at.is_some() {
        return Ok(());
    }
    conn.execute(
        "UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2",
        params![db::now(), id.to_string()],
    )
    .await?;
    Ok(())
}

/// Verifies a presented full key against the database. Looks up candidate
/// rows by `key_prefix` (cheap, indexed by the constraint on the column),
/// then does a real argon2 verification against each candidate — collisions
/// on a 12-character random prefix are astronomically unlikely but this
/// stays correct even if one ever occurred. Returns the authenticated
/// member on success, updating `last_used_at`; `None` for any failure
/// (unknown prefix, hash mismatch, revoked key, or a removed member) so
/// callers can't distinguish those cases from response timing/shape alone.
pub async fn verify(conn: &Connection, full_key: &str) -> Result<Option<Member>> {
    let Some(prefix) = full_key.starts_with(KEY_MARKER).then(|| {
        full_key
            .chars()
            .take(KEY_MARKER.len() + PREFIX_LEN)
            .collect::<String>()
    }) else {
        return Ok(None);
    };

    let mut rows = conn
        .query(
            &format!(
                "SELECT {SELECT_COLUMNS}, key_hash FROM api_keys \
                 WHERE key_prefix = ?1 AND revoked_at IS NULL"
            ),
            params![prefix],
        )
        .await?;

    while let Some(row) = rows.next().await? {
        let key = row_to_key(&row)?;
        let key_hash: String = row.get(7)?;
        if verify_key(full_key, &key_hash) {
            let Some(member) = members::find_by_id(conn, &key.member_id).await? else {
                return Ok(None);
            };
            if member.removed_at.is_some() {
                return Ok(None);
            }
            conn.execute(
                "UPDATE api_keys SET last_used_at = ?1 WHERE id = ?2",
                params![db::now(), key.id.clone()],
            )
            .await?;
            return Ok(Some(member));
        }
    }
    Ok(None)
}
