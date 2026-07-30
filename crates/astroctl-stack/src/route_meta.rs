//! Route metadata as a typed layer — SDD §8.2, tiers per §5.8.1.
//!
//! > Every route registers `RouteMeta { tier: Tier, audit: bool }` via a typed layer. Phase 1
//! > uses it for audit logging only; Phase 2c's confirmation middleware and the LLM tool
//! > generator (ADD §6.1) consume the same declarations — the invariant "one declaration drives
//! > both" is established now.
//!
//! The invariant is enforced by construction rather than by convention: [`ApiRouter`] keeps its
//! [`axum::Router`] private, so the only way to add a route is through a constructor such as
//! [`ApiRouter::get`], and every one of them takes a [`RouteMeta`]. There is no code path that
//! mounts a handler without declaring its tier, which is the difference between an invariant and
//! a comment.
//!
//! Each declaration does three things, all from the one value:
//!
//! 1. inserts [`RouteMeta`] into the request extensions, where Phase 2c's confirmation middleware
//!    will read it before the handler runs;
//! 2. emits the audit record on the way out, when `audit` is set (this milestone's only use);
//! 3. lands in [`ApiRouter::declarations`], which `/api/system/info` publishes.
//!
//! The field node's `Tier::confirmation_mode` has no counterpart here: PRD §8.2 gives the
//! stacking server no
//! `llm` section, because the LLM control layer talks to the field node's API (ADR-10). The tier
//! vocabulary is still declared in full — it is one contract shared by both nodes, and a route
//! that moves between them must not change tier on the way.

use std::time::Instant;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::handler::Handler;
use axum::middleware::{from_fn_with_state, Next};
use axum::response::Response;
use axum::routing::MethodRouter;
use axum::Router;
use serde::Serialize;

/// Consequence class of a route — the `Tier` column of the SDD §5.8.1 route table.
///
/// The same vocabulary as the field node's, deliberately: SDD §8.2 is a cross-cutting mechanism,
/// and a tier that meant different things on the two nodes would be worse than no tier at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Observes state, changes nothing.
    Read,
    /// A state change with no motion and no consequence worth confirming.
    Low,
    /// Motion or capture: `goto`, `capture`.
    Medium,
    /// High consequence: `park`, `unpark`.
    High,
    /// Reachable by the operator, never by the LLM (`/api/mount/estop`, SDD §5.8.1).
    BlockedForLlm,
    /// Proxied verbatim between nodes; the tier that governs the action is declared by the route
    /// that finally serves it (ADR-07). Declared here for vocabulary parity — the stacking
    /// server proxies nothing.
    PassThrough,
}

impl Tier {
    /// The whole vocabulary, in ascending order of consequence.
    ///
    /// Published by `/api/system/info` so the PWA reads the tier list from the running node
    /// rather than from a copy of the operator's YAML.
    pub const ALL: [Tier; 6] = [
        Tier::Read,
        Tier::Low,
        Tier::Medium,
        Tier::High,
        Tier::BlockedForLlm,
        Tier::PassThrough,
    ];

    /// Whether the LLM tool generator (ADD §6.1) may expose this route as a tool.
    #[must_use]
    pub const fn llm_callable(self) -> bool {
        !matches!(self, Self::BlockedForLlm | Self::PassThrough)
    }

    /// The wire spelling, identical to the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::BlockedForLlm => "blocked_for_llm",
            Self::PassThrough => "pass_through",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a route declares about itself (SDD §8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RouteMeta {
    /// Consequence class.
    pub tier: Tier,
    /// Whether every call is written to the audit log.
    pub audit: bool,
}

impl RouteMeta {
    /// A route with an explicit tier and audit decision.
    #[must_use]
    pub const fn new(tier: Tier, audit: bool) -> Self {
        Self { tier, audit }
    }

    /// A read-only route. Not audited: `mount.position` at 1 Hz would drown the audit log in
    /// records of nothing having happened, and the state changes are all on other tiers.
    #[must_use]
    pub const fn read() -> Self {
        Self::new(Tier::Read, false)
    }
}

/// One row of the route table, as published by `/api/system/info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RouteDecl {
    /// Path pattern as registered with axum, e.g. `/stack/{*rest}`.
    pub path: &'static str,
    /// HTTP method.
    pub method: &'static str,
    /// The declaration itself.
    #[serde(flatten)]
    pub meta: RouteMeta,
}

/// A router that cannot mount a route without a [`RouteMeta`].
///
/// The wrapped [`Router`] is private on purpose — see the module docs.
pub struct ApiRouter<S> {
    router: Router<S>,
    declarations: Vec<RouteDecl>,
}

impl<S> ApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// An empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            declarations: Vec::new(),
        }
    }

    /// Declare a `GET` route.
    #[must_use]
    pub fn get<H, T>(self, path: &'static str, meta: RouteMeta, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.declare(path, "GET", meta, axum::routing::get(handler))
    }

    /// Declare a `POST` route that accepts a body larger than axum's 2 MiB default.
    ///
    /// The limit is a parameter rather than a constant here because raising it is a per-route
    /// decision: `/api/ingest` carries a raw frame and needs 512 MiB (SDD §5.11.2), and the
    /// default exists precisely so that nothing else on the node inherits that. Without the
    /// layer the first real 25 MB upload comes back as a 413 that no code in SDD §4.2 describes.
    ///
    /// There is no plain `post`: the only POST this node serves in M1 is ingest, and an unbuilt
    /// constructor is dead code, not a seam.
    #[must_use]
    pub fn post_with_body_limit<H, T>(
        self,
        path: &'static str,
        meta: RouteMeta,
        max_body_bytes: usize,
        handler: H,
    ) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.declare(
            path,
            "POST",
            meta,
            axum::routing::post(handler).layer(DefaultBodyLimit::max(max_body_bytes)),
        )
    }

    fn declare(
        mut self,
        path: &'static str,
        method: &'static str,
        meta: RouteMeta,
        method_router: MethodRouter<S>,
    ) -> Self {
        self.declarations.push(RouteDecl { path, method, meta });
        self.router = self
            .router
            .route(path, method_router.layer(from_fn_with_state(meta, apply)));
        self
    }

    /// The declared route table. Cheap to clone into application state.
    #[must_use]
    pub fn declarations(&self) -> Vec<RouteDecl> {
        self.declarations.clone()
    }

    /// Hand back the assembled router.
    pub fn into_router(self) -> Router<S> {
        self.router
    }
}

impl<S> Default for ApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// The typed layer: publish the declaration inward, audit outward.
async fn apply(State(meta): State<RouteMeta>, mut request: Request, next: Next) -> Response {
    // Phase 2c's confirmation middleware and any handler that wants to know its own tier read it
    // from here rather than re-deriving it from the path.
    request.extensions_mut().insert(meta);

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let response = next.run(request).await;

    if meta.audit {
        // A separate target so an operator can route the audit trail to its own file without
        // taking the rest of the node's logging with it.
        tracing::info!(
            target: "astroctl::audit",
            tier = %meta.tier,
            method = %method,
            path = %path,
            status = response.status().as_u16(),
            latency_ms = started.elapsed().as_millis() as u64,
            "route call"
        );
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use tower::ServiceExt as _;

    async fn echo_tier(request: Request) -> String {
        request
            .extensions()
            .get::<RouteMeta>()
            .map_or_else(|| "<undeclared>".to_owned(), |meta| meta.tier.to_string())
    }

    #[tokio::test]
    async fn the_declaration_reaches_the_handler_as_a_typed_extension() {
        let api = ApiRouter::<()>::new()
            .get("/read", RouteMeta::read(), echo_tier)
            .get("/park", RouteMeta::new(Tier::High, true), echo_tier);
        let router = api.into_router();

        for (path, expected) in [("/read", "read"), ("/park", "high")] {
            let response = router
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request builds"),
                )
                .await
                .expect("router responds");
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads");
            assert_eq!(String::from_utf8_lossy(&body), expected);
        }
    }

    #[test]
    fn declarations_are_recorded_in_registration_order() {
        let api = ApiRouter::<()>::new()
            .get("/read", RouteMeta::read(), echo_tier)
            .get("/api/ingest", RouteMeta::new(Tier::Low, true), echo_tier);

        assert_eq!(
            api.declarations(),
            vec![
                RouteDecl {
                    path: "/read",
                    method: "GET",
                    meta: RouteMeta::read()
                },
                RouteDecl {
                    path: "/api/ingest",
                    method: "GET",
                    meta: RouteMeta::new(Tier::Low, true)
                },
            ]
        );
    }

    /// The tier vocabulary is a shared contract, so its wire spellings and its LLM-callability
    /// must be the same statement on both nodes.
    #[test]
    fn the_tier_vocabulary_matches_the_field_nodes() {
        assert_eq!(
            Tier::ALL.map(Tier::as_str),
            [
                "read",
                "low",
                "medium",
                "high",
                "blocked_for_llm",
                "pass_through"
            ]
        );
        assert!(Tier::Read.llm_callable());
        assert!(Tier::High.llm_callable());
        assert!(!Tier::BlockedForLlm.llm_callable());
        assert!(!Tier::PassThrough.llm_callable());
    }

    #[test]
    fn tier_wire_spelling_matches_serde() {
        for tier in [
            Tier::Read,
            Tier::Low,
            Tier::Medium,
            Tier::High,
            Tier::BlockedForLlm,
            Tier::PassThrough,
        ] {
            assert_eq!(
                serde_json::to_value(tier).expect("serializes"),
                serde_json::Value::String(tier.as_str().to_owned())
            );
            assert_eq!(tier.to_string(), tier.as_str());
        }
    }

    #[test]
    fn route_decl_flattens_the_meta_into_one_object() {
        let decl = RouteDecl {
            path: "/api/system/health",
            method: "GET",
            meta: RouteMeta::read(),
        };
        assert_eq!(
            serde_json::to_value(decl).expect("serializes"),
            serde_json::json!({
                "path": "/api/system/health",
                "method": "GET",
                "tier": "read",
                "audit": false
            })
        );
    }
}
