/// Default local storage path, per the spec: `~/.local/share/wardn/org.db`.
/// Overridable with `WARDN_DB_PATH` so multiple instances (each managing one
/// org — see Decision 2) can run side by side against different files.
pub fn default_db_path() -> String {
    if let Some(dir) = dirs::data_local_dir() {
        dir.join("wardn")
            .join("org.db")
            .to_string_lossy()
            .into_owned()
    } else {
        "org.db".to_string()
    }
}

pub fn db_path(override_path: Option<&str>) -> String {
    if let Some(p) = override_path {
        return p.to_string();
    }
    std::env::var("WARDN_DB_PATH").unwrap_or_else(|_| default_db_path())
}

/// Default bind address for `wardn serve` — loopback-only by default since
/// this is a single-org authorization service, not a public API.
pub fn default_listen_addr() -> String {
    std::env::var("WARDN_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:7787".to_string())
}
