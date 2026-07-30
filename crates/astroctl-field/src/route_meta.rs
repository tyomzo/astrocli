//! Route metadata as a typed layer — SDD §8.2, tiers per §5.8.1.
//!
//! > Every route registers `RouteMeta { tier: Tier, audit: bool }` via a typed layer. Phase 1
//! > uses it for audit logging only; Phase 2c's confirmation middleware and the LLM tool
//! > generator (ADD §6.1) consume the same declarations — the invariant "one declaration drives
//! > both" is established now.
//!
//! The invariant is enforced by construction rather than by convention: [`ApiRouter`] keeps its
//! [`axum::Router`] private, so the only way to add a route is [`ApiRouter::get`] /
//! [`ApiRouter::any`], and both take a [`RouteMeta`]. There is no code path that mounts a handler
//! without declaring its tier, which is the difference between an invariant and a comment.
//!
//! Each declaration does four things, all from the one value:
//!
//! 1. inserts [`RouteMeta`] into the request extensions, where Phase 2c's confirmation middleware
//!    will read it before the handler runs;
//! 2. **enforces the command envelope** for its [`CommandClass`] — staleness and idempotency,
//!    SDD §5.8.1, M1-T10;
//! 3. emits the audit record on the way out, when `audit` is set;
//! 4. lands in [`ApiRouter::declarations`], which `/api/system/info` publishes and which the LLM
//!    tool generator will read to decide what it is allowed to call.
//!
//! # Why the envelope is enforced here (M1-T10)
//!
//! §5.8.1's staleness rule is a *safety* property with two halves that point in opposite
//! directions — a late start is refused, a late stop never is — so the thing that decides which
//! half a route gets must be impossible to get wrong on the eleventh route. Two consequences,
//! both of which this module already had the shape for:
//!
//! * **The class is declared beside the route**, in the same value as the tier, so it is visible
//!   in one table, published by `/api/system/info` with everything else, and reviewed in the
//!   same diff that adds the route.
//! * **A handler cannot see it, let alone set it.** The check runs in this layer, before the
//!   handler and before any body extractor; a handler that wanted to exempt itself would have to
//!   edit the route table, which is exactly where a reviewer is looking.

use std::sync::Arc;
use std::time::Instant;

use astroctl_core::config::{ConfirmationMode, ConfirmationTiers};
use astroctl_core::error::{ApiError, ErrorCode};
use axum::extract::{Request, State};
use axum::handler::Handler;
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use axum::Router;
use serde::Serialize;

use crate::command::{Admission, CommandLedger};

/// Consequence class of a route — the `Tier` column of the SDD §5.8.1 route table.
///
/// Phase 2c maps these to [`ConfirmationMode`] via the operator's `llm.confirmation_tiers`
/// configuration; Phase 1 only records them.
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
    /// Proxied verbatim to the other node; the tier that governs the action is declared by the
    /// route that finally serves it (ADR-07).
    PassThrough,
}

impl Tier {
    /// The whole vocabulary, in ascending order of consequence.
    ///
    /// Published by `/api/system/info` so the PWA and the LLM tool generator read the tier list
    /// and its confirmation policy from the running node rather than from a copy of the
    /// operator's YAML.
    pub const ALL: [Tier; 6] = [
        Tier::Read,
        Tier::Low,
        Tier::Medium,
        Tier::High,
        Tier::BlockedForLlm,
        Tier::PassThrough,
    ];

    /// What the operator's configuration says should happen before an LLM-issued call on this
    /// tier executes (Phase 2c).
    ///
    /// `None` means "not an LLM-callable tier at all", which is a different statement from
    /// "callable, but confirm first" — hence the `Option` rather than defaulting to
    /// [`ConfirmationMode::ConfirmWarn`].
    #[must_use]
    pub const fn confirmation_mode(self, tiers: &ConfirmationTiers) -> Option<ConfirmationMode> {
        match self {
            Self::Read => Some(tiers.read),
            Self::Low => Some(tiers.low),
            Self::Medium => Some(tiers.medium),
            Self::High => Some(tiers.high),
            Self::BlockedForLlm | Self::PassThrough => None,
        }
    }

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

/// What the command envelope does to a route — SDD §5.8.1's staleness paragraph (M1-T10).
///
/// A *different* axis from [`Tier`], and deliberately not derived from it. Tier answers "how much
/// consequence does this carry, and should a human confirm it"; this answers "does refusing a
/// delayed one make the observatory safer or less safe". `/api/mount/slew/stop` is `low` tier and
/// a `Stopping` command; `/api/mount/park` is `high` tier and `MotionInitiating`. Collapsing them
/// would make one of the two wrong for every route where they disagree, which is most of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    /// Not a state-changing command: every `GET`, plus `POST /api/auth/ws-ticket`.
    ///
    /// The envelope is neither required nor read. The ticket route is here rather than under
    /// [`Neutral`] because its answer is a *nonce*: replaying a `command_id` would hand a second
    /// caller a ticket the first one already spent (§4.5 makes it single-use), so idempotency
    /// would turn "the reply was lost" and "the ticket was used" into the same observation on the
    /// one exchange a reconnect cannot afford to get wrong.
    NotACommand,

    /// A command whose classification belongs to the node that finally serves it — `/stack/*`
    /// (ADR-07), exactly as [`Tier::PassThrough`] defers the tier.
    ///
    /// The envelope headers are forwarded verbatim with the rest of the request, so the stack
    /// node applies its own rule to them. Classifying here would mean this node deciding
    /// staleness for a route it cannot see the semantics of.
    PassThrough,

    /// Starts something the world can then be doing while the operator's intent has moved on:
    /// goto, slew, park, capture, tracking.
    ///
    /// Refused when `issued_at` is older than `server.max_command_age_ms` — §5.8.1's
    /// `COMMAND_STALE`. Also idempotent.
    MotionInitiating,

    /// Stops something. **Never refused for any envelope reason**, at any age, with or without an
    /// envelope at all (§5.8.1: "a late stop is safe, a late start is not").
    ///
    /// Idempotent when an envelope is present, because a re-sent stop should return the answer
    /// the first one got — but the envelope is read opportunistically, never demanded. A stopping
    /// route that could answer 422 would be a stopping route that can fail to stop.
    Stopping,

    /// Changes state and starts nothing: connect, disconnect, settings, fault acknowledgement,
    /// live-view start/stop.
    ///
    /// Takes the envelope for idempotency and skips the age check. The age check exists so a
    /// delayed *intent* cannot act on the world after the operator moved on; these commands
    /// either describe a state that is still the state the operator wants (a settings value, a
    /// connection) or act only on the node itself.
    Neutral,

    /// `/api/mount/estop`, and nothing else — SDD §5.8.2.
    ///
    /// Exempt from every envelope requirement: no envelope needed, none read, nothing that could
    /// answer 422. This is a *declaration* rather than the absence of one, so that the exemption
    /// is a line a reviewer sees in the route table and a test can assert is unique. §5.8.2's
    /// "auth only, no JSON parsing" is about there being nothing at all between the bearer check
    /// and the driver call, and a header check is still something.
    Exempt,
}

impl CommandClass {
    /// The whole vocabulary, published by `/api/system/info` beside the tiers.
    pub const ALL: [Self; 6] = [
        Self::NotACommand,
        Self::PassThrough,
        Self::MotionInitiating,
        Self::Stopping,
        Self::Neutral,
        Self::Exempt,
    ];

    /// Whether a request on this route must carry an envelope to be served.
    ///
    /// `Stopping` is false and that is the asymmetry: it *uses* an envelope when one is there and
    /// refuses nothing when it is not.
    #[must_use]
    pub const fn requires_envelope(self) -> bool {
        matches!(self, Self::MotionInitiating | Self::Neutral)
    }

    /// Whether `issued_at` older than `max_command_age_ms` refuses the request.
    #[must_use]
    pub const fn checks_staleness(self) -> bool {
        matches!(self, Self::MotionInitiating)
    }

    /// The wire spelling, identical to the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotACommand => "not_a_command",
            Self::PassThrough => "pass_through",
            Self::MotionInitiating => "motion_initiating",
            Self::Stopping => "stopping",
            Self::Neutral => "neutral",
            Self::Exempt => "exempt",
        }
    }
}

impl std::fmt::Display for CommandClass {
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
    /// What the command envelope does here (SDD §5.8.1, M1-T10).
    pub command: CommandClass,
}

impl RouteMeta {
    /// A route with an explicit tier, audit decision and command class.
    ///
    /// Three arguments rather than a builder, and no defaulted `command`: a default is the
    /// mechanism by which route eleven silently escapes the envelope. Adding this parameter broke
    /// every existing declaration at compile time, which is the review pass this task is for.
    #[must_use]
    pub const fn new(tier: Tier, audit: bool, command: CommandClass) -> Self {
        Self {
            tier,
            audit,
            command,
        }
    }

    /// A read-only route. Not audited: `mount.position` at 1 Hz would drown the audit log in
    /// records of nothing having happened, and the state changes are all on other tiers.
    #[must_use]
    pub const fn read() -> Self {
        Self::new(Tier::Read, false, CommandClass::NotACommand)
    }

    /// An audited mutation that starts motion or an exposure (§5.8.1).
    #[must_use]
    pub const fn motion(tier: Tier) -> Self {
        Self::new(tier, true, CommandClass::MotionInitiating)
    }

    /// An audited mutation that stops something — never refused for an envelope reason.
    #[must_use]
    pub const fn stopping(tier: Tier) -> Self {
        Self::new(tier, true, CommandClass::Stopping)
    }

    /// An audited mutation that starts nothing.
    #[must_use]
    pub const fn neutral(tier: Tier) -> Self {
        Self::new(tier, true, CommandClass::Neutral)
    }
}

/// One row of the route table, as published by `/api/system/info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RouteDecl {
    /// Path pattern as registered with axum, e.g. `/stack/{*rest}`.
    pub path: &'static str,
    /// HTTP method, or `ANY` for a pass-through route.
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

    /// Declare a `POST` route.
    ///
    /// Every state-changing route in SDD §5.8.1 is a POST, so this arrived with M1-T03's mount
    /// rows. It is the same two lines as [`get`](Self::get) on purpose: the value of this type
    /// is that there is no way to mount a handler without declaring its tier, and a second
    /// spelling of "declare a route" would be a second place for that to be forgotten.
    #[must_use]
    pub fn post<H, T>(self, path: &'static str, meta: RouteMeta, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.declare(path, "POST", meta, axum::routing::post(handler))
    }

    /// Declare a `PUT` route.
    ///
    /// One row in SDD §5.8.1 uses it — `/api/camera/settings`, whose GET and PUT are the same
    /// path on two tiers — and it is a `PUT` rather than a `POST` because sending the settings
    /// twice must leave the camera where sending them once did. That is the whole distinction the
    /// method carries, and it is worth carrying on the one route where an operator's retry over a
    /// flaky tunnel is otherwise indistinguishable from a second change.
    #[must_use]
    pub fn put<H, T>(self, path: &'static str, meta: RouteMeta, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.declare(path, "PUT", meta, axum::routing::put(handler))
    }

    /// Declare a route serving every method — the `/stack/*` proxy (SDD §5.8.1).
    #[must_use]
    pub fn any<H, T>(self, path: &'static str, meta: RouteMeta, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.declare(path, "ANY", meta, axum::routing::any(handler))
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

/// The typed layer: publish the declaration inward, enforce the envelope, audit outward.
async fn apply(State(meta): State<RouteMeta>, mut request: Request, next: Next) -> Response {
    // Phase 2c's confirmation middleware and any handler that wants to know its own tier read it
    // from here rather than re-deriving it from the path.
    request.extensions_mut().insert(meta);

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();

    // One `now` for the staleness arithmetic and for the stamp on the way out, so a client
    // measuring its skew from the same response cannot be told two different times.
    let now = chrono::Utc::now();
    let mut response = match admit(meta, &request, now) {
        Ok(Admission::Proceed(None)) => next.run(request).await,
        Ok(Admission::Proceed(Some(reservation))) => {
            let response = next.run(request).await;
            reservation.settle(response).await
        }
        Ok(Admission::Replay(outcome)) => {
            tracing::debug!(target: "astroctl::audit", %path, "replaying a recorded outcome");
            outcome.into_response()
        }
        Ok(Admission::Refuse(error)) | Err(error) => crate::api::ApiFailure(error).into_response(),
    };

    crate::command::stamp_server_time(&mut response, now);

    if meta.audit {
        // A separate target so an operator can route the audit trail to its own file without
        // taking the rest of the node's logging with it.
        tracing::info!(
            target: "astroctl::audit",
            tier = %meta.tier,
            command = %meta.command,
            method = %method,
            path = %path,
            status = response.status().as_u16(),
            latency_ms = started.elapsed().as_millis() as u64,
            "route call"
        );
    }

    response
}

/// Ask the node's ledger what happens to this request (SDD §5.8.1).
///
/// # The `Err` arm is a bug in this binary, not in the request
///
/// The ledger reaches this layer as a request extension, because a `Router<S>` cannot hand its
/// own state to a layer built before that state exists — see [`crate::api::with_state`], which is
/// the single seam that installs it. If it is missing, the assembly is wrong, and the answer is
/// `500` rather than "carry on unchecked": a node that silently stopped enforcing staleness on
/// motion commands is precisely the failure §5.8.1 exists to prevent, and it would be invisible.
///
/// `Stopping` and the exempt classes are answered before the lookup, so a broken assembly still
/// cannot refuse a stop. That ordering is the one thing in this function that must not be
/// rearranged for tidiness.
fn admit(
    meta: RouteMeta,
    request: &Request,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Admission, ApiError> {
    if matches!(
        meta.command,
        CommandClass::NotACommand | CommandClass::PassThrough | CommandClass::Exempt
    ) {
        return Ok(Admission::Proceed(None));
    }

    let Some(ledger) = request.extensions().get::<Arc<CommandLedger>>() else {
        if meta.command == CommandClass::Stopping {
            return Ok(Admission::Proceed(None));
        }
        return Err(ApiError::new(
            ErrorCode::Internal,
            "the command ledger is not installed on this router; SDD §5.8.1's staleness and \
             idempotency rules are not being enforced",
        ));
    };

    Ok(ledger.admit(meta.command, request.headers(), now))
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
            // A `Stopping` route, so this test stays about the tier reaching the handler: a
            // covered class would need an envelope and a ledger, which the envelope tests below
            // supply and this one has no business needing.
            .get("/stop", RouteMeta::stopping(Tier::High), echo_tier);
        let router = api.into_router();

        for (path, expected) in [("/read", "read"), ("/stop", "high")] {
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
            .any(
                "/stack/{*rest}",
                RouteMeta::new(Tier::PassThrough, true, CommandClass::PassThrough),
                echo_tier,
            );

        assert_eq!(
            api.declarations(),
            vec![
                RouteDecl {
                    path: "/read",
                    method: "GET",
                    meta: RouteMeta::read()
                },
                RouteDecl {
                    path: "/stack/{*rest}",
                    method: "ANY",
                    meta: RouteMeta::new(Tier::PassThrough, true, CommandClass::PassThrough)
                },
            ]
        );
    }

    /// The §8.2 invariant, spelled out as an assertion: the tier a route declares is the same
    /// value Phase 2c's confirmation policy is looked up with. If the two ever came from
    /// different places this test would have nothing to assert.
    #[test]
    fn one_declaration_drives_both_consumers() {
        let tiers = ConfirmationTiers {
            read: ConfirmationMode::Auto,
            low: ConfirmationMode::Auto,
            medium: ConfirmationMode::Confirm,
            high: ConfirmationMode::ConfirmWarn,
        };

        assert_eq!(
            Tier::Read.confirmation_mode(&tiers),
            Some(ConfirmationMode::Auto)
        );
        assert_eq!(
            Tier::Medium.confirmation_mode(&tiers),
            Some(ConfirmationMode::Confirm)
        );
        assert_eq!(
            Tier::High.confirmation_mode(&tiers),
            Some(ConfirmationMode::ConfirmWarn)
        );
        // Not "confirm harder" — not callable at all.
        assert_eq!(Tier::BlockedForLlm.confirmation_mode(&tiers), None);
        assert!(!Tier::BlockedForLlm.llm_callable());
        assert_eq!(Tier::PassThrough.confirmation_mode(&tiers), None);
        assert!(!Tier::PassThrough.llm_callable());
        assert!(Tier::Read.llm_callable());
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
                "audit": false,
                "command": "not_a_command"
            })
        );
    }

    #[test]
    fn command_class_wire_spelling_matches_serde() {
        for class in CommandClass::ALL {
            assert_eq!(
                serde_json::to_value(class).expect("serializes"),
                serde_json::Value::String(class.as_str().to_owned())
            );
            assert_eq!(class.to_string(), class.as_str());
        }
    }

    /// The asymmetry of SDD §5.8.1, as a property of the vocabulary rather than of any one route.
    #[test]
    fn only_motion_initiating_checks_staleness_and_stopping_demands_nothing() {
        for class in CommandClass::ALL {
            assert_eq!(
                class.checks_staleness(),
                class == CommandClass::MotionInitiating,
                "{class}: staleness is the motion-initiating rule and nothing else's"
            );
        }
        assert!(
            !CommandClass::Stopping.requires_envelope(),
            "a stopping route that could answer 422 is a stopping route that can fail to stop"
        );
        assert!(!CommandClass::Exempt.requires_envelope());
        assert!(CommandClass::MotionInitiating.requires_envelope());
        assert!(CommandClass::Neutral.requires_envelope());
    }

    // --- the envelope layer, driven through a real router --------------------------------

    mod envelope {
        use super::*;
        use crate::command::{COMMAND_ID, ISSUED_AT, REPLAYED, SERVER_TIME};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        /// How many times a handler behind the layer actually ran.
        ///
        /// This is the "simulator got no command" clause of T-STALE-1, asserted at the layer that
        /// owns the decision: a refusal that reached the handler and was undone afterwards would
        /// pass a status-code test and fail the requirement. A counter *per test* rather than a
        /// static, because `cargo test` runs these in parallel on one process and a shared one
        /// would make each test's assertion depend on which others happened to be running.
        type Runs = Arc<AtomicUsize>;

        /// A handler bound to one test's counter.
        fn counting(runs: &Runs) -> impl Fn() -> BoxFuture + Clone + Send + 'static {
            let runs = Arc::clone(runs);
            move || {
                let runs = Arc::clone(&runs);
                Box::pin(async move {
                    let n = runs.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({ "run": n }))
                }) as BoxFuture
            }
        }

        type BoxFuture = std::pin::Pin<
            Box<dyn std::future::Future<Output = axum::Json<serde_json::Value>> + Send>,
        >;

        /// The four classes that behave differently, on one router.
        fn app(ledger: &Arc<CommandLedger>) -> (Router, Runs) {
            let runs: Runs = Arc::new(AtomicUsize::new(0));
            let router = ApiRouter::<()>::new()
                .post("/goto", RouteMeta::motion(Tier::Medium), counting(&runs))
                .post("/stop", RouteMeta::stopping(Tier::Low), counting(&runs))
                .post("/settings", RouteMeta::neutral(Tier::Low), counting(&runs))
                .post(
                    "/estop",
                    RouteMeta::new(Tier::BlockedForLlm, true, CommandClass::Exempt),
                    counting(&runs),
                )
                .into_router()
                .layer(axum::Extension(Arc::clone(ledger)))
                .with_state(());
            (router, runs)
        }

        fn ledger() -> Arc<CommandLedger> {
            Arc::new(CommandLedger::new(Duration::from_millis(2000)))
        }

        fn request(path: &str, envelope: Option<(&str, chrono::DateTime<chrono::Utc>)>) -> Request {
            let mut builder = HttpRequest::builder().method("POST").uri(path);
            if let Some((id, issued_at)) = envelope {
                builder = builder.header(COMMAND_ID, id).header(
                    ISSUED_AT,
                    issued_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                );
            }
            builder.body(Body::empty()).expect("request builds")
        }

        async fn body_of(response: Response) -> serde_json::Value {
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("body reads");
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        }

        /// T-STALE-1's first two clauses, through the layer: same age, two classes, and the
        /// refused one never reached the handler.
        #[tokio::test]
        async fn a_stale_start_is_refused_and_the_handler_never_runs_while_a_stale_stop_executes() {
            let ledger = ledger();
            let (router, runs) = app(&ledger);
            let five_seconds_ago = chrono::Utc::now() - chrono::Duration::seconds(5);

            let refused = router
                .clone()
                .oneshot(request(
                    "/goto",
                    Some(("old-goto-id-001", five_seconds_ago)),
                ))
                .await
                .expect("router responds");
            assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(body_of(refused).await["code"], "COMMAND_STALE");
            assert_eq!(
                runs.load(Ordering::SeqCst),
                0,
                "a stale goto must not reach the handler, let alone the mount"
            );

            let honoured = router
                .oneshot(request(
                    "/stop",
                    Some(("old-stop-id-001", five_seconds_ago)),
                ))
                .await
                .expect("router responds");
            assert_eq!(honoured.status(), StatusCode::OK);
            assert_eq!(runs.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn a_covered_route_without_an_envelope_names_the_header_it_wants() {
            let ledger = ledger();
            let (router, runs) = app(&ledger);
            for path in ["/goto", "/settings"] {
                let response = router
                    .clone()
                    .oneshot(request(path, None))
                    .await
                    .expect("router responds");
                assert_eq!(
                    response.status(),
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "{path}"
                );
                let body = body_of(response).await;
                assert_eq!(body["code"], "VALIDATION", "{path}");
                assert!(
                    body["message"]
                        .as_str()
                        .is_some_and(|m| m.contains(COMMAND_ID)),
                    "{path}: {body}"
                );
                assert_eq!(runs.load(Ordering::SeqCst), 0, "{path}");
            }
        }

        /// The e-stop, at the layer: no envelope, no body, no header — and the handler still runs.
        #[tokio::test]
        async fn the_exempt_route_runs_with_nothing_at_all_attached() {
            let ledger = ledger();
            let (router, runs) = app(&ledger);
            let response = router
                .oneshot(request("/estop", None))
                .await
                .expect("router responds");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(runs.load(Ordering::SeqCst), 1);
        }

        /// T-STALE-1's third clause: the duplicate returns the first answer and the handler ran
        /// exactly once.
        #[tokio::test]
        async fn a_duplicate_command_id_replays_the_first_answer_without_re_executing() {
            let ledger = ledger();
            let (router, runs) = app(&ledger);
            let now = chrono::Utc::now();

            let first = router
                .clone()
                .oneshot(request("/goto", Some(("repeated-id-0001", now))))
                .await
                .expect("router responds");
            assert_eq!(first.status(), StatusCode::OK);
            assert!(first.headers().get(REPLAYED).is_none());
            assert_eq!(body_of(first).await["run"], 0);

            let second = router
                .oneshot(request("/goto", Some(("repeated-id-0001", now))))
                .await
                .expect("router responds");
            assert_eq!(second.headers().get(REPLAYED).expect("marker"), "true");
            let body = body_of(second).await;
            assert_eq!(body["run"], 0, "the *original* outcome, not a second run");
            assert_eq!(body["replayed"], true);
            assert_eq!(
                runs.load(Ordering::SeqCst),
                1,
                "single execution is the whole acceptance criterion"
            );
        }

        /// §5.8.1's "echoing server time in every response" — including on a refusal, which is
        /// the response a client with a broken clock is most likely to be holding.
        #[tokio::test]
        async fn every_response_carries_server_time() {
            let ledger = ledger();
            let (router, _runs) = app(&ledger);
            let stale = chrono::Utc::now() - chrono::Duration::seconds(30);

            for req in [
                request("/estop", None),
                request("/goto", Some(("fresh-id-000001", chrono::Utc::now()))),
                request("/goto", Some(("stale-id-000001", stale))),
            ] {
                let response = router.clone().oneshot(req).await.expect("router responds");
                let stamped = response
                    .headers()
                    .get(SERVER_TIME)
                    .expect("server time")
                    .to_str()
                    .expect("ascii");
                assert!(stamped.ends_with('Z'), "SDD §2 wants UTC: {stamped}");
            }
        }

        /// The assembly guard: a covered mutation on a router with no ledger fails loudly rather
        /// than serving unchecked — and a stop still goes through, because nothing about a broken
        /// assembly may refuse one.
        #[tokio::test]
        async fn a_router_without_a_ledger_refuses_motion_and_still_honours_a_stop() {
            let runs: Runs = Arc::new(AtomicUsize::new(0));
            let router = ApiRouter::<()>::new()
                .post("/goto", RouteMeta::motion(Tier::Medium), counting(&runs))
                .post("/stop", RouteMeta::stopping(Tier::Low), counting(&runs))
                .into_router()
                .with_state(());
            let now = chrono::Utc::now();

            let refused = router
                .clone()
                .oneshot(request("/goto", Some(("no-ledger-id-01", now))))
                .await
                .expect("router responds");
            assert_eq!(refused.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(runs.load(Ordering::SeqCst), 0);

            let honoured = router
                .oneshot(request("/stop", Some(("no-ledger-id-02", now))))
                .await
                .expect("router responds");
            assert_eq!(honoured.status(), StatusCode::OK);
        }
    }
}
