use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub sqld_url: String,
    pub sqld_admin_url: String,
    pub listen_addr: String,
    pub metrics_token: String,
    pub api_key_pepper: String,
    /// OTLP/gRPC endpoint traces are exported to (e.g.
    /// `http://otel-collector:4317`). Optional: `None` means tracing spans
    /// are created and logged normally but never exported anywhere —
    /// `cargo test`/a bare `cargo run` must not require a collector.
    pub otel_exporter_otlp_endpoint: Option<String>,
    /// Base URL of a Loki instance to push logs to (e.g.
    /// `http://loki:3100`). Optional: `None` means logs stay stdout-only,
    /// exactly like today.
    pub loki_url: Option<String>,
    /// Origins allowed to call this API cross-origin — see `parse_console_origins`
    /// below and `AppState::cors_origins` (`src/auth.rs`).
    pub console_origins: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let metrics_token = std::env::var("METRICS_TOKEN").context("METRICS_TOKEN must be set")?;
        if metrics_token.is_empty() {
            anyhow::bail!(
                "METRICS_TOKEN must not be empty — an empty value fails closed and permanently \
                 401s GET /metrics"
            );
        }
        let api_key_pepper =
            std::env::var("API_KEY_PEPPER").context("API_KEY_PEPPER must be set")?;
        if api_key_pepper.len() < 32 {
            anyhow::bail!(
                "API_KEY_PEPPER must be at least 32 bytes — it's the server-side secret \
                 folded into every API key's hash, and a short value defeats the point of a \
                 pepper"
            );
        }
        Ok(Config {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            sqld_url: std::env::var("SQLD_URL").context("SQLD_URL must be set")?,
            sqld_admin_url: std::env::var("SQLD_ADMIN_URL")
                .context("SQLD_ADMIN_URL must be set")?,
            listen_addr: std::env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8787".to_string()),
            metrics_token,
            api_key_pepper,
            otel_exporter_otlp_endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
            loki_url: std::env::var("LOKI_URL").ok(),
            console_origins: parse_console_origins(
                std::env::var("CONSOLE_ORIGINS").ok().as_deref(),
            ),
        })
    }
}

/// Parses `CONSOLE_ORIGINS` (comma-separated, whitespace around each entry
/// trimmed, empty entries dropped) into the list `AppState::cors_origins`
/// wants. Pulled out as its own function — separate from `Config::from_env`
/// — so it's testable without needing every other required env var set.
/// `None` (the var is unset) and `Some("")` both fall back to the local
/// `console` dev server, matching `AppState::new`'s own default so a
/// deployment that never sets this var behaves identically to one built
/// straight from `AppState::new` with no builder calls.
pub fn parse_console_origins(raw: Option<&str>) -> Vec<String> {
    let default = || vec!["http://localhost:5173".to_string()];
    match raw {
        None => default(),
        Some(raw) => {
            let origins: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if origins.is_empty() {
                default()
            } else {
                origins
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_falls_back_to_local_console_dev_server() {
        assert_eq!(
            parse_console_origins(None),
            vec!["http://localhost:5173".to_string()]
        );
    }

    #[test]
    fn empty_string_falls_back_to_the_same_default() {
        assert_eq!(
            parse_console_origins(Some("")),
            vec!["http://localhost:5173".to_string()]
        );
    }

    #[test]
    fn splits_and_trims_a_comma_separated_list() {
        assert_eq!(
            parse_console_origins(Some(" https://console.oxhive.dev , http://localhost:5173 ")),
            vec![
                "https://console.oxhive.dev".to_string(),
                "http://localhost:5173".to_string(),
            ]
        );
    }

    #[test]
    fn drops_empty_entries_from_a_trailing_comma() {
        assert_eq!(
            parse_console_origins(Some("https://console.oxhive.dev,")),
            vec!["https://console.oxhive.dev".to_string()]
        );
    }
}
