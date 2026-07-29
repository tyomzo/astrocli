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
//! tracing init             →  console + rolling file under `server.log_dir`
//! runtime built            →  explicitly sized from `server.runtime_worker_threads` (§7)
//! API up, health `starting`
//! watchdogs on
//! health `ok`
//! ```
//!
//! §8.1's "frame store open/create session → registry builds drivers → safety wrapper" steps sit
//! between the auth check and the API coming up. They are absent here because the crates that
//! implement them are empty until M1-T01/M1-T07, not because the order is different. **Hardware
//! is never connected at startup** either way — that is an explicit operator action (§8.1), so
//! nothing in this milestone moves a motor by booting.
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
mod cli;
mod proxy;
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

    // --- 3. tracing ------------------------------------------------------------------------
    let telemetry = telemetry::init(
        config.server.log_level,
        &config.server.log_dir,
        "astroctl-field",
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

    match runtime.block_on(serve(config, policy, sizing, logging, uptime)) {
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
    let (router, declarations) = api::router();
    let state = AppState {
        proxy: Arc::new(StackProxy::new(&config.stacking_server)),
        bus: bus.clone(),
        status: Arc::clone(&status),
        uptime,
        runtime: sizing,
        auth: Arc::clone(&auth),
        routes: declarations.into(),
        logging,
        config: Arc::clone(&config),
    };
    // The M0-T06 seam: the embedded PWA and its SPA fallback merge onto this router. They stay
    // outside `with_auth` because a browser cannot put an `Authorization` header on the
    // navigation request that loads the app shell.
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

    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
    });

    // --- 6. watchdogs on (SDD §8.1) ----------------------------------------------------------
    let watchdog = tokio::spawn(watchdog::run(bus.clone(), config.storage.clone(), uptime));

    // --- 7. health `ok` ----------------------------------------------------------------------
    status.set(NodeStatus::Ok);
    bus.publish(Alert::info(
        "NODE_STARTED",
        format!("field node {} is ready", env!("CARGO_PKG_VERSION")),
    ));
    tracing::info!("field node started");

    let outcome = server.await;

    // --- shutdown, in the SDD §7 order -------------------------------------------------------
    // 1. stop accepting — done: `with_graceful_shutdown` returned, in-flight requests finished.
    // 2. abort live view — M1-T09; the watchdogs are what this milestone has to stop.
    watchdog.abort();
    let _ = watchdog.await;
    // 3. finish an in-flight download — M1-T08; nothing owns a camera yet.
    // 4. tracking is deliberately NOT stopped (see the module docs).
    // 5. flush the session log: dropping every `EventBus` handle closes the sink's subscriber,
    //    which is what makes the flush complete rather than merely likely.
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
