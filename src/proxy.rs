use axum::Extension;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::auth::{AppState, AuthedOwner};
use crate::routing;

/// sqld (v0.24.33, ghcr.io/tursodatabase/libsql-server) does not use a
/// custom header for namespace selection. It resolves the namespace from
/// the `Host` header, subdomain-style: the label before the first `.` in
/// the `Host` value is treated as the namespace name (404s if it doesn't
/// exist); a non-dotted `Host` falls back to the `default` namespace. This
/// was verified empirically against the real container in Task 1 (curl)
/// and re-verified here for `reqwest` specifically (see task-5-report.md) —
/// `reqwest::RequestBuilder::header(HOST, ...)` is sent through unmodified,
/// so no DNS-resolver workaround is needed.
pub async fn proxy_handler(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    request: Request,
) -> Response {
    let namespace = match routing::resolve_namespace(&state.pool, &owner).await {
        Ok(ns) => ns,
        Err(status) => return status.into_response(),
    };

    let sqld_url = match std::env::var("SQLD_URL") {
        Ok(url) => url,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let (parts, body) = request.into_parts();
    let target = format!(
        "{}{}",
        sqld_url.trim_end_matches('/'),
        parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
    );

    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let client = reqwest::Client::new();
    let mut req_builder = client.request(
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap(),
        &target,
    );

    let mut forwarded_headers = HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        if name == header::AUTHORIZATION || name == header::HOST {
            continue;
        }
        forwarded_headers.insert(name.clone(), value.clone());
    }
    // Namespace selection for sqld happens via the Host header (see module
    // doc comment above) — not a custom header. Rewrite it to
    // `<namespace>.local` so sqld resolves the request to this owner's
    // namespace.
    let namespace_host = format!("{namespace}.local");
    forwarded_headers.insert(
        header::HOST,
        HeaderValue::from_str(&namespace_host).unwrap(),
    );

    for (name, value) in forwarded_headers.iter() {
        req_builder = req_builder.header(name, value);
    }
    req_builder = req_builder.body(body_bytes);

    let upstream_resp = match req_builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("proxy request to sqld failed: {e:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = upstream_resp.status();
    let headers = upstream_resp.headers().clone();
    let bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("reading sqld response failed: {e:#}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let mut response = Response::builder()
        .status(status.as_u16())
        .body(Body::from(bytes))
        .unwrap();
    *response.headers_mut() = headers
        .iter()
        .filter_map(|(k, v)| {
            Some((
                HeaderName::from_bytes(k.as_str().as_bytes()).ok()?,
                HeaderValue::from_bytes(v.as_bytes()).ok()?,
            ))
        })
        .collect();
    response
}
