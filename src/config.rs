use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub sqld_url: String,
    pub sqld_admin_url: String,
    pub listen_addr: String,
    pub metrics_token: String,
}

impl Config {
    pub fn from_env() -> Result<Config> {
        let metrics_token =
            std::env::var("METRICS_TOKEN").context("METRICS_TOKEN must be set")?;
        if metrics_token.is_empty() {
            anyhow::bail!(
                "METRICS_TOKEN must not be empty — an empty value fails closed and permanently \
                 401s GET /metrics"
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
        })
    }
}
