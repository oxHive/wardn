pub mod api_keys;
pub mod auth;
pub mod config;
pub mod db;
pub mod observability;
pub mod org;
pub mod provisioning;
pub mod proxy;
pub mod registration;
pub mod roles;
pub mod routing;

pub use auth::AppState;
use axum::{
    Router,
    extract::ConnectInfo,
    routing::{any, delete, get, patch, post, put},
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tower_governor::{
    GovernorError, GovernorLayer, governor::GovernorConfigBuilder, key_extractor::KeyExtractor,
};

async fn healthz() -> &'static str {
    "ok"
}

/// Keys the `/users` rate limiter on peer IP, like `tower_governor`'s own
/// `PeerIpKeyExtractor` — except that extractor treats a missing
/// `ConnectInfo<SocketAddr>` extension as a hard error (a 500 to the
/// caller), and this crate's integration-test suite is full of
/// `Router::oneshot` calls that hit `POST /users` as setup for tests that
/// have nothing to do with rate limiting (`oneshot` never populates
/// `ConnectInfo` — only a real listener via
/// `into_make_service_with_connect_info` does, see `main.rs`). Falling back
/// to a fixed placeholder key instead keeps those callers bucketed together
/// under one shared quota rather than failing outright; since production
/// (`main.rs`) always serves through a real listener with `ConnectInfo`
/// wired up, this fallback is never reached outside tests, and genuine
/// per-peer-IP limiting is unaffected.
#[derive(Debug, Clone, Copy)]
struct PeerIpOrFallbackKeyExtractor;

impl KeyExtractor for PeerIpOrFallbackKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &axum::http::Request<T>) -> Result<Self::Key, GovernorError> {
        Ok(req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)))
    }
}

pub fn app(state: AppState) -> Router {
    // 5 requests, then a slow trickle — `tower_governor`'s model is a
    // token-bucket refilling at `per_second`, not a strict rolling-hour
    // window, so a burst of 5 immediately followed by one token every 720
    // seconds approximates "5/hour/IP" closely enough for this project's
    // current needs (see the design spec's finding #3). Only `/users` gets
    // this — it's public and unauthenticated, the most exposed endpoint.
    // `/orgs` deliberately does NOT: it's authenticated, so the per-user
    // quota in `registration::create_org` is the more precise defense there.
    let registration_governor_config = GovernorConfigBuilder::default()
        .key_extractor(PeerIpOrFallbackKeyExtractor)
        .per_second(720)
        .burst_size(5)
        .finish()
        .expect("static governor config values are always valid");
    let registration_limiter = GovernorLayer::new(registration_governor_config);

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
        .route("/metrics", get(observability::metrics_handler))
        .route(
            "/users",
            post(registration::create_user).route_layer(registration_limiter),
        )
        .with_state(state)
}
