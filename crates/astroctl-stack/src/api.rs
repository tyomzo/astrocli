//! The stacking server's HTTP surface — SDD §5.11.1, §8.1, §8.2.
//!
//! Middleware order is the field node's: bearer auth outside routing, route metadata per route.
//! The two binaries cannot share this code — ADD §5.6 rule 5 forbids them depending on each
//! other and `astroctl-core` must stay free of axum (SDD §4.2) — so it is deliberately
//! duplicated. See the M0-T05 result note.
//!
//! # What is not here yet
//!
//! `/ws`, the JSON status socket of SDD §5.11.1. `/ws/preview` landed with M1-T14 because the
//! preview pipeline gave it something to push; `/ws` still has no subscriber. The field node
//! republishes this node's health as `stack.status` by polling `/api/system/health` and
//! `/api/stacking/stats` (§4.3, USB-06), so a JSON socket here would be a socket that never
//! sends — and declaring one is how a route ends up in `/api/system/info` promising a stream
//! nobody feeds.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use astroctl_core::bus::{EventBus, EVENT_BUS_CAPACITY};
use astroctl_core::config::StackConfig;
use astroctl_core::error::ApiError;
use astroctl_core::event::{WorkerState, EVENT_SCHEMA_VERSION};
use astroctl_ipc::supervisor::WorkerHandle;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::{require_bearer, AuthPolicy};
use crate::ingest::{self, Ingest, MAX_UPLOAD_BYTES};
use crate::preview::{self, PreviewHub, PreviewQueue};
use crate::route_meta::{ApiRouter, RouteDecl, RouteMeta, Tier};
use crate::vitals::{self, Uptime};

/// Schema version of the `/api/system/*` response bodies (SDD §2).
const API_SCHEMA_VERSION: u16 = 1;

/// Node lifecycle as reported by `/api/system/health` (SDD §8.1).
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

/// The `starting` → `ok` transition of SDD §8.1.
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

    /// Declare the node started.
    pub fn set(&self, status: NodeStatus) {
        self.0.store(status.code(), Ordering::Relaxed);
    }
}

/// How the runtime was sized (SDD §7).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct RuntimeSizing {
    /// Worker threads the runtime was actually built with.
    pub worker_threads: usize,
    /// What `server.runtime_worker_threads` said; `null` means the SDD §7 default — one per core
    /// on this node, because the heavy compute lives in child processes with their own
    /// scheduling and there is nothing to reserve against.
    pub configured: Option<usize>,
    /// Cores visible to the process.
    pub available_cores: usize,
}

/// Where the node is logging.
#[derive(Clone, Debug, Serialize)]
pub struct LoggingInfo {
    /// Directory holding the rolling log file, or `null` if file logging is not running.
    pub dir: Option<String>,
    /// Why file logging is not running, if it is not.
    pub error: Option<String>,
}

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    /// The loaded, validated configuration (SDD §4.4).
    pub config: Arc<StackConfig>,
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
    /// Logging destination, for support questions.
    pub logging: LoggingInfo,
    /// The mirrored archive and its journal (SDD §5.11).
    pub ingest: Arc<Ingest>,
    /// The `/ws/preview` fan-out and the timestamp of the last preview (SDD §5.11.1, M1-T14).
    pub previews: Arc<PreviewHub>,
    /// The depth-1 slot the ingest handler drops a frame into on the ack path (SDD §5.8.3).
    pub preview_queue: Arc<PreviewQueue>,
    /// The compute worker's supervisor (SDD §5.12.3).
    ///
    /// Held as a handle, not an `Option`: [`astroctl_ipc::supervisor::spawn`] cannot fail and
    /// starts no process, so a running node always has a supervisor even when it has never had a
    /// worker. That is what lets `/api/system/health` report `stopped` — the honest idle state —
    /// rather than the `null` it answered while there was no supervisor at all.
    pub workers: WorkerHandle,
}

/// An [`ApiError`] on its way out as a response.
///
/// The wrapper exists because `ApiError` cannot implement `IntoResponse` itself — it lives in
/// `astroctl-core`, below the API layer, and SDD §4.2 keeps axum out of that crate. The field
/// node carries the identical type for the identical reason (ADD §5.6 rule 5).
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
pub fn router() -> (Router<AppState>, Vec<RouteDecl>) {
    let api = ApiRouter::<AppState>::new()
        .get("/api/system/health", RouteMeta::read(), health)
        .get("/api/system/info", RouteMeta::read(), info)
        // `Low`, not `Medium`: ingest changes state here but moves nothing and captures nothing,
        // and it is the field node's transfer agent that calls it, never an operator. Audited,
        // because the audit log is where "when did this node stop receiving frames" is answered.
        //
        // Not `BlockedForLlm` even though no LLM should ever call it: SDD §5.8.1 reserves that
        // tier for actions an *operator* may take and an agent may not (the e-stop). An agent
        // cannot usefully call ingest at all — it has no frame — so the tier that describes the
        // route honestly is the one that describes its consequence.
        .post_with_body_limit(
            "/api/ingest",
            RouteMeta::new(Tier::Low, true),
            MAX_UPLOAD_BYTES,
            ingest::ingest,
        )
        .get("/api/stacking/stats", RouteMeta::read(), ingest::stats)
        // The §5.11.1 pre-flight. GET also answers HEAD (axum drops the body), which is the form
        // the transfer agent sends before committing ~200 s of shaped link to a body the node
        // may already hold.
        .get(
            "/api/ingest/{session_id}/{frame_id}",
            RouteMeta::read(),
            ingest::preflight,
        )
        // §5.11.1's binary preview socket. Unlike the field node's two WS routes it sits *inside*
        // the bearer layer, and that difference is the whole of §4.5's reasoning: tickets exist
        // because a browser cannot put an `Authorization` header on an upgrade, and the only
        // client of this socket is the field node's proxy, which is not a browser. §4.5 says so
        // outright — "it has no need of a ticket".
        .get("/ws/preview", RouteMeta::read(), preview::upgrade);

    let declarations = api.declarations();
    (api.into_router(), declarations)
}

/// Apply the bearer-auth layer (SDD §4.5, §5.11.1 "the same bearer-token middleware as the field
/// node").
pub fn with_auth(router: Router, auth: Arc<AuthPolicy>) -> Router {
    router.layer(axum::middleware::from_fn_with_state(auth, require_bearer))
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// Version strings for `/api/system/health`.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Versions {
    astroctl: &'static str,
    api: u16,
    event: u16,
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

/// The `worker` object of SDD §5.11.1.
///
/// `state` and `restarts` are §5.11.1's two fields. The restart counter is the one §5.12.3
/// singles out — "a worker quietly restarting every few minutes is the failure mode most likely
/// to go unnoticed" — and it is what the field node forwards into `stack.status` for the same
/// reason.
#[derive(Debug, Serialize)]
pub struct WorkerHealth {
    state: WorkerState,
    restarts: u32,
}

/// `GET /api/system/health` — SDD §5.11.1.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    v: u16,
    status: NodeStatus,
    disk_free_gb: Option<f64>,
    clock_synced: bool,
    uptime_s: u64,
    versions: Versions,
    /// The supervisor's view of the compute worker.
    ///
    /// Still an `Option` in the schema, and now always `Some` on a running node: M1-T13 gave the
    /// node a supervisor, so "there is no supervisor" stopped being a state this binary can be
    /// in. The idle answer is `{state: "stopped", restarts: 0}`, which is a claim about a
    /// supervisor that *is* running and has no worker up — the distinction the earlier `null`
    /// existed to preserve, now carried by `WorkerState::Stopped` instead (SDD change note
    /// 1.16.0). The field is kept optional so a future node that genuinely has no worker — an
    /// ingest-only mirror — has a way to say so.
    worker: Option<WorkerHealth>,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let worker = preview::worker_health(Some(&state.workers)).map(|status| WorkerHealth {
        state: status.state,
        restarts: status.restarts,
    });
    Json(HealthResponse {
        v: API_SCHEMA_VERSION,
        status: state.status.get(),
        disk_free_gb: vitals::disk_free_gb(&state.config.storage.sessions_dir),
        clock_synced: vitals::clock_synced(),
        uptime_s: state.uptime.seconds(),
        versions: Versions::current(),
        worker,
    })
}

/// `GET /api/system/info`.
///
/// **Not in the SDD §5.11.1 route table**, but required by SDD §7: "Both binaries therefore size
/// the runtime explicitly from config … The chosen value is reported in `/api/system/info`."
/// §5.11.1 is the section that is incomplete; it has been amended in the same change set as this
/// code (SDD v1.7.1), per `docs/plan/tasks/README.md` rule 2.
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
    routes: Vec<RouteDecl>,
    tiers: Vec<TierPolicy>,
    event_bus_capacity: usize,
    event_bus_subscribers: usize,
}

/// Bind address and authentication posture.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    bind: String,
    /// `false` means this node is running under the SDD §4.5 loopback exception.
    auth_enforced: bool,
}

/// One tier of the shared vocabulary (SDD §8.2).
///
/// No `confirmation` field: PRD §8.2 gives this node no `llm` section — the LLM control layer
/// talks to the field node (ADR-10).
#[derive(Debug, Serialize)]
pub struct TierPolicy {
    tier: Tier,
    llm_callable: bool,
}

/// The parts of the configuration that answer "what is this node set up to do".
#[derive(Debug, Serialize)]
pub struct ConfigSummary {
    stacking_method: String,
    weight_mode: String,
    registration_method: String,
    live_approximation: bool,
    sessions_dir: String,
    export_dir: String,
    disk_warn_free_gb: f64,
    disk_critical_free_gb: f64,
    calibration_library_dir: String,
    workers: WorkersSummary,
    gpu: GpuSummary,
}

/// The supervised Python workers (ADR-13).
#[derive(Debug, Serialize)]
pub struct WorkersSummary {
    python_interpreter: String,
    compute_worker: String,
    ml_worker: String,
    job_timeout_seconds: u64,
}

/// GPU budget.
#[derive(Debug, Serialize)]
pub struct GpuSummary {
    enabled: bool,
    device: String,
    vram_budget_gb: f64,
}

async fn info(State(state): State<AppState>) -> Json<InfoResponse> {
    let config = &state.config;
    Json(InfoResponse {
        v: API_SCHEMA_VERSION,
        node: "stack",
        status: state.status.get(),
        versions: Versions::current(),
        runtime: state.runtime,
        server: ServerInfo {
            bind: format!("{}:{}", config.server.host, config.server.port),
            auth_enforced: state.auth.is_enforced(),
        },
        logging: state.logging.clone(),
        config: ConfigSummary {
            stacking_method: debug_name(&config.stacking.method),
            weight_mode: debug_name(&config.stacking.weight_mode),
            registration_method: debug_name(&config.stacking.registration_method),
            live_approximation: config.stacking.live_approximation,
            sessions_dir: config.storage.sessions_dir.display().to_string(),
            export_dir: config.stacking.export_dir.display().to_string(),
            disk_warn_free_gb: config.storage.disk_warn_free_gb,
            disk_critical_free_gb: config.storage.disk_critical_free_gb,
            calibration_library_dir: config.calibration.library_dir.display().to_string(),
            workers: WorkersSummary {
                python_interpreter: config.workers.python_interpreter.display().to_string(),
                compute_worker: config.workers.compute_worker.display().to_string(),
                ml_worker: config.workers.ml_worker.display().to_string(),
                job_timeout_seconds: config.workers.job_timeout_seconds,
            },
            gpu: GpuSummary {
                enabled: config.gpu.enabled,
                device: config.gpu.device.clone(),
                vram_budget_gb: config.gpu.vram_budget_gb,
            },
        },
        routes: state.routes.to_vec(),
        tiers: Tier::ALL
            .iter()
            .map(|&tier| TierPolicy {
                tier,
                llm_callable: tier.llm_callable(),
            })
            .collect(),
        event_bus_capacity: EVENT_BUS_CAPACITY,
        event_bus_subscribers: state.bus.subscriber_count(),
    })
}

/// Render a config enum by its `Debug` name, lowercased — see the field node's twin.
fn debug_name<T: std::fmt::Debug>(value: &T) -> String {
    format!("{value:?}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{app_for, TestNode};
    use astroctl_core::error::ErrorCode;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt as _;

    async fn call(node: &TestNode, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let app = app_for(node).await;

        let mut request = Request::builder().uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = app
            .router
            .oneshot(request.body(Body::empty()).expect("request builds"))
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

    #[tokio::test]
    async fn every_route_answers_401_with_the_auth_envelope_without_a_token() {
        let node = TestNode::authenticated("s3cret");
        for path in [
            "/api/system/health",
            "/api/system/info",
            "/api/stacking/stats",
            // A GET against the POST-only ingest route: still 401 rather than 405, because auth
            // runs outside routing. An unauthenticated caller learns nothing about the surface.
            "/api/ingest",
        ] {
            let (status, body) = call(&node, path, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "for {path}");
            assert_eq!(body["code"], "AUTH", "for {path}: {body}");
            assert_eq!(body["v"], 1, "for {path}");
        }
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused() {
        let node = TestNode::authenticated("s3cret");
        let (status, body) = call(&node, "/api/system/health", Some("hunter2")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "AUTH");
    }

    #[tokio::test]
    async fn health_reports_the_sdd_5_11_1_fields() {
        let node = TestNode::authenticated("s3cret");
        let (status, body) = call(&node, "/api/system/health", Some("s3cret")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["v"], 1);
        assert_eq!(body["status"], "starting");
        assert!(
            body["disk_free_gb"].as_f64().is_some_and(|gb| gb >= 0.0),
            "{body}"
        );
        assert!(body["clock_synced"].is_boolean(), "{body}");
        assert_eq!(body["versions"]["astroctl"], env!("CARGO_PKG_VERSION"));
        // §5.11.1's `worker` object, filled by M1-T13's supervisor. A node that has just started
        // reports `stopped` with no restarts — the honest state of an on-demand supervisor whose
        // first job has not arrived (§5.12.3). Asserting `ready` here would be asserting that a
        // Python process is running, which at this point in a node's life it is not.
        assert!(body.get("worker").is_some(), "the key is present: {body}");
        assert_eq!(body["worker"]["state"], "stopped", "{body}");
        assert_eq!(body["worker"]["restarts"], 0, "{body}");
    }

    /// The acceptance criterion, stack side: SDD §7 defaults this node to one worker per core.
    #[tokio::test]
    async fn info_reports_the_resolved_worker_thread_count() {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);

        let (_, body) = call(
            &TestNode::authenticated("s3cret"),
            "/api/system/info",
            Some("s3cret"),
        )
        .await;
        assert_eq!(body["runtime"]["worker_threads"], cores, "{body}");
        assert_eq!(body["runtime"]["configured"], Value::Null);
        assert_eq!(body["runtime"]["available_cores"], cores);

        let pinned = TestNode::authenticated("s3cret").with_worker_threads(Some(1));
        let (_, body) = call(&pinned, "/api/system/info", Some("s3cret")).await;
        assert_eq!(body["runtime"]["worker_threads"], 1, "{body}");
        assert_eq!(body["runtime"]["configured"], 1);
    }

    #[tokio::test]
    async fn info_publishes_the_declared_route_table_and_the_tier_vocabulary() {
        let node = TestNode::authenticated("s3cret");
        let (_, body) = call(&node, "/api/system/info", Some("s3cret")).await;

        assert_eq!(body["node"], "stack");
        let routes = body["routes"].as_array().expect("routes is a list");
        // 6 = health, info, ingest, stats, the §5.11.1 pre-flight, and `/ws/preview`.
        assert_eq!(routes.len(), 6);
        // The preview socket is declared like any other route, which is what puts it inside the
        // bearer layer — §4.5's ticket exception is for browsers, and this socket has none.
        assert!(
            routes.iter().any(|r| r["path"] == "/ws/preview"),
            "the preview socket is declared: {body}"
        );
        let ingest = routes
            .iter()
            .find(|r| r["path"] == "/api/ingest")
            .expect("ingest is declared");
        assert_eq!(ingest["method"], "POST");
        assert_eq!(ingest["tier"], "low");
        assert_eq!(ingest["audit"], true, "ingest is the audited route");
        assert!(routes
            .iter()
            .filter(|r| r["path"] != "/api/ingest")
            .all(|r| r["tier"] == "read" && r["audit"] == false));

        let tiers = body["tiers"].as_array().expect("tiers is a list");
        assert_eq!(tiers.len(), 6);
        assert_eq!(tiers[0]["tier"], "read");
        assert_eq!(tiers[0]["llm_callable"], true);
    }

    #[tokio::test]
    async fn info_summarizes_the_stacking_configuration() {
        let node = TestNode::authenticated("s3cret");
        let (_, body) = call(&node, "/api/system/info", Some("s3cret")).await;
        assert_eq!(body["config"]["stacking_method"], "sigmaclip");
        assert_eq!(body["config"]["export_dir"], "/data/astro/stacks");
        assert_eq!(body["config"]["gpu"]["enabled"], true);
        assert_eq!(body["server"]["auth_enforced"], true);
    }

    /// The statuses ingest actually answers with (SDD §4.2's table, §5.11.2's three refusals).
    /// The wrapper is the only thing that turns a code into a status, so a wrong mapping here
    /// would send the transfer agent's state machine the wrong way (SDD §5.10.1).
    #[test]
    fn the_error_wrapper_maps_each_ingest_code_to_its_status() {
        for (code, expected) in [
            (ErrorCode::ChecksumMismatch, 422),
            (ErrorCode::Validation, 422),
            (ErrorCode::FrameIdConflict, 409),
            (ErrorCode::DiskFull, 507),
            (ErrorCode::Internal, 500),
        ] {
            let response = ApiFailure(ApiError::new(code, "x")).into_response();
            assert_eq!(response.status().as_u16(), expected, "for {code}");
        }
    }

    #[tokio::test]
    async fn an_open_loopback_node_serves_without_a_token_and_says_so() {
        let node = TestNode::open_loopback();
        let (status, body) = call(&node, "/api/system/info", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["server"]["auth_enforced"], false);
    }
}
