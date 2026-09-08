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
