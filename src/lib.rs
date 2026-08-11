pub mod config;

use axum::{Router, routing::get};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app() -> Router {
    Router::new().route("/healthz", get(healthz))
}
