/// Default local storage path, per the spec: `~/.local/share/wardn/org.db`.
/// Overridable with `WARDN_DB_PATH` so multiple instances (each managing one
/// org — see Decision 2) can run side by side against different files.
pub fn default_db_path() -> String {
    match dirs::data_local_dir() {
        Some(dir) => dir
            .join("wardn")
            .join("org.db")
            .to_string_lossy()
            .into_owned(),
        None => "org.db".to_string(),
    }
}

/// Pure resolution logic behind `db_path` — takes the `WARDN_DB_PATH` value
/// (if any) as a parameter instead of reading the environment directly, so
/// it's testable without racing on process-global env state across
/// parallel tests.
pub fn resolve_db_path(override_path: Option<&str>, env_var: Option<&str>) -> String {
    match override_path {
        Some(p) => p.to_string(),
        None => match env_var {
            Some(v) => v.to_string(),
            None => default_db_path(),
        },
    }
}

pub fn db_path(override_path: Option<&str>) -> String {
    resolve_db_path(
        override_path,
        std::env::var("WARDN_DB_PATH").ok().as_deref(),
    )
}

/// Pure resolution logic behind `default_listen_addr` — see
/// `resolve_db_path` for why this takes the env var as a parameter.
pub fn resolve_listen_addr(env_var: Option<&str>) -> String {
    env_var
        .map(str::to_string)
        .unwrap_or_else(|| "127.0.0.1:7787".to_string())
}

/// Default bind address for `wardn serve` — loopback-only by default since
/// this is a single-org authorization service, not a public API.
pub fn default_listen_addr() -> String {
    resolve_listen_addr(std::env::var("WARDN_LISTEN_ADDR").ok().as_deref())
}

/// Each of these three is independently optional — unset means that one
/// piece of observability (metrics, trace export, or log shipping) stays
/// off, not that all of them do. `wardn serve` reads them once at startup;
/// no other subcommand touches them.
#[cfg(feature = "observability")]
pub mod observability {
    /// Pure resolution logic behind `metrics_token` — see
    /// `super::resolve_db_path` for why this takes the env var as a
    /// parameter rather than reading it directly.
    pub fn resolve_metrics_token(env_var: Option<&str>) -> Option<String> {
        env_var.filter(|s| !s.is_empty()).map(str::to_string)
    }

    /// Bearer token required on `GET /metrics`. Unset means metrics are
    /// toggled off entirely — no recorder is installed and the route
    /// always 404s (see `src/observability.rs`), not just "unauthenticated".
    pub fn metrics_token() -> Option<String> {
        resolve_metrics_token(std::env::var("WARDN_METRICS_TOKEN").ok().as_deref())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_db_path_prefers_explicit_override() {
        assert_eq!(
            resolve_db_path(Some("/explicit/path.db"), Some("/env/path.db")),
            "/explicit/path.db"
        );
    }

    #[test]
    fn resolve_db_path_falls_back_to_env_var() {
        assert_eq!(resolve_db_path(None, Some("/env/path.db")), "/env/path.db");
    }

    #[test]
    fn resolve_db_path_falls_back_to_default_when_neither_is_set() {
        assert_eq!(resolve_db_path(None, None), default_db_path());
    }

    #[test]
    fn default_db_path_names_wardn() {
        assert!(default_db_path().contains("wardn"));
    }

    #[test]
    fn resolve_listen_addr_uses_env_var_when_set() {
        assert_eq!(resolve_listen_addr(Some("0.0.0.0:9999")), "0.0.0.0:9999");
    }

    #[test]
    fn resolve_listen_addr_defaults_to_loopback_7787() {
        assert_eq!(resolve_listen_addr(None), "127.0.0.1:7787");
    }

    #[cfg(feature = "observability")]
    mod observability_tests {
        use super::super::observability::resolve_metrics_token;

        #[test]
        fn empty_token_is_treated_as_unset() {
            assert_eq!(resolve_metrics_token(Some("")), None);
        }

        #[test]
        fn non_empty_token_passes_through() {
            assert_eq!(
                resolve_metrics_token(Some("secret")),
                Some("secret".to_string())
            );
        }

        #[test]
        fn unset_is_none() {
            assert_eq!(resolve_metrics_token(None), None);
        }
    }
}
