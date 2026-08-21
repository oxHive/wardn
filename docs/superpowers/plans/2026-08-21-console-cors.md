# hivewarden CORS Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a browser-based client (the `console` app) call hivewarden's API cross-origin, which is blocked outright today — hivewarden has no CORS layer at all.

**Architecture:** A `tower_http::cors::CorsLayer`, added as the outermost layer in `app()` (`src/lib.rs`) so a CORS preflight is answered before it ever reaches `auth_middleware`. Allowed origins are carried on `AppState` (new `cors_origins: Vec<String>` field, set via a builder method matching the existing `with_sqld_admin_url`/`with_metrics_token` pattern) and sourced from a new `CONSOLE_ORIGINS` env var, comma-separated, defaulting to `http://localhost:5173` when unset.

**Tech Stack:** axum 0.8, tower-http 0.7 (`cors` feature, added by this plan), same as the rest of hivewarden.

**Spec:** `console/docs/superpowers/specs/2026-08-21-console-app-design.md`, section "hivewarden change" — this is the one piece of that spec's work that lives in this repo, not `console`.

## Global Constraints

- Rust edition 2024, axum 0.8, sqlx 0.8 Postgres, matching the rest of the crate.
- No credentialed requests (no cookies) — console authenticates via `Authorization: Bearer <api_key>`, so the CORS layer must **not** set `allow_credentials(true)`.
- `AppState::new(...)` must keep compiling and passing exactly as today for every existing test that doesn't call `.with_cors_origins(...)` — give the field a sane default (the same `http://localhost:5173` default `Config` uses) rather than making it a required constructor argument.
- Requests with no `Origin` header (every existing test, every non-browser caller) must be completely unaffected — `tower_http`'s `CorsLayer` only adds headers when an `Origin` header is present, so this holds automatically as long as the layer is wired correctly.

---

### Task 1: `CorsLayer` on `AppState`, wired into `app()`

**Files:**
- Modify: `Cargo.toml` (add `cors` to `tower-http`'s features)
- Modify: `src/auth.rs` (add `cors_origins` field + builder to `AppState`)
- Modify: `src/lib.rs` (build and attach the `CorsLayer` in `app()`)
- Test: `tests/cors_test.rs` (new)

**Interfaces:**
- Consumes: nothing from an earlier task (this is the foundation).
- Produces (for Task 2):
  - `AppState.cors_origins: Vec<String>`
  - `AppState::with_cors_origins(mut self, origins: Vec<String>) -> Self`

- [ ] **Step 1: Write the failing test**

Create `tests/cors_test.rs`:

```rust
mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hivewarden::AppState;
use hivewarden::db;
use tower::ServiceExt;

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://gateway:gateway@127.0.0.1:5433/gateway".to_string());
    db::connect(&url).await.expect("connect to test postgres")
}

fn test_sqld_url() -> String {
    std::env::var("SQLD_URL").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string())
}

#[tokio::test]
async fn allowed_origin_gets_the_cors_header() {
    let pool = test_pool().await;
    let app = hivewarden::app(
        AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string())
            .with_cors_origins(vec!["http://localhost:5173".to_string()]),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("origin", "http://localhost:5173")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "http://localhost:5173"
    );
}

#[tokio::test]
async fn disallowed_origin_gets_no_cors_header() {
    let pool = test_pool().await;
    let app = hivewarden::app(
        AppState::new(pool, test_sqld_url(), common::TEST_API_KEY_PEPPER.to_string())
            .with_cors_origins(vec!["http://localhost:5173".to_string()]),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("origin", "http://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn request_with_no_origin_is_unaffected() {
    let pool = test_pool().await;
    let app = hivewarden::app(AppState::new(
        pool,
        test_sqld_url(),
        common::TEST_API_KEY_PEPPER.to_string(),
    ));
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test cors_test`
Expected: compile failure — `with_cors_origins` doesn't exist yet on `AppState`.

- [ ] **Step 3: Add the `cors` feature to tower-http**

In `Cargo.toml`, change:

```toml
tower-http = { version = "0.7.0", features = ["trace"] }
```

to:

```toml
tower-http = { version = "0.7.0", features = ["trace", "cors"] }
```

- [ ] **Step 4: Add `cors_origins` to `AppState`**

In `src/auth.rs`, add the field to the `AppState` struct (near `api_key_pepper`, same doc-comment style as its neighbors):

```rust
    /// Origins a browser-based client (the `console` app) is allowed to call
    /// this API from cross-origin. Defaults to the local `console` dev
    /// server via `new` — set to the real deployed origin(s) with
    /// `with_cors_origins`, same pattern as `with_sqld_admin_url`. See
    /// `app()` in `lib.rs`, which builds the actual `CorsLayer` from this.
    pub cors_origins: Vec<String>,
```

In `AppState::new`, initialize it in the struct literal:

```rust
            api_key_pepper,
            cors_origins: vec!["http://localhost:5173".to_string()],
```

Add the builder method alongside `with_sqld_admin_url`/`with_metrics_handle`/`with_metrics_token`:

```rust
    pub fn with_cors_origins(mut self, cors_origins: Vec<String>) -> Self {
        self.cors_origins = cors_origins;
        self
    }
```

- [ ] **Step 5: Build and attach the `CorsLayer` in `app()`**

In `src/lib.rs`, add the import alongside the existing `tower_http` import:

```rust
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::{DefaultOnResponse, TraceLayer};
```

Inside `app()`, before the `Router::new()` chain (so `state` is still in scope — `state` is consumed by `.with_state(state)` at the very end), build the layer:

```rust
    let cors_origins: Vec<axum::http::HeaderValue> = state
        .cors_origins
        .iter()
        .filter_map(|origin| axum::http::HeaderValue::from_str(origin).ok())
        .collect();
    let cors_layer = CorsLayer::new()
        .allow_origin(AllowOrigin::list(cors_origins))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
        ])
        .allow_headers([axum::http::header::AUTHORIZATION, axum::http::header::CONTENT_TYPE]);
```

Then attach it as the outermost layer — add `.layer(cors_layer)` right after the existing `TraceLayer` block and before `.with_state(state)`:

```rust
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_span)
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        )
        .layer(cors_layer)
        .with_state(state)
```

(Leave the `TraceLayer` block's existing comment above `.on_response(...)` untouched — only the two new lines around it change.)

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test cors_test`
Expected: PASS (all three tests)

- [ ] **Step 7: Run the full existing test suite to confirm nothing else broke**

Run: `cargo test`
Expected: PASS (no regressions — no other test sends an `Origin` header, so none of them observe any behavior change)

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/auth.rs src/lib.rs tests/cors_test.rs
git commit -m "Add CORS support so browser clients can call the API cross-origin"
```

---

### Task 2: `CONSOLE_ORIGINS` env var wiring

**Files:**
- Modify: `src/config.rs` (add `console_origins`, parsed from `CONSOLE_ORIGINS`)
- Modify: `src/main.rs` (pass `config.console_origins` into `AppState` via `.with_cors_origins`)
- Modify: `.env.example` (document the new var)
- Test: `src/config.rs` (inline `#[cfg(test)]` module, matching this file's existing style — it currently has none, so this introduces the pattern the way `src/roles.rs` already does elsewhere in the crate)

**Interfaces:**
- Consumes: `AppState::with_cors_origins` (Task 1).
- Produces: nothing further downstream — this is the last task in this plan.

- [ ] **Step 1: Write the failing test**

In `src/config.rs`, add at the bottom of the file:

```rust
/// Parses `CONSOLE_ORIGINS` (comma-separated, whitespace around each entry
/// trimmed, empty entries dropped) into the list `AppState::cors_origins`
/// wants. Pulled out as its own function — separate from `Config::from_env`
/// — so it's testable without needing every other required env var set.
/// `None` (the var is unset) and `Some("")` both fall back to the local
/// `console` dev server, matching `AppState::new`'s own default so a
/// deployment that never sets this var behaves identically to one built
/// straight from `AppState::new` with no builder calls.
pub fn parse_console_origins(raw: Option<&str>) -> Vec<String> {
    let default = || vec!["http://localhost:5173".to_string()];
    match raw {
        None => default(),
        Some(raw) => {
            let origins: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if origins.is_empty() { default() } else { origins }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_falls_back_to_local_console_dev_server() {
        assert_eq!(parse_console_origins(None), vec!["http://localhost:5173".to_string()]);
    }

    #[test]
    fn empty_string_falls_back_to_the_same_default() {
        assert_eq!(parse_console_origins(Some("")), vec!["http://localhost:5173".to_string()]);
    }

    #[test]
    fn splits_and_trims_a_comma_separated_list() {
        assert_eq!(
            parse_console_origins(Some(" https://console.oxhive.dev , http://localhost:5173 ")),
            vec![
                "https://console.oxhive.dev".to_string(),
                "http://localhost:5173".to_string(),
            ]
        );
    }

    #[test]
    fn drops_empty_entries_from_a_trailing_comma() {
        assert_eq!(
            parse_console_origins(Some("https://console.oxhive.dev,")),
            vec!["https://console.oxhive.dev".to_string()]
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests`
Expected: FAIL — `parse_console_origins` doesn't exist yet (the four test functions above it, which you just added in the same edit, will actually compile and pass immediately since the function body was written in the same step; to genuinely see red first, comment out the function body and have it `unimplemented!()` before running this step, then restore it in Step 3). If you'd rather not do the comment-out dance, it's acceptable to write Steps 1–3 as one combined edit for this small a function — but still run the command below afterward to confirm green.

- [ ] **Step 3: Wire `console_origins` into `Config`**

In `src/config.rs`, add the field to the `Config` struct:

```rust
    /// Origins allowed to call this API cross-origin — see `parse_console_origins`
    /// below and `AppState::cors_origins` (`src/auth.rs`).
    pub console_origins: Vec<String>,
```

In `Config::from_env`, add to the struct literal:

```rust
            console_origins: parse_console_origins(std::env::var("CONSOLE_ORIGINS").ok().as_deref()),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib config::tests`
Expected: PASS (all four tests)

- [ ] **Step 5: Wire it through `main.rs`**

In `src/main.rs`, the `AppState` construction currently reads:

```rust
    let state = AppState::new(pool, config.sqld_url.clone(), config.api_key_pepper.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone())
        .with_metrics_handle(metrics_handle)
        .with_metrics_token(config.metrics_token.clone());
```

Add one more builder call:

```rust
    let state = AppState::new(pool, config.sqld_url.clone(), config.api_key_pepper.clone())
        .with_sqld_admin_url(config.sqld_admin_url.clone())
        .with_metrics_handle(metrics_handle)
        .with_metrics_token(config.metrics_token.clone())
        .with_cors_origins(config.console_origins.clone());
```

- [ ] **Step 6: Document the new env var**

In `.env.example`, add after `API_KEY_PEPPER`:

```
# Optional. Comma-separated list of origins the console app is allowed to
# call this API from cross-origin (src/auth.rs's CorsLayer). Unset defaults
# to just the local console dev server, http://localhost:5173.
CONSOLE_ORIGINS=http://localhost:5173,https://console.oxhive.dev
```

- [ ] **Step 7: Full build + test suite**

Run: `cargo build && cargo test`
Expected: PASS, no warnings about the new field being unused.

- [ ] **Step 8: Commit**

```bash
git add src/config.rs src/main.rs .env.example
git commit -m "Wire CORS allowed origins from a CONSOLE_ORIGINS env var"
```
