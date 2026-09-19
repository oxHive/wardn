use anyhow::{Context, Result};
use libsql::Connection;

/// Applied immediately after opening every connection — same convention
/// Mynd uses for its own libSQL databases (see `MYND_SPEC_ADDENDUM.md`).
/// `foreign_keys=ON` isn't durable per-database in SQLite/libSQL; it's a
/// per-connection setting, so this must run every time a connection opens,
/// not just once at db creation.
const PRAGMAS: &str = "PRAGMA journal_mode=WAL; \
     PRAGMA synchronous=NORMAL; \
     PRAGMA foreign_keys=ON; \
     PRAGMA busy_timeout=5000;";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS org (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS members (
    id          TEXT PRIMARY KEY,
    email       TEXT UNIQUE NOT NULL,
    role        TEXT NOT NULL,
    invited_by  TEXT REFERENCES members(id),
    joined_at   INTEGER NOT NULL,
    removed_at  INTEGER
);

CREATE TABLE IF NOT EXISTS api_keys (
    id            TEXT PRIMARY KEY,
    member_id     TEXT NOT NULL REFERENCES members(id),
    label         TEXT,
    key_hash      TEXT UNIQUE NOT NULL,
    key_prefix    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    last_used_at  INTEGER,
    revoked_at    INTEGER
);
";

/// Holds the `libsql::Database` alive for as long as the `Connection`
/// borrowed from it is in use. `libsql::Connection` does not keep its
/// parent `Database` alive on its own, so the two must travel together for
/// the lifetime of the process (CLI command or `wardn serve`).
pub struct Db {
    #[allow(dead_code)]
    database: libsql::Database,
    pub conn: Connection,
}

impl Db {
    /// Opens (creating if necessary) the local libSQL database at `path`,
    /// applies Wardn's standard PRAGMAs, and ensures the org/members/api_keys
    /// tables exist.
    pub async fn open(path: &str) -> Result<Db> {
        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let database = libsql::Builder::new_local(path)
            .build()
            .await
            .with_context(|| format!("opening libSQL database at {path}"))?;
        let conn = database.connect().context("opening libSQL connection")?;
        conn.execute_batch(PRAGMAS)
            .await
            .context("applying PRAGMAs")?;
        conn.execute_batch(SCHEMA)
            .await
            .context("creating schema")?;
        Ok(Db { database, conn })
    }
}

/// Seconds since the Unix epoch, as stored in every `*_at` column.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_secs() as i64
}

/// A fresh random id for a new row (org/member/api_key primary key).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
