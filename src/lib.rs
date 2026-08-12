pub mod api_keys;
pub mod auth;
pub mod config;
pub mod db;
pub mod org;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    routing::{any, delete, get, patch, post, put},
};

async fn healthz() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route(
            "/api-keys",
            get(api_keys::list_keys).post(api_keys::create_key),
        )
        .route("/api-keys/{id}", delete(api_keys::revoke_key))
        .route("/orgs", post(registration::create_org))
        .route(
            "/orgs/{org_id}/members",
            get(org::members::list_members).post(org::members::add_member),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}",
            delete(org::members::remove_member),
        )
        .route(
            "/orgs/{org_id}/roles",
            post(org::admin::create_role).get(org::admin::list_roles),
        )
        .route(
            "/orgs/{org_id}/roles/{role_id}",
            patch(org::admin::update_role).delete(org::admin::delete_role),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}/role",
            put(org::admin::assign_member_role),
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
