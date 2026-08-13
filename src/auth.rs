use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use rand::Rng;
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;
use crate::proxy::ProxyClient;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Base URL of the sqld instance to proxy to, e.g. `http://127.0.0.1:8081`.
    /// Held here rather than read from the environment inside the handler so
    /// that it is configured exactly once, in one place, and tests can point
    /// it wherever they like.
    pub sqld_url: String,
    /// Base URL of sqld's admin API, e.g. `http://127.0.0.1:8090` — used by
    /// database provisioning (`src/provisioning.rs`) to create a namespace.
    /// Defaults to empty via `new`; set it with `with_sqld_admin_url` where
    /// provisioning is actually exercised (`main.rs`, provisioning tests).
    /// Every other existing test/call site is unaffected by its absence.
    pub sqld_admin_url: String,
    /// Pooled outbound HTTP clients, shared by every request.
    pub client: ProxyClient,
    /// Renders `GET /metrics` (`src/observability.rs`). `new` builds a local,
    /// non-globally-installed handle so every existing call site keeps
    /// compiling unchanged; `main.rs` overrides it via `with_metrics_handle`
    /// with the handle tied to the globally installed recorder — the one the
    /// `metrics::counter!`/`gauge!`/`histogram!` calls elsewhere in this
    /// crate actually feed. A handle not tied to the global recorder still
    /// renders successfully, just always as empty output.
    pub metrics_handle: PrometheusHandle,
    /// Shared secret required as `Authorization: Bearer <token>` on `GET
    /// /metrics` (`src/observability.rs`) — added because the per-tenant
    /// `namespace` label on proxy metrics would otherwise let anyone
    /// enumerate every tenant's UUID and traffic volume through an
    /// unauthenticated endpoint. Defaults to empty via `new`, which makes
    /// `metrics_handler` reject every request (fail closed) until
    /// `with_metrics_token` sets a real value — `main.rs` does this from
    /// `Config::metrics_token`.
    pub metrics_token: String,
    /// Server-side secret folded into every API key's HMAC-SHA256 hash (see
    /// `hash_key`/`generate_api_key`/`verify_key` below). Unlike
    /// `metrics_token`/`sqld_admin_url`, this is a **required constructor
    /// argument**, not an optional builder: it's load-bearing for every
    /// single authenticated request (`auth_middleware`), not one endpoint —
    /// an empty-by-default value here would make every key verification
    /// silently use an empty pepper until someone remembered to set it,
    /// which is exactly the kind of security-relevant omission that should
    /// be a compile error, not a runtime footgun.
    pub api_key_pepper: String,
}

impl AppState {
    pub fn new(pool: PgPool, sqld_url: String, api_key_pepper: String) -> Self {
        let metrics_handle = PrometheusBuilder::new().build_recorder().handle();
        Self {
            pool,
            sqld_url,
            sqld_admin_url: String::new(),
            client: ProxyClient::new(),
            metrics_handle,
            metrics_token: String::new(),
            api_key_pepper,
        }
    }

    pub fn with_sqld_admin_url(mut self, sqld_admin_url: String) -> Self {
        self.sqld_admin_url = sqld_admin_url;
        self
    }

    pub fn with_metrics_handle(mut self, metrics_handle: PrometheusHandle) -> Self {
        self.metrics_handle = metrics_handle;
        self
    }

    pub fn with_metrics_token(mut self, metrics_token: String) -> Self {
        self.metrics_token = metrics_token;
        self
    }
}

/// Marker every issued key starts with. The DB lookup prefix is derived from
/// the *random* part only — including this constant in the prefix would burn
/// 8 of its characters on a value that is identical for every key.
pub const KEY_MARKER: &str = "hm_live_";

/// Number of characters of the random part used as the DB lookup prefix.
/// 16 characters from a 62-symbol alphabet is ~62^16 ≈ 2^95 possibilities, so
/// the `UNIQUE` constraint on `api_keys.prefix` will never realistically be
/// hit by an issuance collision.
const PREFIX_LEN: usize = 16;

/// Derives the `api_keys.prefix` lookup value from a presented bearer token.
/// Returns `None` for anything that isn't one of our keys, so the caller can
/// reject it without touching the database.
pub fn prefix_of(full_key: &str) -> Option<String> {
    let random_part = full_key.strip_prefix(KEY_MARKER)?;
    let prefix: String = random_part.chars().take(PREFIX_LEN).collect();
    if prefix.chars().count() < PREFIX_LEN {
        return None;
    }
    Some(prefix)
}

/// HMAC-SHA256 of `full_key`, keyed by `pepper`, as a lowercase hex string.
///
/// API keys are 32 random characters from a 62-symbol alphabet (~190 bits of
/// entropy, see `generate_api_key`) — nowhere near brute-forceable, so unlike
/// a human-chosen password there is no reason to slow this down with a
/// memory-hard KDF. A slow hash on every request (this project previously
/// used argon2) is instead a straightforward unauthenticated-DoS amplifier:
/// every request, including ones with a bogus key, paid ~50-100ms of CPU and
/// ~19MiB of memory synchronously on the async runtime. HMAC-SHA256 costs
/// about 1 microsecond. `pepper` is a server-side secret distinct from any
/// individual key, so a leaked `key_hash` column alone doesn't let an
/// attacker forge valid keys without also knowing it.
fn hash_key(pepper: &[u8], full_key: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(pepper).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(full_key.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Generates a new API key. Returns (full_key, prefix, hash) — the caller
/// shows full_key to the user exactly once and stores only prefix+hash.
pub fn generate_api_key(pepper: &[u8]) -> (String, String, String) {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let random_part: String = (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let full_key = format!("{KEY_MARKER}{random_part}");
    let prefix: String = random_part.chars().take(PREFIX_LEN).collect();
    let hash = hash_key(pepper, &full_key);
    (full_key, prefix, hash)
}

/// Verifies `full_key` against a stored `hash` (hex-encoded HMAC-SHA256).
/// `Mac::verify_slice` compares in constant time internally — no separate
/// constant-time-comparison crate needed for this path.
pub fn verify_key(pepper: &[u8], full_key: &str, hash: &str) -> bool {
    let Ok(expected) = hex::decode(hash) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(pepper) else {
        return false;
    };
    mac.update(full_key.as_bytes());
    mac.verify_slice(&expected).is_ok()
}

/// Inserted into request extensions by `auth_middleware` on a successful
/// auth. Read downstream via axum's built-in `axum::Extension<AuthedOwner>`
/// extractor — no custom `FromRequestParts` impl needed, `Extension<T>`
/// already does exactly this for any `T: Clone + Send + Sync + 'static`
/// present in the request's extensions.
#[derive(Debug, Clone)]
pub struct AuthedOwner {
    pub owner_type: String,
    pub owner_id: Uuid,
}

/// Middleware: validates the Authorization bearer token against Postgres,
/// and — on success — inserts an AuthedOwner extension for downstream
/// extractors/handlers to read. Rejects with 401 on any failure (missing
/// header, unknown prefix, hash mismatch, or revoked key).
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let Some(full_key) = auth_header else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(prefix) = prefix_of(full_key) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };

    let row = match db::find_api_key_by_prefix(&state.pool, &prefix).await {
        Ok(row) => row,
        Err(e) => {
            tracing::error!("api key lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(row) = row else {
        // Unknown prefix, or a revoked key — `find_api_key_by_prefix` only
        // matches non-revoked rows, so the two cases are indistinguishable
        // here, and both already get the same 401.
        //
        // Verify against a throwaway hash anyway. Without this, an unknown
        // prefix returns in microseconds while a known-but-wrong key pays
        // the same ~1us HMAC cost as a genuine verification — a timing
        // oracle for "does this prefix exist," same reasoning as before this
        // moved from argon2 to HMAC, just at a much smaller absolute cost.
        let dummy_hash = hash_key(state.api_key_pepper.as_bytes(), "not-a-real-api-key");
        let _ = verify_key(state.api_key_pepper.as_bytes(), full_key, &dummy_hash);
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !verify_key(state.api_key_pepper.as_bytes(), full_key, &row.key_hash) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    request.extensions_mut().insert(AuthedOwner {
        owner_type: row.owner_type,
        owner_id: row.owner_id,
    });
    next.run(request).await
}
