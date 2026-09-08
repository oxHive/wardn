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

/// Each of these three is independently optional — unset means that one
/// piece of observability (metrics, trace export, or log shipping) stays
/// off, not that all of them do. `wardn serve` reads them once at startup;
/// no other subcommand touches them.
#[cfg(feature = "observability")]
pub mod observability {
    /// Bearer token required on `GET /metrics`. Unset means metrics are
    /// toggled off entirely — no recorder is installed and the route
    /// always 404s (see `src/observability.rs`), not just "unauthenticated".
    pub fn metrics_token() -> Option<String> {
        std::env::var("WARDN_METRICS_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// OTLP/gRPC endpoint traces are exported to (e.g.
    /// `http://otel-collector:4317`). Unset means spans are still created
    /// and logged, just never exported anywhere.
    pub fn otel_exporter_otlp_endpoint() -> Option<String> {
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()
    }

    /// Base URL of a Loki instance to push logs to. Unset means logs stay
    /// stdout-only.
    pub fn loki_url() -> Option<String> {
        std::env::var("LOKI_URL").ok()
    }
}
