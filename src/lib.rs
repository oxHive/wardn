pub mod auth;
pub mod config;
pub mod db;
pub mod proxy;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, get},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .with_state(state)
}
