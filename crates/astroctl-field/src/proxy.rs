//! `/stack/*` reverse proxy — ADD ADR-07, SDD §5.8.1.
//!
//! > Operator single-URL topology: field node proxies the stack … One URL/PWA origin, one token
//! > prompt, works in asymmetric VPN topologies (STK-19); direct connection kept as optimization.
//!
//! The operator's browser only ever talks to the field node. `/stack/api/system/health` on this
//! node is `/api/system/health` on the stacking server, method, query, headers and body intact.
//!
//! # Auth
//!
//! The `Authorization` header is forwarded verbatim (ADR-07 "auth forwarded"). That works because
//! PRD §8.1/§8.2 give both nodes the same `auth_token_env` and SDD §4.5 calls it "the shared
//! token": the credential the operator presents to the field node is the one the stack node
//! expects. There is no key in `stacking_server` for a *separate* stack credential, so a
//! deployment with two different tokens cannot be expressed — see the M0-T05 result note.
//!
//! # WebSocket upgrades (M1-T14)
//!
//! §5.8.1: "WS upgrades proxied too, so the operator keeps a single origin". They are, and the
//! interesting part is how the credential crosses the hop, because **the browser cannot attach
//! one**. That is the same constraint §4.5 invented tickets for, and the answer is the same shape
//! in two halves:
//!
//! ```text
//!   browser  --  ws://field/stack/ws/preview?ticket=<field ticket>  -->  field node
//!                                                                          |  consume the
//!                                                                          |  ticket (§4.5)
//!   field node  --  ws://stack/ws/preview  + Authorization: Bearer  -->  stack node
//! ```
//!
//! The **inbound** half is authenticated exactly as `/ws` and `/ws/liveview` are: a single-use
//! ticket in the query string, spent here. The **outbound** half is an ordinary bearer header,
//! because the field node is a program and can set one. §4.5 says this in as many words: "The
//! field node connecting to the stack node's WebSocket (M1-T14's preview proxy) is not a browser
//! and uses the ordinary `Authorization` header; it has no need of a ticket."
//!
//! Two consequences worth stating, because both are easy to get wrong:
//!
//! * **The ticket does not cross.** It is stripped from the forwarded query, so the stacking
//!   server never sees a credential it cannot check and none of them reach *its* access log —
//!   which is the entire point of §4.5, and would be defeated by forwarding the query verbatim.
//! * **The token comes from this node's own configuration**, not from the request, because there
//!   is nothing in the request to take it from. That is the one place this proxy mints a
//!   credential rather than relaying one, and it is sound only because §4.5's shared token makes
//!   the field node's credential *the same credential* the stack node expects.
//!
//! # Why the upgrade is a byte tunnel and not a re-framed WebSocket
//!
//! Once the handshake is done this copies bytes in both directions and never parses a frame. A
//! proxy that decoded and re-encoded RFC 6455 would impose its own frame- and message-size limits
//! on a socket whose whole payload is 200 KB JPEGs, would need its own ping policy, and could
//! silently alter the `ACLV` envelope both nodes agree on. A tunnel cannot: what the stack node
//! wrote is what the browser reads.
//!
//! It also means the connection is deliberately *not* pooled. `hyper_util`'s legacy client keeps
//! connections for reuse, and an upgraded connection is not reusable by definition — so the
//! upgrade path dials its own `TcpStream` and hands it to `hyper::client::conn::http1`, whose
//! `with_upgrades` is what lets the 101 escape the HTTP state machine intact.

use std::time::Duration;

use astroctl_core::config::StackingServerConfig;
use astroctl_core::error::{ApiError, ErrorCode};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::uri::{Authority, PathAndQuery, Scheme};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::api::{ApiFailure, AppState};
use crate::auth::unauthorized;
use crate::ticket::TicketRejection;

/// Prefix stripped from the incoming path before it is forwarded.
pub const PREFIX: &str = "/stack";

/// How long a proxied call may take before the field node gives up on the stack node.
///
/// Not configurable: PRD §8.1 has no key for it, and inventing one would be a silent schema
/// extension (tasks/README rule 2). 30 s is above any health or statistics call and far below
/// the point where an operator concludes the UI has hung — a *frame upload* does not come
/// through here, it goes to the stack node directly from the transfer agent (SDD §5.10).
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on a polled JSON body ([`StackProxy::get_json`]).
///
/// `/api/system/health` and `/api/stacking/stats` are a few hundred bytes; 1 MiB is far above
/// either and stops a misconfigured upstream — one answering with a frame, say — from being read
/// into the field node's memory every poll.
const MAX_POLL_BODY_BYTES: usize = 1 << 20;

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
    /// `Authorization: Bearer …`, pre-rendered, for the outbound half of a WebSocket upgrade.
    ///
    /// Held because the browser cannot send one on an upgrade (§4.5) and there is therefore
    /// nothing in the request to forward — see the module docs. `None` on a node running under
    /// §4.5's loopback exception, where the stack node is equally unauthenticated. Plain HTTP
    /// requests never read this: they forward the operator's own header, which is ADR-07's rule
    /// and keeps the credential the operator presented the one that is checked.
    ///
    /// Pre-rendered and marked sensitive, exactly as the transfer agent's uploader holds the same
    /// credential (SDD §5.10): a sensitive `HeaderValue` prints as `Sensitive` rather than as the
    /// token, so the `Debug` on this struct — which is inside `AppState` — cannot leak it.
    authorization: Option<HeaderValue>,
}

impl StackProxy {
    /// Build the proxy from `stacking_server` (PRD §8.1) and this node's token.
    #[must_use]
    pub fn new(config: &StackingServerConfig, token: Option<&str>) -> Self {
        let upstream = config.enabled.then(|| authority(&config.host, config.port));
        Self {
            // Plain HTTP: the two nodes talk over the VPN, which is the encrypted layer
            // (PRD §7, ADD §5.5). No connector TLS, no certificate management on a Pi.
            client: Client::builder(TokioExecutor::new()).build_http(),
            upstream,
            authorization: token.and_then(|token| {
                let mut value = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
                value.set_sensitive(true);
                Some(value)
            }),
        }
    }

    /// `http://host:port`, for `/api/system/info`.
    #[must_use]
    pub fn upstream(&self) -> Option<String> {
        self.upstream.as_ref().map(|a| format!("http://{a}"))
    }

    /// The configured upstream authority, for the `stack.status` poller.
    #[must_use]
    pub fn authority(&self) -> Option<&Authority> {
        self.upstream.as_ref()
    }

    /// `GET <upstream><path>` with this node's own credential, decoded as JSON.
    ///
    /// The `stack.status` republisher's one call (SDD §4.3, USB-06). It goes through this struct
    /// rather than building a second HTTP client so there is one place that knows the upstream
    /// authority, one connection pool, and one answer to "which credential do the two nodes
    /// share" — and so a deployment that changes `stacking_server.host` cannot end up with a
    /// proxy pointing one way and a poller the other.
    ///
    /// `timeout` is the caller's because the two uses want different ones: an operator waiting on
    /// a proxied request will wait 30 s, and a health poll that took 30 s would leave the panel
    /// claiming the stack node is fine for half a minute after it stopped answering.
    ///
    /// # Errors
    /// [`ApiError`] with `NOT_CONNECTED` when the stacking server is disabled, `NODE_UNREACHABLE`
    /// when it does not answer, and the upstream's own status when it answers with one.
    pub async fn get_json(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiError> {
        let Some(upstream) = self.upstream.clone() else {
            return Err(ApiError::new(
                ErrorCode::NotConnected,
                "the stacking server is disabled on this node (`stacking_server.enabled: false`)",
            ));
        };

        let uri = Uri::builder()
            .scheme(Scheme::HTTP)
            .authority(upstream.clone())
            .path_and_query(path)
            .build()
            .map_err(|error| {
                ApiError::new(
                    ErrorCode::Validation,
                    format!("cannot address `{path}`: {error}"),
                )
            })?;

        let mut request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(uri)
            .body(Body::empty())
            .map_err(|error| {
                ApiError::new(
                    ErrorCode::Validation,
                    format!("cannot build the request: {error}"),
                )
            })?;
        if let Some(value) = self.authorization.clone() {
            request.headers_mut().insert(header::AUTHORIZATION, value);
        }

        let response = tokio::time::timeout(timeout, self.client.request(request))
            .await
            .map_err(|_| {
                unreachable_stack(
                    &upstream,
                    &format!("no answer within {} s", timeout.as_secs()),
                )
            })?
            .map_err(|error| unreachable_stack(&upstream, &error.to_string()))?;

        let status = response.status();
        let bytes = axum::body::to_bytes(Body::new(response.into_body()), MAX_POLL_BODY_BYTES)
            .await
            .map_err(|error| unreachable_stack(&upstream, &error.to_string()))?;

        if !status.is_success() {
            // The stack node answered, so it is *reachable* — a 401 here means the two nodes'
            // tokens disagree, which is a deployment fault the operator must be told about in
            // those words rather than as "unreachable".
            return Err(ApiError::new(
                ErrorCode::NodeUnreachable,
                format!("the stacking server answered {status} for {path}"),
            ));
        }

        serde_json::from_slice(&bytes).map_err(|error| {
            ApiError::new(
                ErrorCode::DeviceProtocol,
                format!("the stacking server's {path} is not JSON: {error}"),
            )
        })
    }

    /// Forward one request.
    async fn forward(&self, request: Request) -> Result<Response, ApiError> {
        let Some(upstream) = self.upstream.clone() else {
            return Err(ApiError::new(
                ErrorCode::NotConnected,
                "the stacking server is disabled on this node (`stacking_server.enabled: false`)",
            ));
        };

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
            // `NODE_UNREACHABLE`, not `DEVICE_TRANSPORT`. §4.2 draws the distinction explicitly —
            // "conflating them tells the operator to check a cable when the problem is a tunnel"
            // — and this proxy was the conflation. Both are 502 and retryable, so nothing about
            // the sender's behaviour changes; what changes is what the panel says.
            ApiError::new(
                ErrorCode::NodeUnreachable,
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

    /// Tunnel one WebSocket upgrade to the stacking server.
    ///
    /// The inbound ticket has already been spent by [`handler`]; this half is about reaching the
    /// stack node and getting the 101 back to the browser byte for byte.
    async fn upgrade(&self, request: Request) -> Result<Response, ApiError> {
        let Some(upstream) = self.upstream.clone() else {
            return Err(ApiError::new(
                ErrorCode::NotConnected,
                "the stacking server is disabled on this node (`stacking_server.enabled: false`)",
            ));
        };

        let (mut parts, _body) = request.into_parts();
        // Taken *out* of the request: this is the browser's half of the connection, and it
        // resolves only once this handler has answered 101.
        let client_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
        let Some(client_upgrade) = client_upgrade else {
            // Reachable when something replayed an upgrade-looking request through a path that
            // cannot actually upgrade — a test harness, or a middleware that rebuilt the request.
            return Err(ApiError::new(
                ErrorCode::Validation,
                "this request carries WebSocket upgrade headers but cannot be upgraded",
            ));
        };

        let target = rewrite_uri(&parts.uri, &upstream)?;
        let path_for_log = target.path().to_owned();

        // The upgrade headers themselves must cross — `Connection`, `Upgrade`,
        // `Sec-WebSocket-Key` and friends are hop-by-hop *and* are the handshake, so the plain
        // path's blanket strip is exactly wrong here. What is rebuilt rather than copied is the
        // authority and the credential.
        let mut headers = parts.headers.clone();
        headers.remove(header::HOST);
        if let Ok(value) = upstream.as_str().parse() {
            headers.insert(header::HOST, value);
        }
        match self.authorization.clone() {
            Some(value) => {
                headers.insert(header::AUTHORIZATION, value);
            }
            // No token configured: this node is under §4.5's loopback exception and the stack
            // node it dials is too. Removing rather than leaving whatever arrived means the
            // upstream sees the same posture the operator's own request had.
            None => {
                headers.remove(header::AUTHORIZATION);
            }
        }

        let mut upstream_request = hyper::Request::builder()
            .method(parts.method.clone())
            .uri(target)
            .body(Body::empty())
            .map_err(|error| {
                ApiError::new(
                    ErrorCode::Validation,
                    format!("cannot build the upstream upgrade request: {error}"),
                )
            })?;
        *upstream_request.headers_mut() = headers;

        // A dedicated connection, not the pooled client. An upgraded connection is single-use by
        // definition, and `with_upgrades` is what lets the 101 leave hyper's HTTP state machine
        // with the socket still attached.
        let stream = tokio::time::timeout(
            UPSTREAM_TIMEOUT,
            tokio::net::TcpStream::connect(upstream.as_str()),
        )
        .await
        .map_err(|_| unreachable_stack(&upstream, "the connection attempt timed out"))?
        .map_err(|error| unreachable_stack(&upstream, &error.to_string()))?;

        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|error| unreachable_stack(&upstream, &error.to_string()))?;
        tokio::spawn(async move {
            // `with_upgrades` keeps this future driving the connection *through* the upgrade.
            // Without it the 101 arrives and the socket is dropped underneath it.
            if let Err(error) = connection.with_upgrades().await {
                tracing::debug!(%error, "the proxied stack connection ended");
            }
        });

        let response =
            tokio::time::timeout(UPSTREAM_TIMEOUT, sender.send_request(upstream_request))
                .await
                .map_err(|_| unreachable_stack(&upstream, "the upgrade handshake timed out"))?
                .map_err(|error| unreachable_stack(&upstream, &error.to_string()))?;

        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            // The stack node refused — most usefully a 401, which on this path means the two
            // nodes' tokens disagree. Passed through as-is rather than translated: the browser
            // gets the upstream's own answer, exactly as it would on the plain HTTP path.
            let (mut parts, body) = response.into_parts();
            parts.headers = strip_hop_by_hop(&parts.headers);
            tracing::warn!(
                upstream = %upstream,
                path = %path_for_log,
                status = parts.status.as_u16(),
                "the stacking server refused a proxied WebSocket upgrade"
            );
            return Ok(Response::from_parts(parts, Body::new(body)));
        }

        // Both halves are now committed. Copying starts once *this* node's 101 has reached the
        // browser, which is what `client_upgrade` resolving means.
        let (mut parts, _) = response.into_parts();
        let upstream_upgrade =
            hyper::upgrade::on(hyper::Response::from_parts(parts.clone(), Body::empty()));
        tokio::spawn(async move {
            tunnel(client_upgrade, upstream_upgrade, path_for_log).await;
        });

        // The upstream's own 101 and its handshake headers — `Sec-WebSocket-Accept` is computed
        // from the browser's key, so it must be the stack node's answer and not one invented
        // here. Hop-by-hop stripping is skipped for the same reason it was on the way out.
        parts.extensions.clear();
        Ok(Response::from_parts(parts, Body::empty()))
    }
}

/// Copy bytes both ways until either side closes.
async fn tunnel(
    client: hyper::upgrade::OnUpgrade,
    upstream: impl std::future::Future<Output = Result<hyper::upgrade::Upgraded, hyper::Error>>,
    path: String,
) {
    let (client, upstream) = tokio::join!(client, upstream);
    let (Ok(client), Ok(upstream)) = (client, upstream) else {
        tracing::debug!(%path, "a proxied WebSocket upgrade did not complete on both sides");
        return;
    };

    let mut client = TokioIo::new(client);
    let mut upstream = TokioIo::new(upstream);
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((to_upstream, to_client)) => tracing::debug!(
            %path,
            to_upstream,
            to_client,
            "a proxied WebSocket closed"
        ),
        // Normal: a browser that navigates away resets rather than closing cleanly, and on this
        // socket that is the *usual* ending. Not a warning.
        Err(error) => tracing::debug!(%path, %error, "a proxied WebSocket ended abruptly"),
    }
}

fn unreachable_stack(upstream: &Authority, detail: &str) -> ApiError {
    ApiError::new(
        ErrorCode::NodeUnreachable,
        format!("cannot reach the stacking server at {upstream}: {detail}"),
    )
}

/// Path prefix of the stacking server's WebSocket routes, as the operator addresses them.
///
/// `/stack/ws/preview` and `/stack/ws`, per SDD §5.11.1's route table.
pub const WS_PREFIX: &str = "/stack/ws/";

/// The `/stack/*` handler, for both of the routers it is mounted in.
///
/// One handler, two authentications, decided by the path — which is decided by which router the
/// request came through (`api::router` is behind the bearer layer, `api::ws_router` is not):
///
/// | Path | Checked by | Then |
/// |---|---|---|
/// | `/stack/ws/…` | a single-use ticket, here | tunnel the upgrade |
/// | anything else under `/stack` | the bearer layer, before this | forward, or tunnel |
///
/// The branch lives here rather than inside [`StackProxy`] because the ticket is a field-node
/// concern: `AppState` owns the store, and the proxy has no business knowing how this node
/// authenticates browsers.
pub async fn handler(State(state): State<AppState>, request: Request) -> Response {
    let ticketed = request.uri().path().starts_with(WS_PREFIX);

    if ticketed {
        // §4.5, inbound half. The same single-use ticket `/ws` and `/ws/liveview` require, spent
        // the same way — a ticket buys exactly one upgrade, so a PWA opening the field's liveview
        // and the stack's preview socket fetches two.
        //
        // Required for *every* request on this prefix, not only for ones carrying upgrade
        // headers. This subtree is deliberately outside the bearer layer, so a plain GET here
        // would otherwise be an unauthenticated request that makes the field node dial the
        // stacking server on a stranger's behalf.
        if let Err(rejection) = state.tickets.consume(ticket_of(request.uri())) {
            // Logged, never returned: telling a caller which of "unknown", "expired" and "already
            // spent" their guess was is an oracle for guessing the next one.
            tracing::info!(reason = rejection.reason(), "stack ws upgrade refused");
            return unauthorized(&ApiError::new(ErrorCode::Auth, TicketRejection::MESSAGE));
        }
        if !is_websocket_upgrade(request.headers()) {
            return ApiFailure(ApiError::new(
                ErrorCode::Validation,
                "this is a WebSocket route; open it with a WebSocket upgrade",
            ))
            .into_response();
        }
    }

    let outcome = if is_websocket_upgrade(request.headers()) {
        state.proxy.upgrade(request).await
    } else {
        state.proxy.forward(request).await
    };

    match outcome {
        Ok(response) => response,
        Err(err) => ApiFailure(err).into_response(),
    }
}

/// Whether this request is an RFC 6455 upgrade.
///
/// Both headers, and `Upgrade` compared case-insensitively: RFC 9110 makes the token
/// case-insensitive and browsers do not all agree on the spelling. A request carrying only
/// `Connection: Upgrade` is not one — treating it as such would send a plain GET down the tunnel
/// path and hang the caller waiting for a handshake that never comes.
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The `?ticket=` parameter, if the query carries one.
///
/// Hand-parsed rather than via an extractor because this handler sees the raw request on its way
/// to being forwarded, and it must also be able to *remove* the parameter (see [`rewrite_uri`]).
fn ticket_of(uri: &Uri) -> Option<&str> {
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("ticket="))
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
///
/// A `ticket` parameter is dropped. It is this node's credential, minted by this node's store and
/// meaningless to the stacking server — and §4.5 exists precisely to keep credentials out of URLs
/// that get logged, so forwarding one into the *other* node's access log would defeat the point
/// one hop later.
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
    let rest = strip_ticket(&rest);

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

/// Remove a `ticket` parameter from `path?query`, leaving everything else in place and in order.
///
/// String surgery rather than a query-string library: the rest of the query must survive byte for
/// byte (see the percent-encoding case in the tests), and re-serializing parsed pairs is exactly
/// how a proxy corrupts a value it did not understand.
fn strip_ticket(path_and_query: &str) -> String {
    let Some((path, query)) = path_and_query.split_once('?') else {
        return path_and_query.to_owned();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !(*pair == "ticket" || pair.starts_with("ticket=")))
        .collect();
    if kept.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
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
            // §4.5: the ticket is this node's credential and must not reach the stacking
            // server's access log. Dropped, and the rest of the query left in its own order.
            ("/stack/ws/preview?ticket=deadbeef", "/ws/preview"),
            ("/stack/ws/preview?ticket=deadbeef&x=1", "/ws/preview?x=1"),
            ("/stack/ws/preview?x=1&ticket=deadbeef", "/ws/preview?x=1"),
            (
                "/stack/ws/preview?x=1&ticket=deadbeef&y=2",
                "/ws/preview?x=1&y=2",
            ),
            // A parameter that merely starts with the same letters is not the ticket.
            ("/stack/api/x?ticketed=1", "/api/x?ticketed=1"),
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
            let app = api::with_auth(api::with_state(router, state), auth);

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
            // `NODE_UNREACHABLE`, not `DEVICE_TRANSPORT`: §4.2 draws the distinction in the row
            // itself — conflating them tells the operator to check a cable when the problem is a
            // tunnel. The status and the retryable flag are unchanged, so nothing downstream of
            // the envelope behaves differently; only the words the panel chooses do.
            assert_eq!(body["code"], "NODE_UNREACHABLE");
            assert_eq!(body["retryable"], true);
            assert!(
                body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("127.0.0.1:1")),
                "the message must name the upstream: {body}"
            );
        }

        /// An upgrade whose ticket is missing, spent or expired never reaches the stacking
        /// server. The inbound half of the hop is authenticated exactly as `/ws` is (§4.5), and
        /// the answer says only "invalid or expired" — telling a caller which of the three their
        /// guess was is an oracle for guessing the next one.
        #[tokio::test]
        async fn an_upgrade_without_a_valid_ticket_is_refused_before_the_stack_is_dialled() {
            let port = start_echo_upstream().await;
            let node = TestNode::authenticated("s3cret").with_stack_upstream("127.0.0.1", port);

            let (router, declarations) = api::router();
            let state = state_with(&node, declarations).await;
            let auth = Arc::clone(&state.auth);
            let app = api::with_auth(api::with_state(router, state.clone()), auth);

            for uri in ["/stack/ws/preview", "/stack/ws/preview?ticket=never-issued"] {
                let response = app
                    .clone()
                    .oneshot(
                        AxumRequest::builder()
                            .uri(uri)
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

                assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "for {uri}");
                let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .expect("body reads");
                let body: Value = serde_json::from_slice(&bytes).expect("json");
                assert_eq!(body["code"], "AUTH");
            }
        }

        /// A ticket buys exactly one upgrade (§4.5's single-use rule), and that holds across the
        /// proxy too — otherwise a ticket captured from a URL would be replayable against the
        /// stacking server for its whole TTL.
        #[tokio::test]
        async fn a_ticket_is_spent_by_the_upgrade_it_authenticates() {
            let node = TestNode::authenticated("s3cret").with_stack_upstream("127.0.0.1", 1);
            let (router, declarations) = api::router();
            let state = state_with(&node, declarations).await;

            let ticket = state.tickets.issue().expect("issues").as_str().to_owned();
            assert_eq!(state.tickets.len(), 1);

            let auth = Arc::clone(&state.auth);
            let app = api::with_auth(api::with_state(router, state.clone()), auth);
            let response = app
                .oneshot(
                    AxumRequest::builder()
                        .uri(format!("/stack/ws/preview?ticket={ticket}"))
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

            // 422, not 502: `oneshot` drives a `Service` and never gives the connection back, so
            // the request carries no `OnUpgrade` extension and the handler refuses it before it
            // dials. That is the branch this assertion is pinning — an upgrade-shaped request on
            // a path that cannot upgrade must be refused, not tunnelled into a hang. What matters
            // here is the line after it: the ticket was accepted and spent on the way through.
            // The real tunnel is exercised over a bound port in `mod upgrades`.
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(
                state.tickets.len(),
                0,
                "a valid ticket is consumed by the upgrade it authenticates"
            );
        }

        /// Only a real RFC 6455 upgrade takes the tunnel. A `Connection: Upgrade` with no
        /// `Upgrade: websocket` is an ordinary request, and sending it down the tunnel path would
        /// demand a ticket for a plain GET.
        #[test]
        fn an_upgrade_is_recognised_by_both_headers_case_insensitively() {
            let mut headers = HeaderMap::new();
            assert!(!is_websocket_upgrade(&headers));

            headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
            assert!(
                !is_websocket_upgrade(&headers),
                "`Connection: Upgrade` alone is not a WebSocket"
            );

            headers.insert(header::UPGRADE, HeaderValue::from_static("WebSocket"));
            assert!(
                is_websocket_upgrade(&headers),
                "the token is case-insensitive"
            );

            headers.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
            assert!(
                !is_websocket_upgrade(&headers),
                "a different protocol is not ours"
            );
        }
    }

    /// The WebSocket tunnel, over two real sockets.
    ///
    /// A `oneshot`-driven `Service` cannot complete an upgrade, so the acceptance criterion —
    /// "WS proxying survives stack restart", and before that "it works at all" — is only testable
    /// against a bound port with a real RFC 6455 client on one end and a real server on the other.
    /// The upstream here is an axum node standing in for the stacking server, so what is asserted
    /// is what actually crossed two hops rather than what this module intended to send.
    mod upgrades {
        use super::*;
        use crate::api;
        use crate::test_support::{state_with, TestNode};
        use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
        use axum::extract::Request as AxumRequest;
        use axum::response::Response as AxumResponse;
        use axum::routing::get;
        use futures_util::{SinkExt, StreamExt};
        use std::net::SocketAddr;
        use tokio_tungstenite::tungstenite::Message as ClientMessage;

        type Socket = tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >;

        /// A stand-in stacking server: reports the credential and query it saw, then echoes.
        /// The assertions are therefore about what crossed the hop, not about intent.
        async fn start_stack_node(token: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
            async fn socket(
                headers: axum::http::HeaderMap,
                uri: Uri,
                upgrade: WebSocketUpgrade,
            ) -> AxumResponse {
                let seen = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("<none>")
                    .to_owned();
                let query = uri.query().unwrap_or("<none>").to_owned();
                upgrade.on_upgrade(move |mut ws: WebSocket| async move {
                    let _ = ws
                        .send(Message::Text(format!("auth={seen} query={query}").into()))
                        .await;
                    while let Some(Ok(message)) = ws.recv().await {
                        match message {
                            Message::Text(text) => {
                                let reply = Message::Text(format!("echo:{text}").into());
                                if ws.send(reply).await.is_err() {
                                    break;
                                }
                            }
                            Message::Binary(bytes) => {
                                if ws.send(Message::Binary(bytes)).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                })
            }

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("an ephemeral port is available");
            let port = listener.local_addr().expect("bound address").port();
            let guard = axum::middleware::from_fn(
                move |request: AxumRequest, next: axum::middleware::Next| async move {
                    let presented = request
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    if presented.as_deref() == Some(token) {
                        next.run(request).await
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                },
            );
            let app = axum::Router::new()
                .route("/ws/preview", get(socket))
                .layer(guard);
            let handle = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            (port, handle)
        }

        /// A field node in front of a given upstream port, on its own bound port.
        async fn start_field_node(upstream_port: u16) -> (SocketAddr, AppState) {
            let node =
                TestNode::authenticated("s3cret").with_stack_upstream("127.0.0.1", upstream_port);
            // The whole node, not just the bearer-authenticated half: `/stack/ws/*` lives in
            // `ws_router`, and a test that assembled only `router()` would answer 401 to the very
            // upgrade it is meant to be exercising — which is exactly what it did.
            let (router, declarations) = api::router();
            let (ws_router, ws_declarations) = api::ws_router();
            let state = state_with(
                &node,
                declarations.into_iter().chain(ws_declarations).collect(),
            )
            .await;
            let app = crate::assemble(router, ws_router, state.clone());

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("an ephemeral port is available");
            let addr = listener.local_addr().expect("bound address");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            (addr, state)
        }

        async fn open(addr: SocketAddr, ticket: &str) -> Result<Socket, String> {
            let url = format!("ws://{addr}/stack/ws/preview?ticket={ticket}");
            match tokio_tungstenite::connect_async(url).await {
                Ok((socket, _)) => Ok(socket),
                Err(error) => Err(error.to_string()),
            }
        }

        async fn text(socket: &mut Socket) -> String {
            loop {
                let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .expect("a frame arrives")
                    .expect("the socket is open")
                    .expect("the frame is readable");
                match message {
                    ClientMessage::Text(text) => return text.to_string(),
                    ClientMessage::Ping(_) | ClientMessage::Pong(_) => {}
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        }

        /// The acceptance criterion, automated: the browser opens a socket on the *field* origin
        /// with a ticket, and a WebSocket to the stacking server comes out the other side —
        /// carrying the bearer credential the browser could not have attached, and not carrying
        /// the ticket.
        #[tokio::test]
        async fn a_browser_ticket_becomes_a_bearer_upgrade_on_the_stack_node() {
            let (upstream_port, _upstream) = start_stack_node("Bearer s3cret").await;
            let (addr, state) = start_field_node(upstream_port).await;
            let ticket = state.tickets.issue().expect("issues").as_str().to_owned();

            let mut socket = open(addr, &ticket).await.expect("the tunnel opens");
            let seen = text(&mut socket).await;

            assert!(
                seen.contains("auth=Bearer s3cret"),
                "the field node presents its own credential upstream: {seen}"
            );
            assert!(
                !seen.contains("ticket"),
                "SDD 4.5: the ticket must not reach the stacking server's log: {seen}"
            );
        }

        /// The tunnel is bidirectional and byte-transparent — binary payloads cross unaltered,
        /// which is what keeps the `ACLV` envelope the two nodes agree on intact.
        #[tokio::test]
        async fn frames_cross_the_tunnel_in_both_directions_unaltered() {
            let (upstream_port, _upstream) = start_stack_node("Bearer s3cret").await;
            let (addr, state) = start_field_node(upstream_port).await;
            let ticket = state.tickets.issue().expect("issues").as_str().to_owned();

            let mut socket = open(addr, &ticket).await.expect("the tunnel opens");
            let _greeting = text(&mut socket).await;

            socket
                .send(ClientMessage::Text("hello".into()))
                .await
                .expect("the client can write through the tunnel");
            assert_eq!(text(&mut socket).await, "echo:hello");

            // A binary frame carrying the envelope's magic, since that is the real payload.
            let payload = b"ACLV\x01\x01\x00\x02{}jpeg".to_vec();
            socket
                .send(ClientMessage::Binary(payload.clone().into()))
                .await
                .expect("binary crosses too");
            let echoed = loop {
                let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .expect("a frame arrives")
                    .expect("open")
                    .expect("readable");
                if let ClientMessage::Binary(bytes) = message {
                    break bytes.to_vec();
                }
            };
            assert_eq!(
                echoed, payload,
                "the tunnel must not re-frame or alter bytes"
            );
        }

        /// REL-10 through the proxy: the stacking server can go away and come back, and a
        /// reconnect succeeds without restarting the field node or reloading the PWA.
        ///
        /// What this does *not* assert is the fate of the socket that was already open. Killing
        /// this fixture means aborting its `axum::serve` task, which frees the listener but leaves
        /// per-connection tasks it already spawned running — so the old tunnel stays up here for
        /// reasons that have nothing to do with the proxy. That half is proved against a real
        /// process kill in the live evidence, where the kernel closes the socket and the tunnel
        /// propagates it. What is asserted here is the part a fixture can honestly show: while the
        /// stack node is down an upgrade **fails with an answer instead of hanging**, and once it
        /// is back a fresh ticket opens a fresh tunnel through the same, still-running field node.
        #[tokio::test]
        async fn a_tunnel_reconnects_through_the_proxy_after_the_stack_node_restarts() {
            let (upstream_port, upstream) = start_stack_node("Bearer s3cret").await;
            let (addr, state) = start_field_node(upstream_port).await;

            let ticket = state.tickets.issue().expect("issues").as_str().to_owned();
            let mut socket = open(addr, &ticket).await.expect("the tunnel opens");
            let _greeting = text(&mut socket).await;

            // The stacking server goes away, freeing its port.
            upstream.abort();
            let _ = upstream.await;

            // A browser reconnecting while it is down gets a refusal, not a stall. This is the
            // difference between a panel that says "stack unreachable" and one that spins.
            let ticket = state.tickets.issue().expect("issues").as_str().to_owned();
            let refused = tokio::time::timeout(Duration::from_secs(5), open(addr, &ticket))
                .await
                .expect("the attempt must not hang");
            assert!(
                refused.is_err(),
                "a tunnel to a node that is not there must fail"
            );

            // It comes back on the same port — a restart, as far as the field node can tell.
            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{upstream_port}"))
                .await
                .expect("the port is free again");
            let app = axum::Router::new().route(
                "/ws/preview",
                get(|upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(|mut ws: WebSocket| async move {
                        let _ = ws.send(Message::Text("back".into())).await;
                    })
                }),
            );
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            // The same field node, never restarted, tunnels again.
            let ticket = state.tickets.issue().expect("issues").as_str().to_owned();
            let mut reopened = open(addr, &ticket).await.expect("the tunnel reopens");
            assert_eq!(text(&mut reopened).await, "back");
        }

        /// The stacking server's own refusal reaches the browser as itself. A 401 here means the
        /// two nodes' tokens disagree, and translating it into "unreachable" would send the
        /// operator to look at a tunnel that is fine.
        #[tokio::test]
        async fn an_upstream_refusal_is_passed_through_rather_than_translated() {
            let (upstream_port, _upstream) = start_stack_node("Bearer a-different-token").await;
            let (addr, state) = start_field_node(upstream_port).await;
            let ticket = state.tickets.issue().expect("issues").as_str().to_owned();

            let error = open(addr, &ticket).await.expect_err("the upstream refuses");
            assert!(
                error.contains("401"),
                "the browser sees the stacking server's own answer: {error}"
            );
        }
    }
}
