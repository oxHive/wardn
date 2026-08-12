use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{SaltString, rand_core::OsRng};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use rand::Rng;
use sqlx::PgPool;
use std::sync::LazyLock;
use uuid::Uuid;

use crate::db;
use crate::proxy::ProxyClient;

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
}

impl AppState {
    pub fn new(pool: PgPool, sqld_url: String) -> Self {
        let metrics_handle = PrometheusBuilder::new().build_recorder().handle();
        Self {
            pool,
            sqld_url,
            sqld_admin_url: String::new(),
            client: ProxyClient::new(),
            metrics_handle,
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

/// Generates a new API key. Returns (full_key, prefix, hash) — the caller
/// shows full_key to the user exactly once and stores only prefix+hash.
pub fn generate_api_key() -> (String, String, String) {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let random_part: String = (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let full_key = format!("{KEY_MARKER}{random_part}");
    let prefix: String = random_part.chars().take(PREFIX_LEN).collect();
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(full_key.as_bytes(), &salt)
        .expect("argon2 hashing does not fail for well-formed input")
        .to_string();
    (full_key, prefix, hash)
}

/// A real argon2 hash of a fixed throwaway secret, computed once. Used only
/// to spend the same CPU on the "no such prefix" path as on a genuine
/// verification (see `auth_middleware`); it can never match a presented key.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    let salt = SaltString::from_b64("aGl2ZW1pbmRnYXRld2F5")
        .expect("static salt is valid base64 of a legal length");
    Argon2::default()
        .hash_password(b"not-a-real-api-key", &salt)
        .expect("argon2 hashing does not fail for well-formed input")
        .to_string()
});

pub fn verify_key(full_key: &str, hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(full_key.as_bytes(), &parsed_hash)
        .is_ok()
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
        // prefix returns in microseconds while a known-but-wrong key pays the
        // full argon2 cost — a timing oracle for "does this prefix exist".
        let _ = verify_key(full_key, &DUMMY_HASH);
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !verify_key(full_key, &row.key_hash) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    request.extensions_mut().insert(AuthedOwner {
        owner_type: row.owner_type,
        owner_id: row.owner_id,
    });
    next.run(request).await
}
