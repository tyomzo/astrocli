//! The field-node binary: the axum application, the WebSocket hub (M1), the `/stack/*` reverse
//! proxy and PWA serving (M0-T06).
//!
//! Per ADD §5.6 rule 5 it never depends on `astroctl-stack`; the two share only
//! `astroctl-core`/`astroctl-ipc` and the HTTP contract of SDD §5.11.1.
//!
//! # Startup sequence (SDD §8.1)
//!
//! ```text
//! config load + validate   →  a bad key is a startup error naming it, never a default (§4.4)
//! auth check               →  SEC-01: no token + a non-loopback bind address refuses to start
//! TLS materials            →  SEC-05: a `server.tls` block that will not load refuses to start
//! tracing init             →  console + rolling file under `server.log_dir`
//! runtime built            →  explicitly sized from `server.runtime_worker_threads` (§7)
//! API up, health `starting`
//! watchdogs on
//! health `ok`
//! ```
//!
//! §8.1's "frame store open/create session → registry builds drivers → safety wrapper" steps sit
//! between the auth check and the API coming up, and all three are now implemented — see
//! [`open_session`] for the first and [`build_mount`], which does the last two in one expression so
//! nothing can hold an unwrapped driver. **Hardware is never connected at startup** either way —
//! that is an explicit operator action (§8.1), so nothing in this milestone moves a motor or opens
//! a shutter by booting.
//!
//! # Shutdown (SDD §7)
//!
//! `stop accepting API → abort live view → finish an in-flight download (bounded) → leave
//! tracking alone → flush the session log → exit`. In M0 the middle two steps have nothing to
//! act on; the first, the last and — most importantly — the *deliberate omission* are
//! implemented. Tracking is never stopped on shutdown: restarting the service mid-session must
//! not stop the mount, whereas a half-downloaded frame is a lost frame. That asymmetry is a
//! design decision, and the place it can be silently lost is here.

mod api;
mod auth;
mod camera;
mod cli;
mod command;
mod liveview;
mod mount;
mod orchestrator;
mod proxy;
mod pwa;
mod route_meta;
mod stack_status;
mod telemetry;
#[cfg(test)]
mod test_support;
mod ticket;
mod tls;
mod transfer;
mod vitals;
mod watchdog;
mod ws;

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use astroctl_core::bus::{EventBus, JsonlSink};
use astroctl_core::config::{load_field_config, FieldConfig};
use astroctl_core::event::Alert;

use crate::api::{AppState, LoggingInfo, NodeStatus, RuntimeSizing, StatusCell};
use crate::auth::AuthPolicy;
use crate::cli::Invocation;
use crate::proxy::StackProxy;
use crate::vitals::Uptime;

/// How long the shutdown path waits for the event log to flush before exiting anyway.
///
/// The acceptance criterion is a clean exit within 2 s at idle; a disk that cannot absorb a few
/// buffered lines in one second is not going to absorb them in five, and an operator power-cycling
/// a Pi that will not die is worse than a truncated telemetry log.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// How long shutdown waits for a capture to finish before abandoning it (SDD §7 step 3).
///
/// The reference body's download is measured at ~2 s and the simulator's default profile is the
/// same; the budget is `camera.timeouts.download_seconds` shaped by what a *restart* can afford
/// rather than what a camera can take, so it is a fixed, generous multiple of the measured figure
/// rather than the config value. A frame that has not landed in ten seconds is not landing.
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// The session name a node opens at startup when it is not continuing one.
///
/// There is no operator-facing key for this in PRD §8.1 and no target to name it after until the
/// catalog arrives (Phase 2a, PLN-03/04), so the session is identified by its date — which is what
/// [`open_or_create_session`](astroctl_session::FrameStore::open_or_create_session) prefixes anyway,
/// giving `2026-07-30_session`. Naming it after tonight's target is the Phase 2a change.
const DEFAULT_SESSION_SLUG: &str = "session";

fn main() -> ExitCode {
    let uptime = Uptime::started_now();

    let config_path = match cli::parse(
        std::env::args_os().skip(1),
        std::env::var_os(cli::CONFIG_PATH_ENV),
    ) {
        Invocation::Run { config } => config,
        Invocation::Help => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Invocation::Version => {
            println!("astroctl-field {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Invocation::Invalid(reason) => {
            eprintln!("astroctl-field: {reason}\n\n{}", cli::USAGE);
            return ExitCode::FAILURE;
        }
    };

    // --- 1. config load and validate (SDD §8.1, §4.4) --------------------------------------
    let config = match load_field_config(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("astroctl-field: {error}");
            return ExitCode::FAILURE;
        }
    };

    // --- 2. auth check (SDD §4.5, SEC-01) ---------------------------------------------------
    let policy = match resolve_auth(&config) {
        Ok(policy) => policy,
        Err(refusal) => {
            eprintln!("astroctl-field: {refusal}");
            return ExitCode::FAILURE;
        }
    };

    // --- 3. TLS materials (SEC-05) -----------------------------------------------------------
    // Before the runtime, the log files or the listener: an operator who configured TLS and got
    // plain HTTP has a PWA that will not install and nothing in the logs pointing at why, so the
    // one thing this must never do is continue. No block at all is a supported mode — `localhost`
    // development and the M0-T08 container harness both run on it (ADD §4, SEC-09).
    let tls = match config.server.tls.as_ref().map(tls::load).transpose() {
        Ok(tls) => tls,
        Err(error) => {
            eprintln!("astroctl-field: {error}");
            return ExitCode::FAILURE;
        }
    };

    // --- 4. tracing ------------------------------------------------------------------------
    let telemetry = telemetry::init(
        config.server.log_level,
        &config.server.log_dir,
        "astroctl-field",
    );
    telemetry.report();

    // --- 5. the explicitly sized runtime (SDD §7) -------------------------------------------
    let available_cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let sizing = RuntimeSizing {
        worker_threads: config.server.resolved_worker_threads(available_cores),
        configured: config.server.runtime_worker_threads,
        available_cores,
    };
    tracing::info!(
        worker_threads = sizing.worker_threads,
        configured = ?sizing.configured,
        available_cores = sizing.available_cores,
        "tokio runtime sized (SDD §7: never one-per-core on the field node — the camera thread \
         and the decode pool need the rest)"
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(sizing.worker_threads)
        .thread_name("astroctl-field")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "cannot build the tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    let logging = LoggingInfo {
        dir: telemetry.file_dir().map(|d| d.display().to_string()),
        error: telemetry.error().map(str::to_owned),
    };

    match runtime.block_on(serve(config, policy, tls, sizing, logging, uptime)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the field node stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// SEC-01, resolved before anything opens a socket.
fn resolve_auth(config: &FieldConfig) -> Result<AuthPolicy, auth::StartupRefusal> {
    let token = config.auth_token().ok();
    let bind: IpAddr = config.server.host.parse().unwrap_or_else(|_| {
        // The config validator (SDD §4.4) rejects a non-IP `server.host`, so this is
        // unreachable. If it were ever reached, the safe reading of an address we cannot parse
        // is "not loopback", which makes the refusal below stricter rather than weaker.
        IpAddr::from([0, 0, 0, 0])
    });
    AuthPolicy::resolve(
        config.server.auth_token_env.name(),
        token.as_ref().map(astroctl_core::config::Secret::expose),
        bind,
    )
}

/// Everything from "API up" to "exit", on the sized runtime.
async fn serve(
    config: Arc<FieldConfig>,
    policy: AuthPolicy,
    tls: Option<tls::Materials>,
    sizing: RuntimeSizing,
    logging: LoggingInfo,
    uptime: Uptime,
) -> Result<(), Box<dyn std::error::Error>> {
    let bus = EventBus::new();

    // The event log is opened before the API comes up so a bad path fails startup rather than
    // silently producing no record of the night (SDD §4.3 sink, §6).
    let events_path = config.server.log_dir.join("events.jsonl");
    let sink = match JsonlSink::open(&bus, &events_path).await {
        Ok(sink) => {
            tracing::info!(path = %events_path.display(), "event log open");
            Some(sink.spawn())
        }
        Err(error) => {
            // Same call as `telemetry`: observability failures degrade, they do not refuse.
            tracing::warn!(path = %events_path.display(), %error, "cannot open the event log");
            None
        }
    };

    let auth = Arc::new(policy);
    let auth_enforced = auth.is_enforced();
    if !auth_enforced {
        tracing::warn!(
            "no authentication token is configured: this node is bound to loopback and serves \
             every request unauthenticated (SDD §4.5). Set the environment variable named by \
             `server.auth_token_env` before exposing it on a network."
        );
    }
    let status = Arc::new(StatusCell::starting());

    // --- 5a. frame store open, session open or created (SDD §8.1, §5.5) -----------------------
    //
    // Before the drivers, because §8.1 puts it there and because the order is the one that
    // matters at 2 a.m.: a node that cannot open its session directory must fail at startup,
    // where the operator is looking, rather than at the first capture of the night.
    let store = Arc::new(open_session(&config).await?);

    // --- 5b. drivers built, nothing connected (SDD §8.1) --------------------------------------
    //
    // "The registry builds drivers, no connect": switching the field node on must not produce
    // motion, so connecting stays an operator action and a failure here is a configuration
    // error rather than a hardware one.
    let device = build_mount(&config, bus.clone())?;
    let mount = Arc::new(mount::MountFacade::new(device, bus.clone(), &config.mount));
    let camera = Arc::new(camera::CameraFacade::new(
        build_camera(
            &config,
            astroctl_drivers::simulator::CameraProfile::default(),
        )?,
        Arc::clone(&store),
        bus.clone(),
    ));
    // --- 5c. the transfer queue, opened and recovered (SDD §5.10.3, §8.1) ---------------------
    //
    // Before the API for the reason §8.1 orders everything else that way: a node whose queue
    // directory is unwritable must fail where the operator is looking, not at the first frame of
    // the night. The subscriber is taken here rather than inside so that the agent cannot miss a
    // `frame.saved` published between its construction and its first `recv`.
    // The store is taken *from the camera facade* rather than from the local variable, even though
    // they are the same `Arc`: the reconciliation has to scan the store the captures actually
    // write through, and naming it that way makes that a property of the call rather than of two
    // bindings happening to agree.
    let (transfer, transfer_agent) =
        transfer::start(&config, camera.store(), bus.clone(), bus.subscribe()).await?;

    let snapshots = Arc::new(ws::SnapshotStore::new());
    // Built before the API because a handler may be serving `/ws/liveview` the instant the
    // listener binds, and an `Option` filled in later would be a `None` the upgrade would have to
    // have an answer for. The hub is idle until something starts the camera stream, which is what
    // makes constructing it this early free.
    let liveview = Arc::new(liveview::LiveViewHub::new());

    // One proxy, shared by the `/stack/*` routes and the `stack.status` republisher, so a
    // deployment cannot end up with the routes pointing at one upstream and the poller at another.
    //
    // The token is handed to it for one purpose: the outbound half of a WebSocket upgrade, where
    // the browser could not attach a credential and there is therefore nothing to forward (SDD
    // §4.5, §5.8.1). Plain proxied requests still relay the operator's own header (ADR-07).
    let stack_proxy = Arc::new(StackProxy::new(
        &config.stacking_server,
        config
            .auth_token()
            .ok()
            .as_ref()
            .map(astroctl_core::config::Secret::expose),
    ));

    let (router, declarations) = api::router();
    let (ws_router, ws_declarations) = api::ws_router();
    let routes: Vec<_> = declarations.into_iter().chain(ws_declarations).collect();
    let state = AppState {
        proxy: Arc::clone(&stack_proxy),
        bus: bus.clone(),
        status: Arc::clone(&status),
        uptime,
        runtime: sizing,
        auth: Arc::clone(&auth),
        routes: routes.into(),
        logging,
        config: Arc::clone(&config),
        certificate: tls.as_ref().map(tls::Materials::status),
        mount: Arc::clone(&mount),
        camera: Arc::clone(&camera),
        transfer: Arc::clone(&transfer),
        tickets: Arc::new(ticket::TicketStore::new()),
        // SDD §5.8.1's staleness budget is read once, here, and never again: a handler that
        // re-read the config would be a second place for the number to come from.
        commands: Arc::new(command::CommandLedger::new(Duration::from_millis(
            config.server.max_command_age_ms,
        ))),
        snapshots: Arc::clone(&snapshots),
        liveview: Arc::clone(&liveview),
    };
    let app = assemble(router, ws_router, state);

    // --- 6. API up, health `starting` (SDD §8.1) ---------------------------------------------
    let addr = SocketAddr::new(
        config.server.host.parse().map_err(|e| {
            format!(
                "`server.host` is not an IP address: {} ({e})",
                config.server.host
            )
        })?,
        config.server.port,
    );
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("cannot bind {addr}: {e}"))?;

    // One `axum::serve`, two listeners. The TLS listener satisfies `axum::serve::Listener`
    // (see [`tls::TlsListener`]) precisely so the graceful-shutdown ordering of SDD §7 below is
    // written once — a second server stack for HTTPS would be a second place for that ordering
    // to be got wrong, and the omission it encodes is the easiest thing in this file to lose.
    let server = match tls {
        Some(materials) => {
            let certificate = materials.status();
            tracing::info!(
                %addr,
                auth_enforced,
                scheme = "https",
                cert_expires_at = %certificate.expires_at_rfc3339(),
                cert_days_remaining = certificate.days_remaining(chrono::Utc::now()),
                "API listening"
            );
            if certificate.is_warning(chrono::Utc::now()) {
                // Logged as well as reported by SEC-07's health field, because a certificate that
                // is already inside the window at boot is the case where nobody is watching
                // `/api/system/health` yet.
                tracing::warn!(
                    cert_expires_at = %certificate.expires_at_rfc3339(),
                    "the TLS certificate is close to expiry; renew it before the secure context \
                     lapses and the installed app stops working (SEC-07)"
                );
            }
            let listener = materials.into_listener(listener);
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown_signal())
                    .await
            })
        }
        None => {
            // A supported mode, not a legacy branch: `localhost` is a secure context to a
            // browser, and two containers on a bridge network (M0-T08) have no name to certify.
            tracing::info!(%addr, auth_enforced, scheme = "http", "API listening");
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown_signal())
                    .await
            })
        }
    };

    // --- 7. watchdogs on (SDD §8.1) ----------------------------------------------------------
    let watchdog = tokio::spawn(watchdog::run(bus.clone(), config.storage.clone(), uptime));
    // The 1 Hz position poll (MNT-02) and the snapshot store that feeds every `/ws` connect.
    //
    // Both hold an `EventSubscriber` rather than an `EventBus` clone. That is load-bearing at
    // shutdown: step 5 below closes the sink by dropping every *sender*, and a task holding one
    // would keep the channel open and stall the flush for its whole timeout. The poll task is
    // the exception that proves it — it publishes, so it does hold a bus clone, and it is
    // therefore aborted before `drop(bus)` like the watchdog.
    let poll = tokio::spawn(mount::poll(Arc::clone(&mount)));
    // `camera.status` on change and every 60 s (§4.3). Same accounting as the position poll: it
    // publishes, so it holds a bus clone and is aborted before `drop(bus)`.
    let camera_poll = tokio::spawn(camera::poll(Arc::clone(&camera)));
    let snapshot_task = tokio::spawn(ws::maintain_snapshot(
        Arc::clone(&snapshots),
        bus.subscribe(),
    ));
    // The preview pipeline (SDD §5.7): `frame.saved` → decode on the blocking pool → cache →
    // push on `/ws/liveview` → `capture.progress: preview_ready`. Its worker publishes, so it
    // holds a bus clone and is stopped before `drop(bus)` like the two polls.
    let previews = liveview::spawn_previews(
        Arc::clone(&liveview),
        Arc::clone(&camera),
        bus.clone(),
        bus.subscribe(),
        astroctl_pipeline::PreviewParams::default(),
    );
    // `stack.status` (§4.3, USB-06): the field node polls the stacking server's health and stats
    // and republishes them, so the PWA has one event source and never talks to the stack node
    // directly (ADR-07). `None` when no stacking server is configured — see the module docs on
    // why that publishes nothing rather than `offline`. It publishes, so it holds a bus clone and
    // is aborted before `drop(bus)` like the polls.
    let stack_status = stack_status::spawn(Arc::clone(&stack_proxy), bus.clone());

    // --- 8. health `ok` ----------------------------------------------------------------------
    status.set(NodeStatus::Ok);
    bus.publish(Alert::info(
        "NODE_STARTED",
        format!("field node {} is ready", env!("CARGO_PKG_VERSION")),
    ));
    tracing::info!("field node started");

    let outcome = server.await;

    // --- shutdown, in the SDD §7 order -------------------------------------------------------
    // 1. stop accepting — done: `with_graceful_shutdown` returned, in-flight requests finished.
    // 2. abort live view (SDD §7). The camera stream first, then the preview pipeline.
    //
    // `abort` and not `stop`: shutdown must not wait on the *camera* answering. A wedged body
    // that never returns from `stop_live_view` would hold this sequence, and every step after it
    // — including flushing the night's event log — would be behind a device that has already
    // stopped talking. The driver's own `disconnect`/drop ends the sensor loop; what this node
    // owes is that no task of *its* outlives the process.
    liveview.abort().await;
    // The preview worker publishes `capture.progress: preview_ready`, so it holds a bus sender
    // and must be gone before step 5 drops the rest — the same accounting as the two polls
    // below, and the same one the facade and the capture task needed.
    previews.abort().await;
    // The transfer agent, for the same reason and with one addition of its own: an upload in
    // flight right now is **abandoned, not awaited**. §5.10.3 makes that safe by design — the row
    // stays `uploading`, the next startup returns it to `queued`, and ingest deduplicates the
    // re-send — so unlike the capture in step 3, there is nothing here worth paying a budget for.
    // Its uploader publishes `transfer.acked`, so it holds a bus handle like the preview worker.
    if let Some(agent) = transfer_agent {
        agent.abort().await;
    }
    watchdog.abort();
    let _ = watchdog.await;
    // The poll task publishes, so it holds a bus sender and must stop before step 5 drops the
    // rest. Aborting it mid-poll is safe: HAL rule 3 says dropping a device future never stops
    // hardware, so a mount that was slewing keeps slewing — which is exactly what step 4 wants.
    poll.abort();
    let _ = poll.await;
    // Aborted rather than awaited. It holds only a receiver and the snapshot store is in-memory
    // state with nothing to flush, so there is no reason to wait for it — and waiting for it was
    // a hang: `await`ing a task that ends on `Recv::Closed` makes shutdown depend on every
    // *sender* having been dropped first, which is a much stronger claim than it looks.
    snapshot_task.abort();
    let _ = snapshot_task.await;
    // The camera status poll publishes too, so it stops here with the others.
    camera_poll.abort();
    let _ = camera_poll.await;
    // The stack.status republisher, same accounting again: it publishes, so it must not outlive
    // the bus. Aborting mid-poll is safe — the poll is a GET, and abandoning one loses nothing
    // but a health reading the node is about to stop caring about.
    if let Some(task) = stack_status {
        task.abort();
        let _ = task.await;
    }
    // 3. finish an in-flight download (bounded). The one step of this sequence that deliberately
    //    *waits*: the exposure has already been spent and a half-downloaded frame is a lost frame,
    //    so the node pays up to `CAPTURE_DRAIN_TIMEOUT` to keep it. Past that it gives up, for the
    //    same reason step 5 has a timeout — a Pi that will not die is worse than one lost frame.
    camera.finish_inflight(CAPTURE_DRAIN_TIMEOUT).await;
    // 4. tracking is deliberately NOT stopped (see the module docs).
    // 5. flush the session log: dropping every `EventBus` handle closes the sink's subscriber,
    //    which is what makes the flush complete rather than merely likely.
    //
    // The invariant this protects is "no `EventBus` handle outlives this point", because a
    // handle is a broadcast *sender* and the sink's subscriber only closes when the last one
    // goes. Two of them are easy to miss, and both were: the facade holds one, and so does the
    // task waiting on an in-flight goto — the case that matters, since a two-minute slew is
    // exactly when a service restart lands. Missing either costs a full `FLUSH_TIMEOUT` and the
    // tail of the night's event log.
    //
    // M1-T08 added two more of the same shape, which is why step 3 above both waits and then
    // guarantees the task is gone: the camera facade holds a handle, and so does the capture task
    // — and a capture in flight is, like a slew, exactly when a restart is most likely to land.
    //
    // Aborting the motion task does not stop the mount (HAL rule 3), which is what step 4 wants.
    mount.abort_inflight();
    drop(mount);
    drop(camera);
    // M1-T11 added a fifth: the transfer facade holds a bus handle through its queue, which the
    // status route needs for as long as the API is up and must not hold for a moment longer.
    drop(transfer);
    drop(store);
    drop(bus);
    if let Some(sink) = sink {
        match tokio::time::timeout(FLUSH_TIMEOUT, sink).await {
            Ok(Ok(Ok(stats))) => tracing::info!(
                written = stats.written,
                lagged = stats.lagged,
                dropped = stats.dropped,
                "event log flushed"
            ),
            Ok(Ok(Err(error))) => tracing::warn!(%error, "the event log ended with an I/O error"),
            Ok(Err(error)) => tracing::warn!(%error, "the event log task did not join"),
            Err(_) => tracing::warn!(
                timeout_s = FLUSH_TIMEOUT.as_secs(),
                "the event log did not flush in time; exiting anyway"
            ),
        }
    }
    tracing::info!("field node stopped");

    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(Box::new(error)),
        Err(error) => Err(Box::new(error)),
    }
}

/// The whole HTTP surface: the authenticated API of [`api::router`], with the unauthenticated PWA
/// merged onto it (M0-T06).
///
/// The merge order is the design. `with_auth` layers the API's routes; `merge` then adds a router
/// whose only member is the SPA fallback, which is therefore *outside* that layer — a browser
/// cannot put an `Authorization` header on the navigation request that loads an app shell. See
/// [`pwa`] for what that costs and how the cost is paid.
///
/// It is a function rather than four lines inside [`serve`] so that the tests drive exactly what
/// the binary binds. A test that assembles its own approximation of the app is a test that keeps
/// passing after the real assembly changes.
/// The three subtrees, and the reason there are three.
///
/// * `router` — the bearer-authenticated API (SDD §4.5).
/// * `ws_router` — `/ws`, authenticated by a single-use ticket instead, because a browser cannot
///   put an `Authorization` header on a WebSocket handshake. It is merged *after* `with_auth`
///   has been applied, so the bearer layer never sees it. That is the whole mechanism: there is
///   no exemption inside the middleware to extend by accident, only a router it was never given.
/// * `pwa::router` — the unauthenticated app shell, for the same reason, one layer up: a browser
///   cannot put a header on the navigation request that loads the shell either.
fn assemble(
    router: axum::Router<AppState>,
    ws_router: axum::Router<AppState>,
    state: AppState,
) -> axum::Router {
    let auth = Arc::clone(&state.auth);
    let deployment = state.config.server.deployment_label.clone();
    api::with_auth(api::with_state(router, state.clone()), Arc::clone(&auth))
        .merge(api::with_state(ws_router, state))
        .merge(pwa::router(auth, deployment))
}

/// Build the configured mount driver through the HAL registry and wrap it in the safety layer
/// (SDD §5.1, §5.4, §8.1; HAL-07, ADR-11).
///
/// This function is the one place in the workspace that names a concrete driver — ADD §5.6 rule
/// 1 puts that job on a binary, because the registry type lives in `astroctl-hal` and hal cannot
/// depend on drivers without a cycle. Everything above the HAL, this file included from the next
/// line onwards, holds `Arc<dyn MountDevice>`.
///
/// The registry is built and consulted rather than matched on directly so that
/// `mount.driver: skywatcher` fails with the registry's own "no such driver, available: …"
/// message naming what this build actually has, instead of a match arm nobody updated.
///
/// # The wrap happens here, and nowhere else
///
/// ADR-11 says the facade the API sees **is** the safety wrapper. This is the seam that makes it
/// literally true: the driver `create_mount` returns is moved into a `SafeMount` in the same
/// expression and no caller ever sees the raw handle. Nothing downstream can be given an
/// unwrapped mount, because nothing downstream is ever handed one — which is what makes MNT-15's
/// "for all callers (UI, REST API, LLM agent)" a property of the code rather than a convention.
fn build_mount(
    config: &FieldConfig,
    bus: EventBus,
) -> Result<Arc<astroctl_safety::SafeMount>, Box<dyn std::error::Error>> {
    let mut registry = astroctl_hal::registry::DriverRegistry::new();
    // Fault and profile knobs live on the factory rather than in config (SDD §9): a failure
    // scenario is a value a test writes down, not something an operator can switch on in
    // production YAML.
    //
    // `ASTROCTL_SIM_FAULTS` does not weaken that, and the distinction is worth being precise
    // about because it looks at first like the config key §9 forbids. It is still a constructor
    // parameter — the plan is parsed once, here, and moved into the factory before any device
    // exists; there is no setter and nothing can change it later. What the variable buys is a
    // *process boundary*: M1-T16's end-to-end suite drives two containers over HTTP, so the only
    // way it can say "give me a mount whose link dies two seconds after connect" is to say it to
    // the thing that starts the container. An in-process test needs none of this and should keep
    // using `with_faults` directly.
    //
    // It is an environment variable rather than a config key on purpose:
    //
    //   * `FieldConfig` is `deny_unknown_fields` at every level (SDD §4.4), so a fault key would
    //     be a real schema extension shipped to every deployment — and `/api/system/info`
    //     publishes the config, which would put fault injection in a production node's API.
    //   * A node started with faults is a node whose behaviour is a lie, so it says so at WARN
    //     on the way up. An operator who finds that line in a journal knows immediately why the
    //     mount "broke".
    //   * A malformed value is fatal. The failure mode this guards against is a scenario that
    //     asks for a fault, silently gets a mount that behaves, and passes.
    let faults = match std::env::var("ASTROCTL_SIM_FAULTS") {
        Ok(spec) => {
            let plan = astroctl_drivers::simulator::FaultPlan::from_spec(&spec)
                .map_err(|e| format!("ASTROCTL_SIM_FAULTS is not a fault plan: {e}"))?;
            if !plan.is_empty() {
                tracing::warn!(
                    %spec,
                    faults = plan.len(),
                    "ASTROCTL_SIM_FAULTS is set: the simulated mount will fail on purpose. \
                     This node is a test harness, not a telescope."
                );
            }
            plan
        }
        Err(_) => astroctl_drivers::simulator::FaultPlan::none(),
    };
    registry
        .register_mount(
            astroctl_drivers::simulator::SimulatorMountFactory::new().with_faults(faults),
        )
        .map_err(|e| format!("cannot register the simulator mount driver: {e}"))?;

    let driver = registry
        .create_mount(config.mount.driver.as_str(), &config.mount)
        .map_err(|e| {
            format!(
                "cannot build the mount driver named by `mount.driver` ({}): {e}",
                config.mount.driver.as_str()
            )
        })?;

    Ok(Arc::new(astroctl_safety::SafeMount::from_config(
        driver, config, bus,
    )))
}

/// Open the frame store and the session the node will write tonight's frames into (SDD §8.1, §5.5).
///
/// # `open_current` first, and why the order is not a preference
///
/// A node that restarts mid-night must continue the session it was in, not start a second one
/// beside it: the frame counter is per session (SDD §5.5 note 1), so a fresh session directory
/// would restart the numbering while the old directory still holds `light_00001`. Two frames with
/// one id in one night is exactly what REL-04's "a crash never reuses an id" forbids, and the only
/// thing that prevents it is asking `CURRENT` first.
///
/// Creating one is the *fallback*, which is also why the failure to create is a startup error: a
/// node whose session directory is unwritable cannot capture at all, and finding that out at the
/// first exposure of the night is finding it out in a field, in the dark.
async fn open_session(
    config: &FieldConfig,
) -> Result<astroctl_session::FrameStore, Box<dyn std::error::Error>> {
    let store = astroctl_session::FrameStore::open(&config.storage)
        .await
        .map_err(|e| {
            format!(
                "cannot open the session store at {}: {e}",
                config.storage.sessions_dir.display()
            )
        })?;

    let session = match store.open_current().await {
        Ok(Some(session)) => {
            tracing::info!(session = %session.id(), "continuing the session `CURRENT` points at");
            session
        }
        Ok(None) => store
            .open_or_create_session(astroctl_session::store::NewSession {
                slug: DEFAULT_SESSION_SLUG.to_owned(),
                target: None,
                equipment: astroctl_session::Equipment::from(&config.equipment),
            })
            .await
            .map_err(|e| format!("cannot create a session: {e}"))?,
        Err(error) => {
            return Err(format!("cannot open the session `CURRENT` points at: {error}").into())
        }
    };

    tracing::info!(
        session = %session.id(),
        dir = %session.dir().display(),
        "session open"
    );
    Ok(store)
}

/// Build the configured camera driver through the HAL registry (SDD §5.1, §8.1; HAL-07).
///
/// [`build_mount`]'s twin, and the second of the two places in the workspace that names a concrete
/// driver (ADD §5.6 rule 1). There is no safety wrapper on the way out, and that asymmetry is the
/// design rather than an omission: ADR-11 wraps the mount because a mount can drive a telescope into
/// a pier, and nothing a camera is asked to do can move anything.
///
/// `profile` is a parameter for the reason SDD §9 gives for the fault plans: a simulator's timings
/// are a value a *test* writes down, not something an operator can switch on in production YAML.
/// The binary passes the measured R10 profile; the test fixtures pass `CameraProfile::fast()`, and
/// still come through this function so they exercise the registry lookup the operator's
/// `camera.driver` actually goes through.
fn build_camera(
    config: &FieldConfig,
    profile: astroctl_drivers::simulator::CameraProfile,
) -> Result<Arc<dyn astroctl_hal::camera::Camera>, Box<dyn std::error::Error>> {
    let mut registry = astroctl_hal::registry::DriverRegistry::new();
    registry
        .register_camera(
            astroctl_drivers::simulator::SimulatorCameraFactory::new().with_profile(profile),
        )
        .map_err(|e| format!("cannot register the simulator camera driver: {e}"))?;

    registry
        .create_camera(config.camera.driver.as_str(), &config.camera)
        .map_err(|e| {
            format!(
                "cannot build the camera driver named by `camera.driver` ({}): {e}",
                config.camera.driver.as_str()
            )
            .into()
        })
}

/// Resolve on SIGTERM (systemd's stop signal) or SIGINT (a terminal).
///
/// Both are honoured because both mean "stop": SIGTERM in production, Ctrl-C on the bench, and a
/// binary that only handles one of them behaves differently in the two places it runs.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::error!(%error, "cannot install the SIGTERM handler");
                // Without a SIGTERM handler the process would still die on the signal, just
                // without the graceful path — so wait for SIGINT and let SIGTERM be abrupt.
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!(signal = "SIGINT", "shutting down");
                return;
            }
        };
        let signal_name = tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!(%error, "the SIGINT handler failed");
                }
                "SIGINT"
            }
        };
        tracing::info!(signal = signal_name, "shutting down");
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(signal = "SIGINT", "shutting down");
    }
}
