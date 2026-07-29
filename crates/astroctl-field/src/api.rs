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
use crate::proxy::{self, StackProxy};
use crate::route_meta::{ApiRouter, RouteDecl, RouteMeta, Tier};
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
    /// Logging destination, for support questions.
    pub logging: LoggingInfo,
}

/// Wraps an [`ApiError`] so it can be returned from a handler.
///
/// `ApiError` itself cannot implement `IntoResponse`: it lives in `astroctl-core`, which sits
/// below the API layer and must not pull axum into every crate in the workspace (SDD §4.2).
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
        // ADR-07. Audited: a call that changes state on the other node is exactly as consequential
        // as one that changes state here, and this node cannot tell which is which — so it records
        // every one of them.
        .any(
            "/stack",
            RouteMeta::new(Tier::PassThrough, true),
            proxy::handler,
        )
        .any(
            "/stack/{*rest}",
            RouteMeta::new(Tier::PassThrough, true),
            proxy::handler,
        );

    let declarations = api.declarations();
    (api.into_router(), declarations)
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

/// `GET /api/system/health` — SDD §5.8.1.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    v: u16,
    status: NodeStatus,
    /// `null` when the volume cannot be interrogated — never a fabricated 0.0 (see
    /// [`crate::vitals::disk_free_gb`]).
    disk_free_gb: Option<f64>,
    clock_synced: bool,
    uptime_s: u64,
    versions: Versions,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        v: API_SCHEMA_VERSION,
        status: state.status.get(),
        disk_free_gb: vitals::disk_free_gb(&state.config.storage.sessions_dir),
        clock_synced: vitals::clock_synced(),
        uptime_s: state.uptime.seconds(),
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

/// Bind address and authentication posture.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    bind: String,
    /// `false` means this node is running under the SDD §4.5 loopback exception, unauthenticated.
    auth_enforced: bool,
    max_command_age_ms: u64,
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
        let state = state_with(node, declarations);
        let auth = Arc::clone(&state.auth);
        let app = with_auth(router.with_state(state), auth);

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
        let state = state_with(&node, declarations);
        state.status.set(NodeStatus::Ok);
        let auth = Arc::clone(&state.auth);
        let app = with_auth(router.with_state(state), auth);

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

        // Every route the router serves is declared — that is the §8.2 invariant, and the
        // declaration list is the only place a route can come from.
        assert_eq!(routes.len(), 4);
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
        assert_eq!(body["config"]["mount_driver"], "skywatcher");
        assert_eq!(body["config"]["camera_driver"], "gphoto2");
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
