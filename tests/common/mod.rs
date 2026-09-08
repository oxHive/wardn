//! Shared test helpers. `mod common;` is re-included by every integration
//! test file, each compiled as its own separate crate — so a helper only
//! some of those files use looks unused from any one file's point of view.
#![allow(dead_code)]

use wardn::Db;

/// Opens a fresh libSQL database in a temp file for one test. Returning the
/// `TempDir` alongside the `Db` keeps the directory (and its file) alive for
/// as long as the caller holds onto it — dropping it deletes the file.
pub async fn temp_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("org.db");
    let db = Db::open(path.to_str().unwrap()).await.expect("open db");
    (dir, db)
}

/// A fresh, not-yet-created database path in a temp dir — for tests that
/// drive the CLI commands directly (`wardn::cli::cmd_init`, etc.), which
/// open the database themselves the same way a real `wardn` invocation
/// would.
pub fn temp_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("org.db").to_str().unwrap().to_string();
    (dir, path)
}
