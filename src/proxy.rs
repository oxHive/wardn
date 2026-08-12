use std::collections::HashSet;
use std::time::{Duration, Instant};

use axum::Extension;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, Version, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use uuid::Uuid;

use crate::auth::{AppState, AuthedOwner};
use crate::roles::{self, Permission};
use crate::routing;

/// sqld exposes **two** namespace selectors on the same HTTP port, both
/// verified empirically against the real container
/// (`ghcr.io/tursodatabase/libsql-server`):
///
/// * `Host` — subdomain-style: the label before the first `.` names the
///   namespace; a non-dotted `Host` falls back to `default`. Honoured on the
///   Hrana/HTTP endpoints only.
/// * `x-namespace-bin` — gRPC binary metadata carrying *unpadded* base64 of
///   the namespace name. This is the **only** selector the replication gRPC
///   service (`/wal_log.ReplicationLog/*`, what hivemind's embedded-replica
///   sync client speaks) understands, and on the Hrana/HTTP endpoints it
///   *overrides* `Host`.
///
/// Both are therefore set authoritatively here from the namespace resolved
/// out of Postgres, and any client-supplied copy of either is stripped first
/// — a client that could smuggle its own `x-namespace-bin` through would read
/// and write any other tenant's namespace with its own valid API key.
const X_NAMESPACE_BIN: HeaderName = HeaderName::from_static("x-namespace-bin");

/// Selects an org-shared namespace instead of the caller's own mapping.
/// Absent → today's behavior (personal key → personal namespace, no org
/// lookup at all). Present → the caller must be a member of this org with a
/// role granting the permission required for the outbound protocol; see
/// `proxy_handler`. Stripped from the forwarded headers the same way
/// `x-namespace-bin`/`Authorization`/`Host` are — it's a gateway-internal
/// selector, sqld has no notion of it.
const X_ORG_ID: HeaderName = HeaderName::from_static("x-org-id");

/// RFC 9110 §7.6.1 connection-specific ("hop-by-hop") header fields. A proxy
/// must not forward these in either direction: they describe the single
/// connection they arrived on, not the message. Forwarding them also breaks
/// HTTP/2 outright — h2 bans `connection`, `keep-alive`, `transfer-encoding`
/// and `upgrade` on the wire.
///
/// `connection` itself may *name* further headers to drop; see
/// [`connection_named_headers`].
const HOP_BY_HOP_HEADERS: [HeaderName; 8] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Pooled outbound clients, held in [`AppState`] and reused across requests
/// (one per process, not one per request — a fresh client per request throws
/// away the connection pool and forces a new TCP + h2 handshake every time).
///
/// Two of them, because sqld's gRPC endpoints only work over HTTP/2 while its
/// Hrana endpoints work over both, and `hyper_util`'s legacy client cannot
/// negotiate h2c on the fly (there is no ALPN on cleartext). The proxy mirrors
/// whatever version the *client* used: HTTP/2 in means prior-knowledge h2c
/// out, anything else means HTTP/1.1 out.
#[derive(Clone, Debug)]
pub struct ProxyClient {
    http1: Client<HttpConnector, Body>,
    http2: Client<HttpConnector, Body>,
}

/// How long to wait for a TCP connection to sqld before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a pooled connection may sit idle before it's closed. Bounds how
/// long a stuck-but-not-dropped sqld connection can occupy a pool slot.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// How long to wait for sqld's response *head* after sending a request.
/// Deliberately not a whole-request timeout: a streaming gRPC response can
/// legitimately run far longer than this once headers arrive.
const RESPONSE_HEAD_TIMEOUT: Duration = Duration::from_secs(30);

fn connector_with_timeout() -> HttpConnector {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
    connector
}

impl ProxyClient {
    pub fn new() -> Self {
        Self {
            http1: Client::builder(TokioExecutor::new())
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .build(connector_with_timeout()),
            http2: Client::builder(TokioExecutor::new())
                .http2_only(true)
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .build(connector_with_timeout()),
        }
    }

    fn for_version(&self, version: Version) -> (&Client<HttpConnector, Body>, Version) {
        if version == Version::HTTP_2 {
            (&self.http2, Version::HTTP_2)
        } else {
            (&self.http1, Version::HTTP_11)
        }
    }
}

impl Default for ProxyClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Header names listed in the request's `Connection` field, which RFC 9110
/// §7.6.1 also makes hop-by-hop for this message.
fn connection_named_headers(headers: &HeaderMap) -> HashSet<HeaderName> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|token| HeaderName::from_bytes(token.trim().as_bytes()).ok())
        .collect()
}

/// Copies `src` into a fresh [`HeaderMap`], dropping hop-by-hop headers,
/// anything named by `Connection`, and anything in `also_drop`.
///
/// Uses `append`, not `insert`: a message may legitimately carry the same
/// field name more than once (`Cookie`, `Set-Cookie`, `Via`, ...) and
/// `insert` would silently keep only the last value.
fn forwardable_headers(src: &HeaderMap, also_drop: &[HeaderName]) -> HeaderMap {
    let connection_named = connection_named_headers(src);
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src.iter() {
        if HOP_BY_HOP_HEADERS.contains(name)
            || connection_named.contains(name)
            || also_drop.contains(name)
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Parses the optional `X-Org-Id` header. `None` when absent. `Some(Err(()))`
/// when present but not a valid UUID — the caller turns that into 400 rather
/// than silently falling back to the personal-namespace path, since a
/// malformed org id is almost certainly a client bug worth surfacing.
fn org_id_header(headers: &HeaderMap) -> Option<Result<Uuid, ()>> {
    let raw = headers.get(&X_ORG_ID)?;
    Some(
        raw.to_str()
            .ok()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(()),
    )
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    Extension(owner): Extension<AuthedOwner>,
    request: Request,
) -> Response {
    let start = Instant::now();
    let (parts, body) = request.into_parts();

    let mut org_id_for_usage: Option<Uuid> = None;
    let namespace = match org_id_header(&parts.headers) {
        Some(Ok(org_id)) => {
            org_id_for_usage = Some(org_id);
            // Mirrors the HTTP/2-in-means-h2c-out fork `ProxyClient::for_version`
            // makes below: db:sync gates the gRPC replication leg, db:query
            // gates everything else (Hrana/HTTP1.1).
            //
            // `require_permission` takes the whole `AuthedOwner` and rejects a
            // non-`user` owner_type itself — a workspace/org-owned key's
            // owner_id names a workspace/org, not a user, and must never be
            // looked up as an `org_members.user_id`.
            let required = if parts.version == Version::HTTP_2 {
                Permission::DbSync
            } else {
                Permission::DbQuery
            };
            if let Err(status) =
                roles::require_permission(&state.pool, org_id, &owner, required).await
            {
                return status.into_response();
            }
            match routing::resolve_org_namespace(&state.pool, org_id).await {
                Ok(ns) => ns,
                Err(status) => return status.into_response(),
            }
        }
        Some(Err(())) => return StatusCode::BAD_REQUEST.into_response(),
        None => match routing::resolve_namespace(&state.pool, &owner).await {
            Ok(ns) => ns,
            Err(status) => return status.into_response(),
        },
    };

    let target = format!(
        "{}{}",
        state.sqld_url.trim_end_matches('/'),
        parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
    );
    let target: Uri = match target.parse() {
        Ok(uri) => uri,
        Err(e) => {
            tracing::error!("could not build sqld target URI from {target:?}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Strip the client's credentials (they authenticate to *us*, not to sqld)
    // and both namespace selectors, then set the selectors ourselves from the
    // namespace Postgres resolved for this owner. Never `unwrap` on the
    // header values: a malformed `sqld_namespace` row must produce a 500 for
    // this one request, not panic the worker and kill the connection.
    // `migrations/0002_namespace_format_constraint.sql` makes such a row
    // impossible to insert in the first place; this is the belt to its braces.
    let mut headers = forwardable_headers(
        &parts.headers,
        &[header::AUTHORIZATION, header::HOST, X_NAMESPACE_BIN, X_ORG_ID],
    );
    let namespace_bin = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&namespace);
    let Ok(namespace_bin_value) = HeaderValue::from_str(&namespace_bin) else {
        tracing::error!(
            "sqld_namespace {namespace:?} cannot be encoded into an x-namespace-bin \
             header; refusing to proxy"
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    headers.insert(X_NAMESPACE_BIN, namespace_bin_value);

    let (client, outbound_version) = state.client.for_version(parts.version);

    // One usage event per request that actually reached sqld — see
    // docs/superpowers/specs/2026-08-12-usage-metering-design.md. Defined
    // here, called from the two places below that qualify:
    //
    // * the normal path, once sqld's response head is in hand;
    // * the response-head timeout (`GATEWAY_TIMEOUT`), which *does* count —
    //   the connection to sqld was established and it spent the full
    //   `RESPONSE_HEAD_TIMEOUT` working on the request, consuming real
    //   database-side resources.
    //
    // Everything that returns *before* this point does not emit:
    // auth/permission/namespace-resolution rejections and malformed-header
    // failures never reached sqld at all. Neither does the transport-level
    // failure arm (`BAD_GATEWAY`) — deliberately, not by omission: a hyper
    // error before any response is ambiguous between "connection refused, sqld
    // never saw it" and "reached sqld, then the connection broke", and the
    // design spec doesn't make that call, so it stays unmetered.
    //
    // `duration_ms` measures request entry (after `auth_middleware`) to
    // response *head*, not full transfer — on the `sync` path, where the
    // streaming gRPC body is the long-lived part, it says very little about
    // how long the request really took.
    //
    // Note also that `usage` events ride the same subscriber and level as
    // ordinary logs: a deployment that raises the level floor above `INFO`
    // (via `RUST_LOG`, see `src/main.rs`) silently stops metering.
    let emit_usage_event = |status: StatusCode| {
        let protocol = if outbound_version == Version::HTTP_2 {
            "sync"
        } else {
            "query"
        };
        let org_id_field = org_id_for_usage
            .map(|id| id.to_string())
            .unwrap_or_default();
        tracing::info!(
            target: "usage",
            owner_type = %owner.owner_type,
            owner_id = %owner.owner_id,
            org_id = %org_id_field,
            namespace = %namespace,
            protocol,
            status = status.as_u16() as u64,
            duration_ms = start.elapsed().as_millis() as u64,
            "usage event"
        );
    };

    // `Host` only means anything on the Hrana/HTTP endpoints, which are
    // HTTP/1.1 only in practice — and RFC 9113 §8.3.1 treats an HTTP/2
    // request carrying both `:authority` (derived from `target` below) and a
    // `Host` with a different value as malformed. sqld/hyper tolerate the
    // mismatch today, but a stricter intermediary in front of sqld later
    // would not, so it's simplest to just never send it on the h2 leg.
    if outbound_version == Version::HTTP_11 {
        let namespace_host = format!("{namespace}.local");
        let Ok(host_value) = HeaderValue::from_str(&namespace_host) else {
            tracing::error!(
                "sqld_namespace {namespace:?} cannot be encoded into a Host header; \
                 refusing to proxy"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        headers.insert(header::HOST, host_value);
    }

    // RFC 9113 §8.2.2 permits `te: trailers` on HTTP/2 even though `te` is
    // otherwise hop-by-hop — some gRPC servers (grpc-go) reject a request
    // that omits it. `forwardable_headers` already stripped it above; put it
    // back, but only when the client actually sent it and we're speaking h2
    // outbound (sqld's own gRPC service doesn't check for it, but another
    // gRPC upstream might).
    if outbound_version == Version::HTTP_2
        && let Some(te) = parts.headers.get(header::TE)
        && te.as_bytes().eq_ignore_ascii_case(b"trailers")
    {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }

    // The body is moved through untouched — no `to_bytes` buffering in either
    // direction, so an arbitrarily large upload or a long-lived streaming
    // gRPC response costs the gateway a constant amount of memory.
    let mut outbound = hyper::Request::builder()
        .method(parts.method)
        .uri(target)
        .version(outbound_version);
    match outbound.headers_mut() {
        Some(slot) => *slot = headers,
        None => {
            tracing::error!("could not build outbound request to sqld");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let outbound = match outbound.body(body) {
        Ok(req) => req,
        Err(e) => {
            tracing::error!("could not build outbound request to sqld: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let upstream = match tokio::time::timeout(RESPONSE_HEAD_TIMEOUT, client.request(outbound)).await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            tracing::error!("proxy request to sqld failed: {e}");
            return StatusCode::BAD_GATEWAY.into_response();
        }
        Err(_) => {
            tracing::error!(
                "proxy request to sqld timed out after {RESPONSE_HEAD_TIMEOUT:?} \
                 waiting for a response"
            );
            emit_usage_event(StatusCode::GATEWAY_TIMEOUT);
            return StatusCode::GATEWAY_TIMEOUT.into_response();
        }
    };

    let (upstream_parts, upstream_body) = upstream.into_parts();
    // `content-length`/`transfer-encoding` describe upstream's framing of the
    // body on *its* connection. We re-frame it on ours, so hyper recomputes
    // them; copying upstream's across desynchronises the response.
    let response_headers = forwardable_headers(
        &upstream_parts.headers,
        &[header::CONTENT_LENGTH, header::TRANSFER_ENCODING],
    );

    emit_usage_event(upstream_parts.status);

    // `Body::new` keeps the upstream body as a stream *and* passes its trailer
    // frame through — gRPC carries its `grpc-status`/`grpc-message` in HTTP/2
    // trailers on a successful call, so dropping them would turn every synced
    // RPC into a hang or a protocol error.
    let mut response = Response::new(Body::new(upstream_body));
    *response.status_mut() = upstream_parts.status;
    *response.headers_mut() = response_headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        let mut src = HeaderMap::new();
        src.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        src.insert(header::TE, HeaderValue::from_static("trailers"));
        src.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        src.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
        src.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));

        let out = forwardable_headers(&src, &[]);

        assert!(out.get(header::CONNECTION).is_none());
        assert!(out.get(header::TE).is_none());
        assert!(out.get(header::TRANSFER_ENCODING).is_none());
        assert!(out.get(header::UPGRADE).is_none());
        assert_eq!(out.get(header::CONTENT_TYPE).unwrap(), "text/plain");
    }

    #[test]
    fn headers_named_by_connection_are_dropped() {
        let mut src = HeaderMap::new();
        src.insert(
            header::CONNECTION,
            HeaderValue::from_static("x-custom-hop, keep-alive"),
        );
        src.insert("x-custom-hop", HeaderValue::from_static("secret"));
        src.insert("x-kept", HeaderValue::from_static("yes"));

        let out = forwardable_headers(&src, &[]);

        assert!(out.get("x-custom-hop").is_none());
        assert_eq!(out.get("x-kept").unwrap(), "yes");
    }

    /// `insert` would keep only the last value; a client sending two cookies
    /// must have both forwarded.
    #[test]
    fn repeated_headers_all_survive() {
        let mut src = HeaderMap::new();
        src.append(header::COOKIE, HeaderValue::from_static("a=1"));
        src.append(header::COOKIE, HeaderValue::from_static("b=2"));

        let out = forwardable_headers(&src, &[]);

        let values: Vec<_> = out
            .get_all(header::COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(values, vec!["a=1", "b=2"]);
    }

    #[test]
    fn explicitly_dropped_headers_are_removed() {
        let mut src = HeaderMap::new();
        src.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        src.insert(header::HOST, HeaderValue::from_static("evil.local"));
        src.insert(X_NAMESPACE_BIN, HeaderValue::from_static("dmljdGltbnM"));

        let out = forwardable_headers(
            &src,
            &[header::AUTHORIZATION, header::HOST, X_NAMESPACE_BIN],
        );

        assert!(out.is_empty());
    }

    /// Namespace names must round-trip as sqld's gRPC binary metadata, which
    /// is base64 *without* padding — sqld rejects the padded form with
    /// "Invalid namespace bytes: `Invalid padding`" (verified live).
    #[test]
    fn namespace_is_encoded_as_unpadded_base64() {
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode("probens");
        assert_eq!(encoded, "cHJvYmVucw");
        assert!(!encoded.contains('='));
    }
}
