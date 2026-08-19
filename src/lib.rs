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
pub mod telemetry;

pub use auth::AppState;
use axum::{
    Router,
    extract::ConnectInfo,
    routing::{any, delete, get, patch, post, put},
};
use opentelemetry::trace::TraceContextExt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tower_governor::{
    GovernorError, GovernorLayer, governor::GovernorConfigBuilder, key_extractor::KeyExtractor,
};
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// How often the `/users` rate limiter's per-IP state map is swept of
/// entries that haven't been touched recently. `tower_governor` does not do
/// this on its own — its own README example spawns exactly this kind of
/// timer, and without one the map only grows, one entry per distinct source
/// IP that has ever hit `POST /users`. Left unbounded, that's a
/// memory-exhaustion vector on the very endpoint this rate limiter exists to
/// protect (trivially reachable with many source addresses — an IPv6 /64, a
/// botnet — each only needing to send one request to add an entry).
const GOVERNOR_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

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
/// per-peer-IP limiting is unaffected. If it ever *did* fire in a real
/// deployment (e.g. some future entrypoint that skips
/// `into_make_service_with_connect_info`), the failure mode is fail-shut,
/// not fail-open: every caller would collapse into one shared bucket and
/// hit the rate limit *sooner* than intended, never later.
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

/// Creates the one-per-request span `TraceLayer` attaches below. Stamps a
/// `trace_id` field onto it immediately, read back from this span's own
/// OTel context via `OpenTelemetrySpanExt` — this is what makes every JSON
/// log line emitted during the request carry a `trace_id` (the fmt layer's
/// default `with_current_span(true)` includes it), which is what Loki's
/// derived field (`grafana/provisioning/datasources/loki.yml`) keys on to
/// link a log line to its Tempo trace. Works whether or not the OTel layer
/// is actually active (`main.rs`, Task 6) — with no OTel layer registered,
/// `context().span().span_context()` is a valid-but-empty span context, and
/// `trace_id` renders as all-zeroes rather than failing.
fn make_span(request: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    let span = tracing::info_span!(
        "http_request",
        method = %request.method(),
        path = %request.uri().path(),
        trace_id = tracing::field::Empty,
    );
    let trace_id = span.context().span().span_context().trace_id();
    span.record("trace_id", tracing::field::display(trace_id));
    span
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

    // `app()` is called once per process in production (`main.rs`) and once
    // per test — either way, a background sweep that outlives the caller's
    // interest in it is harmless: it just runs until the process (or test
    // binary) exits.
    let limiter = registration_governor_config.limiter().clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(GOVERNOR_CLEANUP_INTERVAL).await;
            limiter.retain_recent();
        }
    });

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
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_span)
                // Default `on_response` logs at DEBUG, which the default `info`
                // level floor (`main.rs`) swallows entirely — meaning any
                // request handler that never emits its own event (every
                // control-plane handler except `proxy_handler`'s usage event)
                // leaves no log line carrying this request's `trace_id` for
                // Loki to correlate against its Tempo trace. Raising this to
                // INFO gives every request exactly one such line for free.
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        )
        .with_state(state)
}
