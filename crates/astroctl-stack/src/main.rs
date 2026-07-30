//! The stacking-server binary: frame ingest, the calibration index, job control, the rebuild
//! manager, preview and export.
//!
//! Per ADD §5.6 rule 5 it never depends on `astroctl-field`. The two nodes share
//! `astroctl-core`, `astroctl-ipc` and the HTTP contract of SDD §5.11.1 — and, in this
//! milestone, a deliberately duplicated copy of the HTTP scaffolding, because rule 5 forbids a
//! shared crate above `astroctl-core` and `astroctl-core` must stay free of axum (SDD §4.2).
//!
//! # Startup sequence (SDD §8.1)
//!
//! ```text
//! config load + validate   →  a bad key is a startup error naming it, never a default (§4.4)
//! auth check               →  SEC-01: no token + a non-loopback bind address refuses to start
//! tracing init             →  console + rolling file under `server.log_dir`
//! runtime built            →  explicitly sized from `server.runtime_worker_threads` (§7);
//!                             one per core here, because the heavy compute lives in child
//!                             processes with their own scheduling
//! API up, health `starting`
//! watchdogs on
//! health `ok`
//! ```
//!
//! §8.1 is written for the field node; the steps it names between the auth check and the API
//! coming up (frame store, driver registry, safety wrapper) have stack-node counterparts. The
//! session mirror is one of them and is opened there, before the socket: a node that cannot
//! record an ingest cannot honour the contract of SDD §5.11, and one that accepts frames it
//! cannot account for is worse than one that refuses to start. The worker supervisor is the
//! other and arrives with M1-T13. **Workers are not spawned at boot**: SDD §5.12.3 supervises
//! them as on-demand child processes, which is the same principle as the field node never
//! connecting hardware at startup.
//!
//! # Shutdown (SDD §7)
//!
//! Stop accepting, stop the watchdogs, flush the event log, exit. The field node's "finish the
//! in-flight download, leave tracking alone" asymmetry has no counterpart here; an ingest in
//! flight completes because `with_graceful_shutdown` waits for in-flight requests, and the
//! write-ahead ordering of ADD §9.2 means anything not yet acked is simply re-uploaded (SDD
//! §5.10.1).

mod api;
mod auth;
mod cli;
mod ingest;
mod journal;
mod mirror;
mod preview;
mod route_meta;
mod telemetry;
#[cfg(test)]
mod test_support;
mod vitals;
mod watchdog;

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use astroctl_core::bus::{EventBus, JsonlSink};
use astroctl_core::config::{load_stack_config, StackConfig};
use astroctl_core::event::Alert;

use crate::api::{AppState, LoggingInfo, NodeStatus, RuntimeSizing, StatusCell};
use crate::auth::AuthPolicy;
use crate::cli::Invocation;
use crate::vitals::Uptime;

/// How long the shutdown path waits for the event log to flush before exiting anyway.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

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
            println!("astroctl-stack {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Invocation::Invalid(reason) => {
            eprintln!("astroctl-stack: {reason}\n\n{}", cli::USAGE);
            return ExitCode::FAILURE;
        }
    };

    // --- 1. config load and validate (SDD §8.1, §4.4) --------------------------------------
    let config = match load_stack_config(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("astroctl-stack: {error}");
            return ExitCode::FAILURE;
        }
    };

    // --- 2. auth check (SDD §4.5, SEC-01) ---------------------------------------------------
    let policy = match resolve_auth(&config) {
        Ok(policy) => policy,
        Err(refusal) => {
            eprintln!("astroctl-stack: {refusal}");
            return ExitCode::FAILURE;
        }
    };

    // --- 3. tracing ------------------------------------------------------------------------
    let telemetry = telemetry::init(
        config.server.log_level,
        &config.server.log_dir,
        "astroctl-stack",
    );
    telemetry.report();

    // --- 4. the explicitly sized runtime (SDD §7) -------------------------------------------
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
        "tokio runtime sized (SDD §7: one per core by default — the heavy compute is in child \
         processes with their own scheduling)"
    );

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(sizing.worker_threads)
        .thread_name("astroctl-stack")
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

    match runtime.block_on(serve(config, policy, sizing, logging, uptime)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "the stacking server stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// SEC-01, resolved before anything opens a socket.
fn resolve_auth(config: &StackConfig) -> Result<AuthPolicy, auth::StartupRefusal> {
    let token = config.auth_token().ok();
    let bind: IpAddr = config
        .server
        .host
        .parse()
        // Unreachable — the config validator rejects a non-IP `server.host`. If it were reached,
        // "not loopback" is the reading that makes the refusal stricter rather than weaker.
        .unwrap_or_else(|_| IpAddr::from([0, 0, 0, 0]));
    AuthPolicy::resolve(
        config.server.auth_token_env.name(),
        token.as_ref().map(astroctl_core::config::Secret::expose),
        bind,
    )
}

/// Everything from "API up" to "exit", on the sized runtime.
async fn serve(
    config: Arc<StackConfig>,
    policy: AuthPolicy,
    sizing: RuntimeSizing,
    logging: LoggingInfo,
    uptime: Uptime,
) -> Result<(), Box<dyn std::error::Error>> {
    let bus = EventBus::new();

    let events_path = config.server.log_dir.join("events.jsonl");
    let sink = match JsonlSink::open(&bus, &events_path).await {
        Ok(sink) => {
            tracing::info!(path = %events_path.display(), "event log open");
            Some(sink.spawn())
        }
        Err(error) => {
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

    // --- 4b. the session mirror and its journal (SDD §5.11.3) ---------------------------------
    let mirror = mirror::SessionMirror::open(&config.storage.sessions_dir).await?;
    let swept = mirror.sweep_temporaries().await;
    if swept > 0 {
        tracing::warn!(
            count = swept,
            "removed temporaries left by interrupted uploads; the frames they belonged to were \
             never acked and will be re-sent (SDD §5.10.3)"
        );
    }
    tracing::info!(path = %mirror.root().display(), "session archive open");

    // --- 4c. the worker supervisor and the preview pipeline (SDD §5.12.3, §5.11.1) -------------
    //
    // `spawn` starts **no process**: §5.12.3 supervises workers as on-demand children, so the
    // first submitted job is what brings one up and a node with nothing to compute holds no
    // Python interpreter and no GPU context. That is the same principle as the field node not
    // connecting hardware at startup, and it is why this line is cheap enough to run before the
    // socket is bound.
    let workers = astroctl_ipc::supervisor::spawn(&config.workers, &bus);
    let previews = Arc::new(preview::PreviewHub::new());
    let preview_queue = Arc::new(preview::PreviewQueue::new());
    // The pipeline is handed the *only* `WorkerHandle` this node keeps, so dropping it at
    // shutdown is what stops the supervisor — see the teardown below.
    let preview_pipeline = preview::spawn(
        Arc::clone(&preview_queue),
        Arc::clone(&previews),
        workers.clone(),
        bus.clone(),
    );

    let status = Arc::new(StatusCell::starting());
    let (router, declarations) = api::router();
    let state = AppState {
        bus: bus.clone(),
        status: Arc::clone(&status),
        uptime,
        runtime: sizing,
        auth: Arc::clone(&auth),
        routes: declarations.into(),
        logging,
        config: Arc::clone(&config),
        ingest: Arc::new(ingest::Ingest::new(mirror)),
        previews,
        preview_queue,
        workers,
    };
    let app = api::with_auth(router.with_state(state), auth);

    // --- 5. API up, health `starting` (SDD §8.1) ---------------------------------------------
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
    tracing::info!(%addr, auth_enforced, "API listening");

    // `into_make_service_with_connect_info` so ingest can record *who* uploaded a frame in the
    // journal (SDD §5.11.3 "received frames, timestamps, source"). It costs one extension per
    // connection and is the only way to get the peer address into a handler.
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await
    });

    // --- 6. watchdogs on (SDD §8.1) ----------------------------------------------------------
    let watchdog = tokio::spawn(watchdog::run(bus.clone(), config.storage.clone(), uptime));

    // --- 7. health `ok` ----------------------------------------------------------------------
    status.set(NodeStatus::Ok);
    bus.publish(Alert::info(
        "NODE_STARTED",
        format!("stacking server {} is ready", env!("CARGO_PKG_VERSION")),
    ));
    tracing::info!("stacking server started");

    let outcome = server.await;

    // --- shutdown, in the SDD §7 order -------------------------------------------------------
    // 1. stop accepting — done: `with_graceful_shutdown` returned, in-flight ingests finished.
    // 2. stop the watchdogs.
    watchdog.abort();
    let _ = watchdog.await;
    // 3. worker supervisor teardown.
    //
    // The pipeline task holds an `EventBus` clone (it publishes `STACK_PREVIEW_FAILED`) and the
    // node's only `WorkerHandle`; aborting and joining it drops both. The supervisor stops when
    // its last handle goes — `astroctl-ipc`'s contract — and *it* holds a bus clone too, for its
    // `WORKER_*` alerts. `spawn` returns no join handle, so this cannot be awaited directly; what
    // makes it correct anyway is step 4: dropping the last sender wakes the supervisor
    // immediately, and the flush below is an `await`, so the runtime polls the woken supervisor
    // to completion long before the timeout. A worker process still running is killed by
    // `kill_on_drop`, which is why no Python child outlives this either.
    preview_pipeline.abort().await;
    // 4. flush the event log: dropping every `EventBus` handle closes the sink's subscriber.
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
    tracing::info!("stacking server stopped");

    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(Box::new(error)),
        Err(error) => Err(Box::new(error)),
    }
}

/// Resolve on SIGTERM (systemd's stop signal) or SIGINT (a terminal).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::error!(%error, "cannot install the SIGTERM handler");
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
