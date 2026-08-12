pub mod auth;
pub mod config;
pub mod db;
pub mod org_admin;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route(
            "/orgs/{org_id}/roles",
            post(org_admin::create_role).get(org_admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org_admin::update_role).delete(org_admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org_admin::assign_member_role),
        )
        .route("/{*path}", any(proxy::proxy_handler))
        .route("/", any(proxy::proxy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .route("/healthz", get(healthz))
        .route("/users", post(registration::create_user))
        .with_state(state)
}
