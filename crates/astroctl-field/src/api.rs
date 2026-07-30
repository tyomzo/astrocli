//! The field node's HTTP surface: application state, the router, and the two system routes that
//! exist in M0 — SDD §5.8.1, §8.1, §8.2.
//!
//! # Middleware order, outermost first
//!
//! ```text
//! bearer auth (§4.5)        ← before routing: an unauthenticated caller cannot probe for paths
//!   └ routing
//!       └ route metadata (§8.2)   ← per route; publishes the tier, writes the audit record
//!           └ handler
//! ```
//!
//! Auth sits outside routing on purpose. It also means the stack of layers a request traverses
//! before reaching a handler is exactly two, which is what SDD §5.8.2 needs from this design:
//! the e-stop handler's "registered before the normal middleware stack (auth only, no JSON
//! parsing)" is, in this shape, just a route that declares no body extractor. Nothing has to be
//! bypassed, so nothing can be forgotten.
//!
//! # The static-asset seam (M0-T06)
//!
//! [`router`] returns the authenticated API surface and nothing else, with no fallback of its
//! own. [`crate::pwa`] merges the `include_dir!`-embedded PWA onto it and brings the SPA fallback;
//! the two subtrees stay separable because a browser cannot put an `Authorization` header on the
//! navigation request that loads the app shell, so the shell and the API cannot share one auth
//! policy. `crate::assemble` is where the two are joined.
//!
//! One consequence lands back here: after the merge the fallback for *every* unmatched path is
//! the PWA's, so "auth runs before routing" no longer covers an undeclared `/api/...` path by
//! itself. [`crate::pwa`] restores that property deliberately rather than by accident — see its
//! module docs.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use astroctl_core::bus::{EventBus, EVENT_BUS_CAPACITY};
use astroctl_core::config::FieldConfig;
use astroctl_core::error::ApiError;
use astroctl_core::event::EVENT_SCHEMA_VERSION;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::{require_bearer, AuthPolicy};
use crate::camera;
use crate::command::CommandLedger;
use crate::mount;
use crate::proxy::{self, StackProxy};
use crate::route_meta::{ApiRouter, CommandClass, RouteDecl, RouteMeta, Tier};
use crate::vitals::{self, Uptime};

/// Schema version of the `/api/system/*` response bodies (SDD §2: every externally visible JSON
/// schema carries one).
const API_SCHEMA_VERSION: u16 = 1;

/// Node lifecycle as reported by `/api/system/health` (SDD §8.1).
///
/// Only two states, because §8.1 names only two: the API comes up reporting `starting`, the
/// watchdogs start, and it becomes `ok`. There is deliberately no `stopping` — graceful shutdown
/// stops accepting connections first (SDD §7), so nothing could observe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// The API is up; the watchdogs are not running yet.
    Starting,
    /// Fully started.
    Ok,
}

impl NodeStatus {
    const fn code(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Ok => 1,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Ok,
            _ => Self::Starting,
        }
    }
}

/// What `/api/system/health` puts in `status`.
///
/// [`NodeStatus`]'s two lifecycle states plus `warn`, which is **derived when the response is
/// built and never stored**. SEC-07 degrades a perfectly running node whose certificate is inside
/// the expiry threshold, and that is a property of the clock rather than a transition the node
/// makes — putting it in [`StatusCell`] would collapse "the node is up" and "the node is healthy"
/// into one variable, and then nothing could turn the warning off again when the certificate is
/// renewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// The API is up; the watchdogs are not running yet.
    Starting,
    /// Fully started, nothing degraded.
    Ok,
    /// Serving normally, but something needs the operator before it stops being true.
    Warn,
}

impl HealthStatus {
    /// Lifecycle first: `starting` lasts milliseconds and "not ready yet" is the more actionable
    /// of the two readings while it does. The warning is still in `cert_days_remaining`, and it
    /// reaches `status` a moment later.
    fn of(lifecycle: NodeStatus, degraded: bool) -> Self {
        match (lifecycle, degraded) {
            (NodeStatus::Starting, _) => Self::Starting,
            (NodeStatus::Ok, false) => Self::Ok,
            (NodeStatus::Ok, true) => Self::Warn,
        }
    }
}

/// The `starting` → `ok` transition of SDD §8.1, shared between `main` and the handlers.
#[derive(Debug)]
pub struct StatusCell(AtomicU8);

impl StatusCell {
    /// A node that has not finished starting.
    #[must_use]
    pub const fn starting() -> Self {
        Self(AtomicU8::new(NodeStatus::Starting.code()))
    }

    /// Current status.
    #[must_use]
    pub fn get(&self) -> NodeStatus {
        NodeStatus::from_code(self.0.load(Ordering::Relaxed))
    }

    /// Declare the node started — called once the watchdogs are running (§8.1).
    pub fn set(&self, status: NodeStatus) {
        self.0.store(status.code(), Ordering::Relaxed);
    }
}

/// How the runtime was sized (SDD §7), reported by `/api/system/info`.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct RuntimeSizing {
    /// Worker threads the runtime was actually built with.
    pub worker_threads: usize,
    /// What `server.runtime_worker_threads` said; `null` means the SDD §7 default was applied.
    pub configured: Option<usize>,
    /// Cores visible to the process, i.e. what the default was computed from.
    pub available_cores: usize,
}

/// Where the node is logging, for `/api/system/info`.
#[derive(Clone, Debug, Serialize)]
pub struct LoggingInfo {
    /// Directory holding the rolling log file, or `null` if file logging is not running.
    pub dir: Option<String>,
    /// Why file logging is not running, if it is not.
    pub error: Option<String>,
}

/// Everything a handler needs. Cheap to clone: one `Arc` per field.
#[derive(Clone)]
pub struct AppState {
    /// The loaded, validated configuration (SDD §4.4 — nothing re-reads the file).
    pub config: Arc<FieldConfig>,
    /// The one event pipeline (SDD §4.3).
    pub bus: EventBus,
    /// `starting` → `ok` (SDD §8.1).
    pub status: Arc<StatusCell>,
    /// Process start time.
    pub uptime: Uptime,
    /// Resolved runtime sizing (SDD §7).
    pub runtime: RuntimeSizing,
    /// Authentication posture (SDD §4.5).
    pub auth: Arc<AuthPolicy>,
    /// The declared route table (SDD §8.2).
    pub routes: Arc<[RouteDecl]>,
    /// The `/stack/*` client (ADR-07).
    pub proxy: Arc<StackProxy>,
    /// The mount facade (SDD §5.8, M1-T03) — the routes and the 1 Hz poll share this.
    pub mount: Arc<crate::mount::MountFacade>,
    /// The camera facade (SDD §5.3.2, §5.6, M1-T08) — the routes, the capture task and the 60 s
    /// status poll share this. It carries the frame store, so `/api/session/*` reaches the session
    /// through the same handle that writes to it.
    pub camera: Arc<crate::camera::CameraFacade>,
    /// The transfer queue (SDD §5.10, M1-T11) — `/api/transfer/status` reads it, and the upload
    /// loop drains it. `None` inside when this node does not transfer; the route exists either
    /// way so that authentication does not behave differently depending on configuration.
    pub transfer: Arc<crate::transfer::TransferFacade>,
    /// Outstanding WebSocket tickets (SDD §4.5).
    pub tickets: Arc<crate::ticket::TicketStore>,
    /// Executed `command_id`s and their answers (SDD §5.8.1, §8.3(4), M1-T10).
    ///
    /// Beside the ticket store because it is the same kind of thing: per-node, per-process,
    /// deliberately not durable. A `command_id` issued before a restart belongs to a request whose
    /// answer the client can no longer act on.
    pub commands: Arc<CommandLedger>,
    /// The latest event per stateful topic, for the connect snapshot (SDD §5.8.3).
    pub snapshots: Arc<crate::ws::SnapshotStore>,
    /// The live-view fan-out and the camera stream feeding it (SDD §5.7, §8.3(5), M1-T09).
    ///
    /// One per node, not one per client: the camera has one sensor, and a second stream would be
    /// a second USB conversation with a body that only has one to give.
    pub liveview: Arc<crate::liveview::LiveViewHub>,
    /// Logging destination, for support questions.
    pub logging: LoggingInfo,
    /// The served certificate's expiry (SEC-07), or `None` on a plain-HTTP node.
    ///
    /// A value rather than a handle to the loader: the expiry is fixed once the certificate is
    /// loaded, and this node has no reload path — a renewal restarts the service (see
    /// `docs/ops/tls-setup.md`), which is what makes a `Copy` snapshot honest here.
    pub certificate: Option<crate::tls::CertificateStatus>,
}

/// Wraps an [`ApiError`] so it can be returned from a handler.
///
/// `ApiError` itself cannot implement `IntoResponse`: it lives in `astroctl-core`, which sits
/// below the API layer and must not pull axum into every crate in the workspace (SDD §4.2).
#[derive(Debug)]
pub struct ApiFailure(pub ApiError);

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.http_status())
            // Unreachable: `ErrorCode::http_status` is a total function over a closed enum and
            // every value in it is a valid status. Not an `expect`, because a panic here would
            // take the node down while *reporting an error*.
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0)).into_response()
    }
}

// ---------------------------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------------------------

/// Build the authenticated API surface and the route table it declares.
///
/// The state is applied by the caller so that `/api/system/info` can report the very table this
/// function produced — the declarations exist before the state that carries them does.
pub fn router() -> (Router<AppState>, Vec<RouteDecl>) {
    let api = ApiRouter::<AppState>::new()
        .get("/api/system/health", RouteMeta::read(), health)
        .get("/api/system/info", RouteMeta::read(), info)
        // --- SDD §4.5: the only way a browser authenticates `/ws` ------------------------------
        // `read` tier: issuing a ticket changes nothing an operator would want to confirm, and
        // it is the exchange that keeps the long-lived token out of URLs. Audited, because a
        // burst of ticket requests is what a reconnect storm looks like from the node's side.
        //
        // `not_a_command` despite being a POST, and the reason is §4.5's single use: the answer is
        // a nonce, so replaying a `command_id` would hand the second caller a ticket the first one
        // already spent — and requiring an envelope would put a 422 in the middle of the reconnect
        // path, which is the one exchange that has to work when the link is at its worst.
        .post("/api/auth/ws-ticket", RouteMeta::new(Tier::Read, true, CommandClass::NotACommand), mount::ws_ticket)
        // --- SDD §5.8.1: the mount rows ---------------------------------------------------
        //
        // Tiers are §5.8.1's verbatim. Everything that moves the telescope is audited: the
        // audit record is what lets an operator reconstruct what the node was asked to do on a
        // night something went wrong, and a motion command missing from it is the one you would
        // want.
        //
        // The command classes are M1-T10's, and they are a *second* axis — see [`CommandClass`].
        // Each one below is argued where it is declared, because a wrong class here is a safety
        // property rather than a preference.
        //
        // Connecting starts no motion and disconnecting stops none — the HAL contract is explicit
        // that a disconnect leaves a tracking mount tracking (SDD §7), which is what makes this
        // `neutral` rather than `stopping`. The envelope is still required: a retry over a tunnel
        // that dropped the reply should get the status the first call returned, not drive the
        // driver a second time.
        .post("/api/mount/connect", RouteMeta::neutral(Tier::Low), mount::connect)
        .post("/api/mount/disconnect", RouteMeta::neutral(Tier::Low), mount::disconnect)
        .get("/api/mount/position", RouteMeta::read(), mount::position)
        .get("/api/mount/status", RouteMeta::read(), mount::status)
        // §5.8.1 names goto first among the motion-initiating commands, and it is the clearest
        // case in the table: the request says "point there", the mount takes minutes to do it, and
        // a request that arrives five seconds late is acting on an intent the operator has already
        // replaced with another one.
        .post("/api/mount/goto", RouteMeta::motion(Tier::Medium), mount::goto)
        // §5.8.1 lists "tracking start" as motion-initiating and "tracking off" as stopping — but
        // gives them **one route and a `mode` field**, so a per-route class cannot express both.
        // Declared `motion_initiating`, deliberately, for two reasons. The mechanism: a class read
        // from the body would be a safety property the *client* picks, and a client that can spell
        // `mode: "off"` can spell it on a request that starts sidereal tracking. The consequence:
        // the command this over-refuses is "stop tracking", whose refusal leaves the mount
        // tracking — the state SDD §7 calls the safe one — while the stops that must never be
        // refused (`slew/stop`, `capture/abort`, `estop`) each have a route of their own below.
        .post("/api/mount/tracking", RouteMeta::motion(Tier::Low), mount::tracking)
        // A slew *lease*, and every renewal is a fresh command with a fresh id — see
        // `frontend/src/lib/slewHold.ts`. Staleness applies to renewals too and should: the lease
        // is at most 2000 ms (§5.8.1's clamp), so a renewal delayed past `max_command_age_ms` is
        // one whose axis the dead-man's switch has already stopped.
        .post("/api/mount/slew", RouteMeta::motion(Tier::Low), mount::slew)
        // §5.8.1's first named stopping command.
        .post("/api/mount/slew/stop", RouteMeta::stopping(Tier::Low), mount::slew_stop)
        // Park is a slew, so it is motion-initiating for the same reason goto is, whatever its
        // tier says about consequence.
        .post("/api/mount/park", RouteMeta::motion(Tier::High), mount::park)
        // Unpark is the one `high` route that moves nothing: it releases the park interlock and
        // returns (see [`mount::unpark`]). A late unpark starts nothing, so there is nothing for
        // the age check to protect — and refusing it would leave a mount that cannot be unparked
        // over a slow link.
        .post("/api/mount/unpark", RouteMeta::neutral(Tier::High), mount::unpark)
        // --- SDD §5.8.2: the e-stop (REL-01, PRF-12, MNT-08) -----------------------------
        //
        // `BlockedForLlm` is §5.8.1's own tier for this row: an emergency stop is the operator's
        // act and nothing else's. Audited, obviously — this is the line of the night's log an
        // incident review starts from.
        //
        // Registered here, in the same table as everything else, and that is §5.8.2 satisfied
        // rather than sidestepped: "before the normal middleware stack (auth only, no JSON
        // parsing)" describes a *stack of two* — auth, then routing — which is what this router
        // already is (see the module docs). The "no JSON parsing" half is a property of the
        // handler's signature: `mount::estop` declares no body extractor, so there is nothing
        // between the bearer check and the driver call that could refuse the request.
        //
        // `exempt` from the M1-T10 envelope, and the exemption is *declared* rather than inherited
        // from the absence of a body. §5.8.1 already names the e-stop among the stopping commands
        // that are never staleness-rejected; `Stopping` would have satisfied that. `Exempt` says
        // the stronger thing §5.8.2 needs: this route reads nothing off the request at all. No
        // header is parsed, no id is looked up, no lock is taken — there is nothing between the
        // bearer check and `emergency_stop()` that could answer anything but a stop.
        .post(
            "/api/mount/estop",
            RouteMeta::new(Tier::BlockedForLlm, true, CommandClass::Exempt),
            mount::estop,
        )
        // --- SDD §5.8.1: the camera rows (M1-T08) ------------------------------------------
        //
        // Tiers verbatim from §5.8.1: capture is `medium` beside goto, because §8.2's tier is a
        // *consequence* class and an exposure the operator did not ask for costs a slot in the
        // night rather than a telescope. Everything that changes state is audited, capture most
        // of all — the audit line is what ties a frame in the session directory to the request
        // that asked for it.
        .post("/api/camera/connect", RouteMeta::neutral(Tier::Low), camera::connect)
        .post("/api/camera/disconnect", RouteMeta::neutral(Tier::Low), camera::disconnect)
        .get("/api/camera/settings", RouteMeta::read(), camera::settings)
        // `neutral`, and the method already said why (see [`ApiRouter::put`]): a settings value is
        // a *state*, not an event. One that arrives five seconds late is exactly as correct as one
        // that arrives instantly, so there is nothing for staleness to protect — while the
        // idempotency half is the whole reason this row is a PUT rather than a POST.
        .put("/api/camera/settings", RouteMeta::neutral(Tier::Low), camera::update_settings)
        // §5.8.1 names "capture start" among the motion-initiating commands, and the class name is
        // its category rather than a claim about motors: an exposure the operator no longer wants
        // still costs a slot in the night, still moves a shutter, and — with the mount slewing to
        // the next target — still writes a frame of a smear.
        .post("/api/camera/capture", RouteMeta::motion(Tier::Medium), camera::capture)
        // §5.8.1's second named stopping command.
        .post(
            "/api/camera/capture/abort",
            RouteMeta::stopping(Tier::Low),
            camera::abort_capture,
        )
        // The one route SDD §5.8.1's table does not list, because §5.6 is where it is required:
        // "Faulted (from any state; operator ack → Idle)" has no way to happen without a route to
        // do it on. `low` and audited — it changes nothing about the telescope, but a night in
        // which a fault was acknowledged three times is a night whose log should say so.
        //
        // `neutral`: acknowledging a fault starts nothing and stops nothing, it clears a latch.
        // An acknowledgement delayed by the link is still an acknowledgement of the fault that is
        // still there — and if the fault has already been cleared, the route answers that it had
        // nothing to clear.
        .post(
            "/api/camera/fault/ack",
            RouteMeta::neutral(Tier::Low),
            camera::acknowledge_fault,
        )
        .get("/api/camera/battery", RouteMeta::read(), camera::battery)
        .get("/api/camera/storage", RouteMeta::read(), camera::storage)
        // Two more routes SDD §5.8.1's table does not list. §5.7 requires live view to be
        // startable and stoppable — "stopping closes the stream server-side" is an M1-T09
        // acceptance criterion — and the table has no row for either, because it describes the
        // *socket* the frames arrive on and not the control that opens the tap.
        //
        // `low` and audited, matching the camera's other state-changing routes: starting live
        // view moves no telescope, but it does start a sensor loop and a USB conversation, and a
        // night where the stream was started nine times is a night whose log should say so.
        //
        // `neutral` for start, `stopping` for stop, and the pair is worth stating because start is
        // the one route where the argument goes the other way and loses. Live view drives the
        // sensor, so it looks motion-shaped; but it acts only on the node's own screen — no motor,
        // no shutter, nothing on disk — and the operator's remedy for a stream they no longer want
        // is the stop below, which is never refused. Against that, refusing a stale start means a
        // black image panel on exactly the slow link that made it stale.
        .post(
            "/api/camera/liveview/start",
            RouteMeta::neutral(Tier::Low),
            crate::liveview::start,
        )
        .post(
            "/api/camera/liveview/stop",
            RouteMeta::stopping(Tier::Low),
            crate::liveview::stop,
        )
        .get("/api/session/current", RouteMeta::read(), camera::session_current)
        .get(
            "/api/session/frames/{id}/preview.jpg",
            RouteMeta::read(),
            crate::liveview::preview,
        )
        // SDD §5.10.4. `read`/`NotACommand`: it observes the upload queue and changes nothing —
        // not even the queue's own state, which only the drain loop moves. Unaudited for the same
        // reason every other `read` row is: §8.2 puts the audit log on state changes, and "someone
        // looked at the queue depth" is not one.
        .get(
            "/api/transfer/status",
            RouteMeta::read(),
            crate::transfer::status,
        )
        // ADR-07. Audited: a call that changes state on the other node is exactly as consequential
        // as one that changes state here, and this node cannot tell which is which — so it records
        // every one of them.
        //
        // The command class defers for the same reason the tier does: the envelope headers are
        // forwarded verbatim with everything else, so the node that serves the request is the one
        // that decides what they mean there. This node classifying them would be it deciding
        // staleness for routes it cannot see.
        .any(
            "/stack",
            RouteMeta::new(Tier::PassThrough, true, CommandClass::PassThrough),
            proxy::handler,
        )
        .any(
            "/stack/{*rest}",
            RouteMeta::new(Tier::PassThrough, true, CommandClass::PassThrough),
            proxy::handler,
        );

    let declarations = api.declarations();
    (api.into_router(), declarations)
}

/// The ticket-authenticated surface: `/ws` and `/ws/liveview` (SDD §4.5, §5.8.3).
///
/// Separate from [`router`] because it is authenticated differently, and separate at the
/// *router* level rather than by a path exemption inside [`require_bearer`]. The distinction
/// matters: an exemption list is one line away from covering a second path by accident, whereas
/// a router that is never handed to `with_auth` cannot leak its policy to anything but its own
/// members. `crate::assemble` is where the three subtrees meet.
///
/// It still goes through [`ApiRouter`], so `/ws` appears in the table `/api/system/info`
/// publishes (§8.2) and gets the same per-route metadata layer as everything else. `audit:
/// false` — the record would fire on the *handshake*, reporting a 101 and a latency that is the
/// upgrade rather than the session, which is a log line that looks like information and is not.
pub fn ws_router() -> (Router<AppState>, Vec<RouteDecl>) {
    let ws = ApiRouter::<AppState>::new()
        .get("/ws", RouteMeta::read(), crate::ws::upgrade)
        // §8.3(5)'s second socket. It belongs *here*, beside `/ws`, for the reason this router
        // exists at all: both are ticket-authenticated and neither may pass through the bearer
        // layer. A liveview route added to `router()` instead would demand a bearer header the
        // browser cannot attach to a WebSocket upgrade, and would look like a working route on
        // every non-browser client.
        .get("/ws/liveview", RouteMeta::read(), crate::liveview::upgrade)
        // The stacking server's sockets, proxied (ADR-07, §5.8.1 "WS upgrades proxied too, so the
        // operator keeps a single origin"). Here rather than beside `/stack/{*rest}` for exactly
        // the reason given above: this is a browser upgrade, the browser cannot attach a bearer
        // header, and a route inside `with_auth` would answer 401 before the ticket was ever
        // looked at. It is the *same* handler — `proxy::handler` branches on the upgrade headers
        // — so the split is about which authentication the request passes through, not about two
        // implementations.
        //
        // More specific than `/stack/{*rest}`, so axum routes `/stack/ws/preview` here and
        // everything else under `/stack/` to the bearer-authenticated proxy.
        .any(
            "/stack/ws/{*rest}",
            RouteMeta::new(Tier::PassThrough, true, CommandClass::PassThrough),
            proxy::handler,
        );
    let declarations = ws.declarations();
    (ws.into_router(), declarations)
}

/// Apply the node's state to a router — the one place `Router<AppState>` becomes `Router`.
///
/// It exists because of one thing the per-route envelope layer needs and cannot ask for. That
/// layer is built inside [`router`], long before any [`AppState`] exists, so axum has no way to
/// hand it the node's [`CommandLedger`]: `from_fn_with_state` fixes its state at construction, and
/// the state a `Router<S>` will eventually be given is not available then. The ledger therefore
/// travels as a request extension, installed here — at the exact seam where the router and the
/// state meet, so there is one line to get right rather than one per assembly site.
///
/// A router that skipped this would not fail quietly: `admit` answers `500` on a covered mutation
/// when the extension is absent, and the assembled-app tests drive that path.
pub fn with_state(router: Router<AppState>, state: AppState) -> Router {
    router
        .layer(axum::Extension(Arc::clone(&state.commands)))
        .with_state(state)
}

/// Apply the bearer-auth layer to a router (SDD §4.5).
///
/// Separate from [`router`] so the ordering is visible at the call site rather than buried:
/// auth is the outermost layer, and every route inside it — including the ones M1 adds and the
/// WS upgrades — is covered by construction.
pub fn with_auth(router: Router, auth: Arc<AuthPolicy>) -> Router {
    router.layer(axum::middleware::from_fn_with_state(auth, require_bearer))
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// Version strings for `/api/system/health` (SDD §5.8.1 `versions`).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Versions {
    /// Binary version.
    astroctl: &'static str,
    /// `/api/system/*` body schema.
    api: u16,
    /// Event schema (SDD §4.3).
    event: u16,
    /// Error envelope schema (SDD §4.2).
    error_envelope: u16,
}

impl Versions {
    const fn current() -> Self {
        Self {
            astroctl: env!("CARGO_PKG_VERSION"),
            api: API_SCHEMA_VERSION,
            event: EVENT_SCHEMA_VERSION,
            error_envelope: ApiError::SCHEMA_VERSION,
        }
    }
}

/// `GET /api/system/health` — SDD §5.8.1, plus SEC-07's certificate expiry.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    v: u16,
    status: HealthStatus,
    /// `null` when the volume cannot be interrogated — never a fabricated 0.0 (see
    /// [`crate::vitals::disk_free_gb`]).
    disk_free_gb: Option<f64>,
    clock_synced: bool,
    uptime_s: u64,
    /// `notAfter` of the served certificate as UTC RFC 3339 (SDD §2), or `null` on a node
    /// serving plain HTTP — which is a supported mode, so `null` is not a fault.
    cert_expires_at: Option<String>,
    /// Whole days until that moment, **negative once it has passed**. An expired certificate
    /// revokes the secure context exactly as a missing one does, so it disables the wake lock and
    /// the installed app; SEC-07 exists so the operator does not discover that in a field.
    cert_days_remaining: Option<i64>,
    versions: Versions,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    // One `now` for both fields, so the reported expiry and the reported status can never
    // disagree about which side of the threshold this instant is on.
    let now = chrono::Utc::now();
    let certificate = state.certificate;

    Json(HealthResponse {
        v: API_SCHEMA_VERSION,
        status: HealthStatus::of(
            state.status.get(),
            certificate.is_some_and(|c| c.is_warning(now)),
        ),
        disk_free_gb: vitals::disk_free_gb(&state.config.storage.sessions_dir),
        clock_synced: vitals::clock_synced(),
        uptime_s: state.uptime.seconds(),
        cert_expires_at: certificate.map(|c| c.expires_at_rfc3339()),
        cert_days_remaining: certificate.map(|c| c.days_remaining(now)),
        versions: Versions::current(),
    })
}

/// `GET /api/system/info` — SDD §5.8.1 ("config summary, driver list, capabilities").
#[derive(Debug, Serialize)]
pub struct InfoResponse {
    v: u16,
    node: &'static str,
    status: NodeStatus,
    versions: Versions,
    runtime: RuntimeSizing,
    server: ServerInfo,
    logging: LoggingInfo,
    config: ConfigSummary,
    /// Drivers built by the HAL registry. Empty in M0: the registry is M1-T01, and hardware is
    /// never connected at startup regardless (SDD §8.1).
    drivers: Vec<DriverInfo>,
    /// The route table as declared (SDD §8.2). Published so the LLM tool generator and an
    /// operator debugging a 403 read the same list the server routes on.
    routes: Vec<RouteDecl>,
    /// The tier vocabulary and the confirmation policy each tier carries (SDD §8.2, PRD §8.1
    /// `llm.confirmation_tiers`). The same declaration that routes a request decides this.
    tiers: Vec<TierPolicy>,
    /// The command-class vocabulary and what each one does to the envelope (SDD §5.8.1, M1-T10).
    ///
    /// Published for the same reason the tiers are: a client that wants to know whether a route
    /// will refuse it for staleness should read the answer off the node it is talking to, not off
    /// a copy of this table compiled into its own bundle.
    command_classes: Vec<CommandClassPolicy>,
    event_bus_capacity: usize,
    /// Live bus subscribers — the WS hub's clients (M1) plus the event log.
    event_bus_subscribers: usize,
}

/// One tier and what it means for the LLM control layer.
#[derive(Debug, Serialize)]
pub struct TierPolicy {
    tier: Tier,
    /// Whether the LLM tool generator (ADD §6.1) may expose routes on this tier at all.
    llm_callable: bool,
    /// `auto` / `confirm` / `confirm_warn`, or `null` for a tier the LLM cannot call.
    confirmation: Option<&'static str>,
}

impl TierPolicy {
    fn of(tier: Tier, tiers: &astroctl_core::config::ConfirmationTiers) -> Self {
        use astroctl_core::config::ConfirmationMode;
        Self {
            tier,
            llm_callable: tier.llm_callable(),
            // `ConfirmationMode` derives only `Deserialize` (it is config input, never output),
            // so the wire spelling is written out here rather than delegated to serde.
            confirmation: tier.confirmation_mode(tiers).map(|mode| match mode {
                ConfirmationMode::Auto => "auto",
                ConfirmationMode::Confirm => "confirm",
                ConfirmationMode::ConfirmWarn => "confirm_warn",
            }),
        }
    }
}

/// One command class and what the envelope layer does with it (SDD §5.8.1).
#[derive(Debug, Serialize)]
pub struct CommandClassPolicy {
    class: CommandClass,
    /// Whether a request on such a route is refused without an envelope.
    requires_envelope: bool,
    /// Whether `issued_at` older than `max_command_age_ms` refuses it.
    checks_staleness: bool,
}

impl CommandClassPolicy {
    const fn of(class: CommandClass) -> Self {
        Self {
            class,
            requires_envelope: class.requires_envelope(),
            checks_staleness: class.checks_staleness(),
        }
    }
}

/// Bind address and authentication posture.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    bind: String,
    /// `false` means this node is running under the SDD §4.5 loopback exception, unauthenticated.
    auth_enforced: bool,
    max_command_age_ms: u64,
    /// Whether this node terminates TLS (SEC-05). The operator's first question when the PWA
    /// will not install is "is this origin actually https", and the answer should not require
    /// reading the startup log of a process that has been running for a week.
    tls: bool,
}

/// The parts of the configuration that answer "what is this node set up to do".
#[derive(Debug, Serialize)]
pub struct ConfigSummary {
    site: SiteSummary,
    mount_driver: String,
    camera_driver: String,
    guide_camera_driver: Option<String>,
    solver_backend: String,
    sessions_dir: String,
    disk_warn_free_gb: f64,
    disk_critical_free_gb: f64,
    stacking_server: StackSummary,
    llm_enabled: bool,
}

/// Observing site.
#[derive(Debug, Serialize)]
pub struct SiteSummary {
    latitude: f64,
    longitude: f64,
    elevation: f64,
    timezone: String,
}

/// Where `/stack/*` points.
#[derive(Debug, Serialize)]
pub struct StackSummary {
    enabled: bool,
    /// `http://host:port`, or `null` when disabled.
    upstream: Option<String>,
}

/// One entry of the HAL registry's driver list (M1-T01).
#[derive(Debug, Serialize)]
pub struct DriverInfo {
    kind: String,
    name: String,
}

async fn info(State(state): State<AppState>) -> Json<InfoResponse> {
    let config = &state.config;
    Json(InfoResponse {
        v: API_SCHEMA_VERSION,
        node: "field",
        status: state.status.get(),
        versions: Versions::current(),
        runtime: state.runtime,
        server: ServerInfo {
            bind: format!("{}:{}", config.server.host, config.server.port),
            auth_enforced: state.auth.is_enforced(),
            max_command_age_ms: config.server.max_command_age_ms,
            tls: state.certificate.is_some(),
        },
        logging: state.logging.clone(),
        config: ConfigSummary {
            site: SiteSummary {
                latitude: config.site.latitude,
                longitude: config.site.longitude,
                elevation: config.site.elevation,
                timezone: config.site.timezone.clone(),
            },
            mount_driver: debug_name(&config.mount.driver),
            camera_driver: debug_name(&config.camera.driver),
            guide_camera_driver: config.guide_camera.driver.as_ref().map(debug_name),
            solver_backend: debug_name(&config.solver.backend),
            sessions_dir: config.storage.sessions_dir.display().to_string(),
            disk_warn_free_gb: config.storage.disk_warn_free_gb,
            disk_critical_free_gb: config.storage.disk_critical_free_gb,
            stacking_server: StackSummary {
                enabled: config.stacking_server.enabled,
                upstream: state.proxy.upstream(),
            },
            llm_enabled: config.llm.enabled,
        },
        drivers: Vec::new(),
        routes: state.routes.to_vec(),
        tiers: Tier::ALL
            .iter()
            .map(|&tier| TierPolicy::of(tier, &config.llm.confirmation_tiers))
            .collect(),
        command_classes: CommandClass::ALL
            .iter()
            .copied()
            .map(CommandClassPolicy::of)
            .collect(),
        event_bus_capacity: EVENT_BUS_CAPACITY,
        event_bus_subscribers: state.bus.subscriber_count(),
    })
}

/// Render a config enum by its `Debug` name, lowercased.
///
/// The config enums derive only `Deserialize`, so there is no `Serialize` to reuse and no
/// `as_str` to call. Lowercasing `Debug` gives `skywatcher`, `gphoto2`, `astap` — the same
/// tokens the operator wrote in the YAML, which is the point of showing them at all.
fn debug_name<T: std::fmt::Debug>(value: &T) -> String {
    format!("{value:?}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{state_with, TestNode};
    use axum::body::Body;
    use axum::http::{header, Request};
    use serde_json::Value;
    use tower::ServiceExt as _;

    /// Drive the real router, with auth, exactly as `main` assembles it.
    async fn call(node: &TestNode, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let (router, declarations) = router();
        let state = state_with(node, declarations).await;
        let auth = Arc::clone(&state.auth);
        let app = with_auth(with_state(router, state), auth);

        let mut request = Request::builder().uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = app
            .oneshot(request.body(Body::empty()).expect("request builds"))
            .await
            .expect("router responds");

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    // --- auth on every route ----------------------------------------------------------

    #[tokio::test]
    async fn every_route_answers_401_with_the_auth_envelope_without_a_token() {
        let node = TestNode::authenticated("s3cret");
        for path in [
            "/api/system/health",
            "/api/system/info",
            "/stack/api/system/health",
            "/stack",
            // Not a declared route: auth runs before routing, so this is 401 rather than 404 —
            // an unauthenticated caller cannot map the API by probing.
            "/api/mount/position",
        ] {
            let (status, body) = call(&node, path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "for {path}");
            assert_eq!(body["code"], "AUTH", "for {path}: {body}");
            assert_eq!(body["v"], 1, "for {path}");
            assert_eq!(body["retryable"], false, "for {path}");
            assert!(body["message"].is_string(), "for {path}");
        }
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused_on_the_proxy_path_too() {
        let node = TestNode::authenticated("s3cret");
        for path in ["/api/system/health", "/stack/api/system/health"] {
            let (status, body) = call(&node, path, Some("hunter2")).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "for {path}");
            assert_eq!(body["code"], "AUTH", "for {path}");
        }
    }

    // --- health -----------------------------------------------------------------------

    #[tokio::test]
    async fn health_reports_the_sdd_5_8_1_fields() {
        let node = TestNode::authenticated("s3cret");
        let (status, body) = call(&node, "/api/system/health", Some("s3cret")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["v"], 1);
        // The API is up before the watchdogs are (SDD §8.1).
        assert_eq!(body["status"], "starting");
        assert!(
            body["disk_free_gb"].as_f64().is_some_and(|gb| gb >= 0.0),
            "disk_free_gb: {body}"
        );
        assert!(body["clock_synced"].is_boolean(), "{body}");
        assert!(body["uptime_s"].is_u64(), "{body}");
        assert_eq!(body["versions"]["astroctl"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["versions"]["event"], 1);
    }

    #[tokio::test]
    async fn health_reports_ok_once_the_watchdogs_are_running() {
        let node = TestNode::authenticated("s3cret");
        let (router, declarations) = router();
        let state = state_with(&node, declarations).await;
        state.status.set(NodeStatus::Ok);
        let auth = Arc::clone(&state.auth);
        let app = with_auth(with_state(router, state), auth);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/system/health")
                    .header(header::AUTHORIZATION, "Bearer s3cret")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        let body: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["status"], "ok");
    }

    // --- SEC-07: certificate expiry in health -----------------------------------------

    /// A node with no `tls` block is a supported deployment, not a broken one, so the fields are
    /// `null` rather than absent or zero — a dashboard that reads 0 days remaining would be
    /// wrong in the alarming direction.
    #[tokio::test]
    async fn a_plain_http_node_reports_no_certificate_rather_than_a_fabricated_one() {
        let node = TestNode::authenticated("s3cret");
        let (_, body) = call(&node, "/api/system/health", Some("s3cret")).await;
        assert_eq!(body["cert_expires_at"], Value::Null, "{body}");
        assert_eq!(body["cert_days_remaining"], Value::Null, "{body}");
        assert_eq!(body["status"], "starting", "{body}");

        let (_, info) = call(&node, "/api/system/info", Some("s3cret")).await;
        assert_eq!(info["server"]["tls"], false, "{info}");
    }

    #[tokio::test]
    async fn a_tls_node_reports_its_certificates_expiry() {
        let node = TestNode::authenticated("s3cret").with_tls(
            "ecdsa-sec1.crt.pem",
            "ecdsa-sec1.key.pem",
            14,
        );
        let (status, body) = call(&node, "/api/system/health", Some("s3cret")).await;

        assert_eq!(status, StatusCode::OK);
        let expires_at = body["cert_expires_at"]
            .as_str()
            .expect("a timestamp: {body}");
        assert!(expires_at.ends_with('Z'), "UTC per SDD §2: {expires_at}");
        // The fixture is long-dated, so this also asserts the threshold is not simply always on.
        assert!(
            body["cert_days_remaining"].as_i64().is_some_and(|d| d > 14),
            "{body}"
        );

        let (_, info) = call(&node, "/api/system/info", Some("s3cret")).await;
        assert_eq!(info["server"]["tls"], true, "{info}");
    }

    /// Drive the health handler on a started node, which `call` cannot do — it builds state at
    /// `starting`, and the degraded reading only applies once the lifecycle has reached `ok`.
    async fn health_of_started(node: &TestNode) -> Value {
        let (router, declarations) = router();
        let state = state_with(node, declarations).await;
        state.status.set(NodeStatus::Ok);
        let auth = Arc::clone(&state.auth);
        let app = with_auth(with_state(router, state), auth);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/system/health")
                    .header(header::AUTHORIZATION, "Bearer s3cret")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        serde_json::from_slice(&bytes).expect("json")
    }

    /// The acceptance criterion, through the real handler. The fixture certificate expired in
    /// 2020, so it is permanently inside any threshold — a certificate short-dated at test time
    /// would assert the same thing today and start failing on the day it lapsed.
    #[tokio::test]
    async fn a_certificate_inside_the_threshold_degrades_status_to_warn() {
        let node =
            TestNode::authenticated("s3cret").with_tls("expired.crt.pem", "expired.key.pem", 14);
        let body = health_of_started(&node).await;

        assert_eq!(body["status"], "warn", "{body}");
        assert!(
            body["cert_days_remaining"]
                .as_i64()
                .is_some_and(|days| days < 0),
            "an expiry in the past must read as negative days, not as zero: {body}"
        );
        assert_eq!(
            body["cert_expires_at"], "2020-04-01T00:00:00.000Z",
            "{body}"
        );
    }

    /// The other half of the same criterion: a healthy certificate must not degrade anything,
    /// or `warn` would carry no information.
    #[tokio::test]
    async fn a_healthy_certificate_leaves_a_started_node_ok() {
        let node = TestNode::authenticated("s3cret").with_tls(
            "ecdsa-sec1.crt.pem",
            "ecdsa-sec1.key.pem",
            14,
        );
        let body = health_of_started(&node).await;
        assert_eq!(body["status"], "ok", "{body}");
    }

    /// `warn` must not mask "not ready yet": during the milliseconds before the watchdogs are up,
    /// the lifecycle is the more actionable of the two readings.
    #[test]
    fn a_starting_node_reports_starting_even_with_an_expiring_certificate() {
        assert_eq!(
            HealthStatus::of(NodeStatus::Starting, true),
            HealthStatus::Starting
        );
        assert_eq!(HealthStatus::of(NodeStatus::Ok, true), HealthStatus::Warn);
        assert_eq!(HealthStatus::of(NodeStatus::Ok, false), HealthStatus::Ok);
    }

    // --- info -------------------------------------------------------------------------

    /// The acceptance criterion: the resolved worker-thread count is in `/api/system/info`, and a
    /// configured value changes it.
    #[tokio::test]
    async fn info_reports_the_resolved_worker_thread_count() {
        let default_node = TestNode::authenticated("s3cret");
        let (_, body) = call(&default_node, "/api/system/info", Some("s3cret")).await;
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let expected = 2.min(cores.saturating_sub(2)).max(1);
        assert_eq!(body["runtime"]["worker_threads"], expected, "{body}");
        assert_eq!(body["runtime"]["configured"], Value::Null);
        assert_eq!(body["runtime"]["available_cores"], cores);

        let pinned = TestNode::authenticated("s3cret").with_worker_threads(Some(1));
        let (_, body) = call(&pinned, "/api/system/info", Some("s3cret")).await;
        assert_eq!(body["runtime"]["worker_threads"], 1, "{body}");
        assert_eq!(body["runtime"]["configured"], 1);
    }

    #[tokio::test]
    async fn info_publishes_the_declared_route_table() {
        let node = TestNode::authenticated("s3cret");
        let (_, body) = call(&node, "/api/system/info", Some("s3cret")).await;

        let routes = body["routes"].as_array().expect("routes is a list");
        let health = routes
            .iter()
            .find(|r| r["path"] == "/api/system/health")
            .expect("health is declared");
        assert_eq!(health["tier"], "read");
        assert_eq!(health["audit"], false);
        assert_eq!(health["method"], "GET");

        let proxy = routes
            .iter()
            .find(|r| r["path"] == "/stack/{*rest}")
            .expect("the proxy is declared");
        assert_eq!(proxy["tier"], "pass_through");
        assert_eq!(proxy["audit"], true);
        assert_eq!(proxy["method"], "ANY");

        // A motion route, so the table's *content* is checked and not only its size: §5.8.1
        // gives goto the `medium` tier, which is what Phase 2c will make the LLM ask about.
        let goto = routes
            .iter()
            .find(|r| r["path"] == "/api/mount/goto")
            .expect("goto is declared");
        assert_eq!(goto["tier"], "medium");
        assert_eq!(
            goto["audit"], true,
            "a command that moves the telescope is audited"
        );
        assert_eq!(goto["method"], "POST");

        // The e-stop is declared like everything else, and on §5.8.1's own tier: the LLM control
        // layer must never be able to call it, and `llm_callable` is derived from this row.
        let estop = routes
            .iter()
            .find(|r| r["path"] == "/api/mount/estop")
            .expect("the e-stop is declared");
        assert_eq!(estop["tier"], "blocked_for_llm");
        assert_eq!(estop["audit"], true);
        assert_eq!(estop["method"], "POST");
        let blocked = body["tiers"]
            .as_array()
            .expect("tiers")
            .iter()
            .find(|t| t["tier"] == "blocked_for_llm")
            .expect("the tier is published");
        assert_eq!(blocked["llm_callable"], false);

        // The camera rows of §5.8.1, and the one route §5.6 requires that §5.8.1 does not list.
        let capture = routes
            .iter()
            .find(|r| r["path"] == "/api/camera/capture")
            .expect("capture is declared");
        assert_eq!(
            capture["tier"], "medium",
            "an exposure the operator did not ask for is a `medium` consequence, beside goto"
        );
        assert_eq!(capture["audit"], true);
        assert_eq!(capture["method"], "POST");

        // The same path on two methods and two tiers: reading the settings is `read`, changing
        // them is `low`. A table that collapsed them would have to pick one, and Phase 2c's
        // confirmation policy would then either confirm every read or confirm no change.
        let settings: Vec<_> = routes
            .iter()
            .filter(|r| r["path"] == "/api/camera/settings")
            .collect();
        assert_eq!(settings.len(), 2, "{settings:?}");
        assert!(settings
            .iter()
            .any(|r| r["method"] == "GET" && r["tier"] == "read"));
        assert!(settings
            .iter()
            .any(|r| r["method"] == "PUT" && r["tier"] == "low" && r["audit"] == true));

        // SDD §5.6's "operator ack → Idle" has no row in §5.8.1's table, and a state that cannot
        // be cleared is a node that stops capturing for the night.
        let ack = routes
            .iter()
            .find(|r| r["path"] == "/api/camera/fault/ack")
            .expect("the fault acknowledgement is declared");
        assert_eq!(ack["method"], "POST");
        assert_eq!(ack["tier"], "low");
        assert_eq!(ack["audit"], true);

        // The live-view controls and the preview image (M1-T09). None of the three is in
        // §5.8.1's table — the table names the socket the frames arrive on, not the tap that
        // opens it — so the declaration is where they are pinned.
        let start = routes
            .iter()
            .find(|r| r["path"] == "/api/camera/liveview/start")
            .expect("live view start is declared");
        assert_eq!(start["method"], "POST");
        assert_eq!(start["tier"], "low");
        assert_eq!(start["audit"], true);

        let preview = routes
            .iter()
            .find(|r| r["path"] == "/api/session/frames/{id}/preview.jpg")
            .expect("the preview image is declared");
        assert_eq!(preview["method"], "GET");
        assert_eq!(
            preview["tier"], "read",
            "serving a cached image reads state and changes none"
        );

        // Every route the router serves is declared — that is the §8.2 invariant, and the
        // declaration list is the only place a route can come from. The count is the review
        // checkpoint: 4 → 15 is M1-T03's ws-ticket row plus the ten mount rows of §5.8.1,
        // 15 → 16 is M1-T05's e-stop, 16 → 26 is M1-T08's nine camera/session rows plus the
        // fault acknowledgement, 26 → 29 is M1-T09's live-view start/stop and the preview
        // image, and 29 → 30 is M1-T11's `/api/transfer/status`. `/ws` and `/ws/liveview` are
        // *not* among them because they are declared by `ws_router()`, which is authenticated by
        // ticket rather than by the bearer layer this test drives.
        assert_eq!(routes.len(), 30);
    }

    /// The route-eleven guard (M1-T10): no route that can change state escapes classification.
    ///
    /// Written as a rule over the whole table rather than as a list of the routes that exist
    /// today, because the failure this task is about is the *next* route — one added with a tier
    /// and no thought about staleness, which a per-route assertion would not notice. The three
    /// exceptions are named here with their reasons, so adding a fourth is a deliberate edit to a
    /// test rather than a silent omission.
    #[test]
    fn every_mutating_route_declares_a_command_class() {
        let (_, routes) = router();

        for route in &routes {
            let mutating = matches!(route.method, "POST" | "PUT");
            let excused = matches!(
                (route.path, route.method),
                // A nonce, not a command — see the declaration.
                ("/api/auth/ws-ticket", "POST")
                    // Classified by the node that serves it (ADR-07).
                    | ("/stack", "ANY")
                    | ("/stack/{*rest}", "ANY")
            );

            if mutating && !excused {
                assert_ne!(
                    route.meta.command,
                    CommandClass::NotACommand,
                    "{} {} changes state and declares no command class: SDD §5.8.1's envelope \
                     would not cover it",
                    route.method,
                    route.path
                );
            }
            if route.method == "GET" {
                assert_eq!(
                    route.meta.command,
                    CommandClass::NotACommand,
                    "{} reads state; a command class on it would claim otherwise",
                    route.path
                );
            }
            assert_eq!(
                route.meta.tier == Tier::PassThrough,
                route.meta.command == CommandClass::PassThrough,
                "{} {}: the two pass-through declarations defer to the same other node and must \
                 agree",
                route.method,
                route.path
            );
        }

        // The exemption is one route, and it is the one §5.8.2 is about. A second `exempt` row is
        // the shape of this mechanism being worked around.
        let exempt: Vec<_> = routes
            .iter()
            .filter(|r| r.meta.command == CommandClass::Exempt)
            .map(|r| r.path)
            .collect();
        assert_eq!(exempt, vec!["/api/mount/estop"], "{exempt:?}");

        // And the safety-critical classes by name, because a flip between these two is the one
        // mistake in this table that a reviewer could read past.
        let class_of = |path: &str, method: &str| {
            routes
                .iter()
                .find(|r| r.path == path && r.method == method)
                .unwrap_or_else(|| panic!("{method} {path} is declared"))
                .meta
                .command
        };
        for path in [
            "/api/mount/goto",
            "/api/mount/slew",
            "/api/mount/park",
            "/api/mount/tracking",
            "/api/camera/capture",
        ] {
            assert_eq!(
                class_of(path, "POST"),
                CommandClass::MotionInitiating,
                "{path} starts something; a delayed one must be refused (§5.8.1)"
            );
        }
        for path in [
            "/api/mount/slew/stop",
            "/api/camera/capture/abort",
            "/api/camera/liveview/stop",
        ] {
            assert_eq!(
                class_of(path, "POST"),
                CommandClass::Stopping,
                "{path} stops something; a late stop is safe and is never refused (§5.8.1)"
            );
        }
        for (path, method) in [
            ("/api/mount/connect", "POST"),
            ("/api/mount/disconnect", "POST"),
            ("/api/mount/unpark", "POST"),
            ("/api/camera/connect", "POST"),
            ("/api/camera/disconnect", "POST"),
            ("/api/camera/settings", "PUT"),
            ("/api/camera/fault/ack", "POST"),
            ("/api/camera/liveview/start", "POST"),
        ] {
            assert_eq!(
                class_of(path, method),
                CommandClass::Neutral,
                "{method} {path} starts nothing and stops nothing"
            );
        }
    }

    /// The class vocabulary reaches the wire, so a client can ask the node what it enforces
    /// instead of compiling a copy of this table into its own bundle.
    #[tokio::test]
    async fn info_publishes_the_command_class_policy_and_the_class_of_every_route() {
        let node = TestNode::authenticated("s3cret");
        let (_, body) = call(&node, "/api/system/info", Some("s3cret")).await;

        let goto = body["routes"]
            .as_array()
            .expect("routes")
            .iter()
            .find(|r| r["path"] == "/api/mount/goto")
            .expect("goto is declared");
        assert_eq!(goto["command"], "motion_initiating", "{goto}");

        let classes = body["command_classes"].as_array().expect("classes");
        assert_eq!(classes.len(), 6, "{classes:?}");
        let policy = |name: &str| {
            classes
                .iter()
                .find(|c| c["class"] == name)
                .unwrap_or_else(|| panic!("{name} is published"))
                .clone()
        };
        assert_eq!(policy("motion_initiating")["checks_staleness"], true);
        assert_eq!(policy("motion_initiating")["requires_envelope"], true);
        assert_eq!(policy("stopping")["checks_staleness"], false);
        assert_eq!(
            policy("stopping")["requires_envelope"],
            false,
            "a stopping route that could answer 422 is a stopping route that can fail to stop"
        );
        assert_eq!(policy("neutral")["checks_staleness"], false);
        assert_eq!(policy("neutral")["requires_envelope"], true);
        assert_eq!(policy("exempt")["requires_envelope"], false);

        // The budget the staleness check is measured against is already published; M1-T10 is what
        // makes it more than documentation.
        assert_eq!(body["server"]["max_command_age_ms"], 2000, "{body}");
    }

    /// Both sockets are declared, published and outside the bearer layer — all three, because any
    /// two of them without the third is a bug: undeclared means §8.2's table lies, unpublished
    /// means the PWA cannot discover it, and inside the layer means a browser can never connect.
    #[test]
    fn the_ws_routes_are_declared_separately_from_the_bearer_authenticated_surface() {
        let (_, bearer) = router();
        for path in ["/ws", "/ws/liveview"] {
            assert!(
                !bearer.iter().any(|r| r.path == path),
                "{path} must not be in the bearer-authenticated table (SDD §4.5: ticket, not \
                 header) — a browser cannot attach a header to a WebSocket upgrade"
            );
        }

        // The proxied stack socket belongs here for the same reason (M1-T14): the browser opens
        // it with a ticket because it cannot attach a header, so a declaration under `router()`
        // would answer 401 to every upgrade while looking like a working route to `curl`.
        assert!(
            !bearer.iter().any(|r| r.path.starts_with(crate::proxy::WS_PREFIX)),
            "the stacking server's sockets must not be behind the bearer layer either"
        );

        let (_, ws) = ws_router();
        assert_eq!(
            ws.iter().map(|r| r.path).collect::<Vec<_>>(),
            vec!["/ws", "/ws/liveview", "/stack/ws/{*rest}"],
            "§8.3(5)'s two sockets and the proxied stack socket, all ticket-authenticated"
        );
        for route in &ws {
            if route.path.starts_with(crate::proxy::WS_PREFIX) {
                // The proxied one is `ANY`/`pass_through`: the serving node classifies (ADR-07),
                // and this node declaring `read` would be it deciding a tier for a route it
                // cannot see.
                assert_eq!(route.method, "ANY");
                assert_eq!(route.meta.tier, Tier::PassThrough);
                continue;
            }
            assert_eq!(route.method, "GET");
            assert_eq!(route.meta.tier, Tier::Read);
            assert!(
                !route.meta.audit,
                "the audit record would fire on the handshake and report the upgrade's latency \
                 as the session's, which looks like information and is not"
            );
        }
    }

    #[tokio::test]
    async fn info_shows_the_authentication_posture_and_the_proxy_target() {
        let node = TestNode::authenticated("s3cret");
        let (_, body) = call(&node, "/api/system/info", Some("s3cret")).await;
        assert_eq!(body["server"]["auth_enforced"], true);
        assert_eq!(body["node"], "field");
        assert_eq!(
            body["config"]["stacking_server"]["upstream"],
            "http://192.168.1.100:8471"
        );
        // The fixture rewrites `mount.driver` and `camera.driver` to `simulator` (see
        // `test_support`): the example config selects skywatcher and gphoto2, which are right for
        // the operator and unbuildable on a machine with no telescope and no camera. What matters
        // here is that `info` reports the drivers the config actually named, whichever those are.
        assert_eq!(body["config"]["mount_driver"], "simulator");
        assert_eq!(body["config"]["camera_driver"], "simulator");
        assert!(body["drivers"].as_array().expect("list").is_empty());
    }

    /// The loopback exception is a degraded posture, so it has to be visible from the API rather
    /// than only from the startup log of a process nobody is watching.
    #[tokio::test]
    async fn an_open_loopback_node_serves_without_a_token_and_says_so() {
        let node = TestNode::open_loopback();
        let (status, body) = call(&node, "/api/system/info", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["server"]["auth_enforced"], false);
    }

    // --- the error envelope -----------------------------------------------------------

    #[test]
    fn api_failure_renders_the_status_its_code_demands() {
        use astroctl_core::error::ErrorCode;
        for (code, expected) in [
            (ErrorCode::Auth, 401),
            (ErrorCode::NotConnected, 409),
            (ErrorCode::DeviceTransport, 502),
            (ErrorCode::DiskFull, 507),
        ] {
            let response = ApiFailure(ApiError::new(code, "x")).into_response();
            assert_eq!(response.status().as_u16(), expected, "for {code}");
        }
    }
}
