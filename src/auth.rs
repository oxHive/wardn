use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{SaltString, rand_core::OsRng};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::Rng;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}

/// Generates a new API key. Returns (full_key, prefix, hash) — the caller
/// shows full_key to the user exactly once and stores only prefix+hash.
pub fn generate_api_key() -> (String, String, String) {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    let random_part: String = (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let full_key = format!("hm_live_{random_part}");
    let prefix = full_key.chars().take(12).collect::<String>();
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(full_key.as_bytes(), &salt)
        .expect("argon2 hashing does not fail for well-formed input")
        .to_string();
    (full_key, prefix, hash)
}

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
    let prefix: String = full_key.chars().take(12).collect();

    let row = match db::find_api_key_by_prefix(&state.pool, &prefix).await {
        Ok(row) => row,
        Err(e) => {
            tracing::error!("api key lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(row) = row else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if row.revoked_at.is_some() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !verify_key(full_key, &row.key_hash) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    request.extensions_mut().insert(AuthedOwner {
        owner_type: row.owner_type,
        owner_id: row.owner_id,
    });
    next.run(request).await
}
