//! `/stack/*` reverse proxy — ADD ADR-07, SDD §5.8.1.
//!
//! > Operator single-URL topology: field node proxies the stack … One URL/PWA origin, one token
//! > prompt, works in asymmetric VPN topologies (STK-19); direct connection kept as optimization.
//!
//! The operator's browser only ever talks to the field node. `/stack/api/system/health` on this
//! node is `/api/system/health` on the stacking server, method, query, headers and body intact.
//!
//! # What this does not do yet
//!
//! WebSocket upgrades. SDD §5.8.1 says they are proxied too, and M1-T14 implements it; until
//! then an upgrade request through here is refused explicitly rather than being silently
//! downgraded to a plain HTTP call that would hang the browser waiting for a handshake.
//!
//! # Auth
//!
//! The `Authorization` header is forwarded verbatim (ADR-07 "auth forwarded"). That works because
//! PRD §8.1/§8.2 give both nodes the same `auth_token_env` and SDD §4.5 calls it "the shared
//! token": the credential the operator presents to the field node is the one the stack node
//! expects. There is no key in `stacking_server` for a *separate* stack credential, so a
//! deployment with two different tokens cannot be expressed — see the M0-T05 result note.

use std::time::Duration;

use astroctl_core::config::StackingServerConfig;
use astroctl_core::error::{ApiError, ErrorCode};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::uri::{Authority, PathAndQuery, Scheme};
use axum::http::{header, HeaderMap, HeaderName, Uri};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::api::{ApiFailure, AppState};

/// Prefix stripped from the incoming path before it is forwarded.
pub const PREFIX: &str = "/stack";

/// How long a proxied call may take before the field node gives up on the stack node.
///
/// Not configurable: PRD §8.1 has no key for it, and inventing one would be a silent schema
/// extension (tasks/README rule 2). 30 s is above any health or statistics call and far below
/// the point where an operator concludes the UI has hung — a *frame upload* does not come
/// through here, it goes to the stack node directly from the transfer agent (SDD §5.10).
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Headers that describe one hop of a connection and must never be forwarded across it
/// (RFC 9110 §7.6.1). `Host` is not in that list but is rewritten below anyway: the upstream
/// authority is the stack node, not whatever name the operator typed at the field node.
/// Lowercase names, not `HeaderName` constants: `HeaderName` is not permitted in a `const` slice
/// (it holds a `Bytes`, which has interior mutability), and `HeaderMap::remove` takes `&str`
/// anyway.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The client half of ADR-07.
#[derive(Debug, Clone)]
pub struct StackProxy {
    client: Client<HttpConnector, Body>,
    /// `None` when `stacking_server.enabled` is false — the routes still exist (so auth behaves
    /// identically) but answer 409 instead of dialling a host the operator turned off.
    upstream: Option<Authority>,
}

impl StackProxy {
    /// Build the proxy from `stacking_server` (PRD §8.1).
    #[must_use]
    pub fn new(config: &StackingServerConfig) -> Self {
        let upstream = config.enabled.then(|| authority(&config.host, config.port));
        Self {
            // Plain HTTP: the two nodes talk over the VPN, which is the encrypted layer
            // (PRD §7, ADD §5.5). No connector TLS, no certificate management on a Pi.
            client: Client::builder(TokioExecutor::new()).build_http(),
            upstream,
        }
    }

    /// `http://host:port`, for `/api/system/info`.
    #[must_use]
    pub fn upstream(&self) -> Option<String> {
        self.upstream.as_ref().map(|a| format!("http://{a}"))
    }

    /// Forward one request.
    async fn forward(&self, request: Request) -> Result<Response, ApiError> {
        let Some(upstream) = self.upstream.clone() else {
            return Err(ApiError::new(
                ErrorCode::NotConnected,
                "the stacking server is disabled on this node (`stacking_server.enabled: false`)",
            ));
        };

        if request.headers().contains_key(header::UPGRADE) {
            return Err(ApiError::new(
                ErrorCode::Unsupported,
                "WebSocket proxying through /stack/* is not implemented yet (M1-T14); \
                 connect to the stacking server directly until then",
            ));
        }

        let (mut parts, body) = request.into_parts();
        let target = rewrite_uri(&parts.uri, &upstream)?;
        let path_for_log = target.path().to_owned();
        parts.uri = target;
        parts.headers = forwarded_headers(&parts.headers, &upstream);
        // Extensions are this process's own request-scoped state (`RouteMeta`, `MatchedPath`, the
        // connection info); none of it means anything to the upstream node.
        parts.extensions.clear();

        let response = tokio::time::timeout(
            UPSTREAM_TIMEOUT,
            self.client.request(Request::from_parts(parts, body)),
        )
        .await
        .map_err(|_| {
            ApiError::new(
                ErrorCode::DeviceTimeout,
                format!(
                    "the stacking server at {upstream} did not answer within {} s",
                    UPSTREAM_TIMEOUT.as_secs()
                ),
            )
        })?
        .map_err(|error| {
            ApiError::new(
                ErrorCode::DeviceTransport,
                format!("cannot reach the stacking server at {upstream}: {error}"),
            )
        })?;

        tracing::debug!(
            upstream = %upstream,
            path = %path_for_log,
            status = response.status().as_u16(),
            "proxied to the stacking server"
        );

        let (mut parts, body) = response.into_parts();
        parts.headers = strip_hop_by_hop(&parts.headers);
        Ok(Response::from_parts(parts, Body::new(body)))
    }
}

/// The `/stack/*` handler.
pub async fn handler(State(state): State<AppState>, request: Request) -> Response {
    match state.proxy.forward(request).await {
        Ok(response) => response,
        Err(err) => ApiFailure(err).into_response(),
    }
}

fn authority(host: &str, port: u16) -> Authority {
    // An IPv6 literal must be bracketed in an authority; `stacking_server.host` is validated to
    // be a bare host or IP (no scheme, no path) but may well be `::1`.
    let text = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    // The config validator (SDD §4.4) already rejected whitespace, schemes and slashes, and the
    // port is a `u16`, so this cannot fail. Falling back to loopback rather than panicking keeps
    // a config-validation bug from taking the node down: the proxy answers 502, the rest runs.
    Authority::from_maybe_shared(text.clone()).unwrap_or_else(|_| {
        tracing::error!(authority = %text, "stacking_server host/port is not a valid authority");
        Authority::from_static("127.0.0.1:0")
    })
}

/// `/stack/api/system/health?x=1` → `http://<upstream>/api/system/health?x=1`.
fn rewrite_uri(original: &Uri, upstream: &Authority) -> Result<Uri, ApiError> {
    let path_and_query = original.path_and_query().map_or("/", PathAndQuery::as_str);
    let rest = path_and_query
        .strip_prefix(PREFIX)
        .unwrap_or(path_and_query);
    // `/stack` alone leaves nothing, and `/stack?q=1` leaves a bare query: both address the
    // upstream root.
    let rest = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        format!("/{rest}")
    };

    Uri::builder()
        .scheme(Scheme::HTTP)
        .authority(upstream.clone())
        .path_and_query(rest.clone())
        .build()
        .map_err(|error| {
            ApiError::new(
                ErrorCode::Validation,
                format!("cannot proxy `{rest}` to the stacking server: {error}"),
            )
        })
}

/// Request headers to send upstream: everything the client sent except the hop-by-hop set, with
/// `Host` rewritten to the upstream authority.
///
/// `Authorization` is deliberately *not* stripped — forwarding it is the point (ADR-07).
fn forwarded_headers(incoming: &HeaderMap, upstream: &Authority) -> HeaderMap {
    let mut headers = strip_hop_by_hop(incoming);
    headers.remove(header::HOST);
    if let Ok(value) = upstream.as_str().parse() {
        headers.insert(header::HOST, value);
    }
    headers
}

fn strip_hop_by_hop(incoming: &HeaderMap) -> HeaderMap {
    let mut headers = incoming.clone();
    // Everything named by `Connection` is itself hop-by-hop (RFC 9110 §7.6.1), so read the list
    // before removing the header that carries it.
    let listed: Vec<HeaderName> = incoming
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|token| HeaderName::try_from(token.trim()).ok())
        .collect();
    for name in &listed {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn upstream() -> Authority {
        Authority::from_static("192.168.1.100:8471")
    }

    #[test]
    fn the_stack_prefix_is_stripped_and_the_rest_survives_intact() {
        let cases = [
            ("/stack/api/system/health", "/api/system/health"),
            ("/stack/api/ingest", "/api/ingest"),
            (
                "/stack/api/stacking/stats?session=2026-07-29_m31",
                "/api/stacking/stats?session=2026-07-29_m31",
            ),
            // Percent-encoding must survive untouched — decoding and re-encoding is how a proxy
            // corrupts a session id with a slash in it.
            (
                "/stack/api/sessions/2026-07-29_m31%2Fa",
                "/api/sessions/2026-07-29_m31%2Fa",
            ),
            // The prefix itself, with and without a query.
            ("/stack", "/"),
            ("/stack/", "/"),
            ("/stack?deep=1", "/?deep=1"),
        ];
        for (from, expected_path_and_query) in cases {
            let rewritten =
                rewrite_uri(&from.parse().expect("test uri"), &upstream()).expect("rewrites");
            assert_eq!(rewritten.scheme_str(), Some("http"), "for {from}");
            assert_eq!(rewritten.authority(), Some(&upstream()), "for {from}");
            assert_eq!(
                rewritten
                    .path_and_query()
                    .map(PathAndQuery::as_str)
                    .unwrap_or_default(),
                expected_path_and_query,
                "for {from}"
            );
        }
    }

    #[test]
    fn the_operators_credential_is_what_reaches_the_stack_node() {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer s3cret"),
        );
        incoming.insert(header::HOST, HeaderValue::from_static("field.vpn:8470"));
        incoming.insert(header::ACCEPT, HeaderValue::from_static("application/json"));

        let forwarded = forwarded_headers(&incoming, &upstream());
        assert_eq!(
            forwarded
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer s3cret"),
            "ADR-07: auth is forwarded, not re-issued"
        );
        assert_eq!(
            forwarded.get(header::HOST).and_then(|v| v.to_str().ok()),
            Some("192.168.1.100:8471"),
            "Host names the upstream, not this node"
        );
        assert_eq!(forwarded.get(header::ACCEPT), incoming.get(header::ACCEPT));
    }

    #[test]
    fn hop_by_hop_headers_do_not_cross_the_proxy() {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, X-Hop"),
        );
        incoming.insert(
            HeaderName::from_static("keep-alive"),
            HeaderValue::from_static("timeout=5"),
        );
        incoming.insert(
            HeaderName::from_static("x-hop"),
            HeaderValue::from_static("1"),
        );
        incoming.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        incoming.insert(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_static("Basic x"),
        );
        incoming.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let forwarded = forwarded_headers(&incoming, &upstream());
        for gone in [
            header::CONNECTION.as_str(),
            "keep-alive",
            // Named by `Connection`, therefore hop-by-hop even though it is not in the fixed list.
            "x-hop",
            header::TRANSFER_ENCODING.as_str(),
            header::PROXY_AUTHORIZATION.as_str(),
        ] {
            assert!(
                !forwarded.contains_key(gone),
                "`{gone}` must not cross the proxy"
            );
        }
        assert!(forwarded.contains_key(header::CONTENT_TYPE));
    }

    #[test]
    fn an_ipv6_upstream_is_bracketed() {
        assert_eq!(authority("::1", 8471).as_str(), "[::1]:8471");
        assert_eq!(
            authority("192.168.1.100", 8471).as_str(),
            "192.168.1.100:8471"
        );
        assert_eq!(authority("stack.vpn", 8471).as_str(), "stack.vpn:8471");
    }

    // --- end to end, over a real socket ------------------------------------------------

    mod end_to_end {
        use super::*;
        use crate::api;
        use crate::test_support::{state_with, TestNode};
        use axum::extract::Request as AxumRequest;
        use axum::http::{HeaderValue, StatusCode};
        use axum::routing::any;
        use serde_json::{json, Value};
        use std::sync::Arc;
        use tower::ServiceExt as _;

        /// Stand-in for the stacking server: answers every path by echoing what it received, so
        /// the assertions are about what actually crossed the socket rather than about what this
        /// process intended to send.
        async fn start_echo_upstream() -> u16 {
            async fn echo(request: AxumRequest) -> axum::Json<Value> {
                axum::Json(json!({
                    "path": request.uri().path(),
                    "query": request.uri().query(),
                    "method": request.method().as_str(),
                    "authorization": request
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok()),
                    "host": request
                        .headers()
                        .get(header::HOST)
                        .and_then(|v| v.to_str().ok()),
                }))
            }

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("an ephemeral port is available");
            let port = listener.local_addr().expect("bound address").port();
            let app = axum::Router::new()
                .route("/{*rest}", any(echo))
                .route("/", any(echo));
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            port
        }

        async fn call(node: &TestNode, path: &str) -> (StatusCode, Value) {
            let (router, declarations) = api::router();
            let state = state_with(node, declarations).await;
            let auth = Arc::clone(&state.auth);
            let app = api::with_auth(router.with_state(state), auth);

            let response = app
                .oneshot(
                    AxumRequest::builder()
                        .uri(path)
                        .header(
                            header::AUTHORIZATION,
                            HeaderValue::from_static("Bearer s3cret"),
                        )
                        .body(Body::empty())
                        .expect("request builds"),
                )
                .await
                .expect("router responds");

            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("body reads");
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }

        /// The acceptance criterion, automated: the field node's `/stack/api/system/health` is
        /// the stack node's `/api/system/health`, with the operator's credential attached.
        #[tokio::test]
        async fn the_proxy_reaches_the_stack_node_with_the_operators_token() {
            let port = start_echo_upstream().await;
            let node = TestNode::authenticated("s3cret").with_stack_upstream("127.0.0.1", port);

            let (status, body) = call(&node, "/stack/api/system/health?verbose=1").await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["path"], "/api/system/health");
            assert_eq!(body["query"], "verbose=1");
            assert_eq!(body["method"], "GET");
            assert_eq!(body["authorization"], "Bearer s3cret");
            assert_eq!(body["host"], format!("127.0.0.1:{port}"));
        }

        #[tokio::test]
        async fn a_disabled_stacking_server_is_refused_not_dialled() {
            let node = TestNode::authenticated("s3cret").with_stack_disabled();
            let (status, body) = call(&node, "/stack/api/system/health").await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body["code"], "NOT_CONNECTED");
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("disabled")),
                "{body}"
            );
        }

        /// An unreachable stack node must produce the error envelope, not a bare hyper failure —
        /// the PWA switches on `code`, and "the other node is down" is a normal operating state
        /// (SDD §5.10.1).
        #[tokio::test]
        async fn an_unreachable_stack_node_answers_the_error_envelope() {
            // Port 1 on loopback: nothing is listening and the connection is refused at once.
            let node = TestNode::authenticated("s3cret").with_stack_upstream("127.0.0.1", 1);
            let (status, body) = call(&node, "/stack/api/system/health").await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(body["code"], "DEVICE_TRANSPORT");
            assert_eq!(body["retryable"], true);
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("127.0.0.1:1")),
                "the message must name the upstream: {body}"
            );
        }

        /// M1-T14 proxies WebSockets. Until it does, an upgrade must be refused explicitly rather
        /// than silently answered as a plain request the browser will wait forever on.
        #[tokio::test]
        async fn a_websocket_upgrade_is_refused_explicitly_until_m1_t14() {
            let port = start_echo_upstream().await;
            let node = TestNode::authenticated("s3cret").with_stack_upstream("127.0.0.1", port);

            let (router, declarations) = api::router();
            let state = state_with(&node, declarations).await;
            let auth = Arc::clone(&state.auth);
            let app = api::with_auth(router.with_state(state), auth);

            let response = app
                .oneshot(
                    AxumRequest::builder()
                        .uri("/stack/ws/preview")
                        .header(
                            header::AUTHORIZATION,
                            HeaderValue::from_static("Bearer s3cret"),
                        )
                        .header(header::CONNECTION, "Upgrade")
                        .header(header::UPGRADE, "websocket")
                        .body(Body::empty())
                        .expect("request builds"),
                )
                .await
                .expect("router responds");

            assert_eq!(response.status(), StatusCode::CONFLICT);
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("body reads");
            let body: Value = serde_json::from_slice(&bytes).expect("json");
            assert_eq!(body["code"], "UNSUPPORTED");
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("M1-T14")),
                "{body}"
            );
        }
    }
}
