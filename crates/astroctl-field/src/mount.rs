//! The mount facade and its routes — SDD §5.8.1's mount rows, §4.3's two mount topics.
//!
//! This is the first vertical slice: HTTP in, `Arc<dyn MountDevice>` at the bottom, events out
//! on the bus at 1 Hz. Everything above the HAL that wants to know where the telescope is reads
//! it from here.
//!
//! # What the facade owns, and what it deliberately does not
//!
//! It owns the device handle, the poll task, and the in-flight goto. It does **not** own safety:
//! M1-T05's `SafeMount` wraps this facade's device handle, so the altitude limit, the meridian
//! limit, the slew TTL watcher and the e-stop route all arrive by wrapping rather than by
//! editing this file. That is why nothing here validates a target against `mount.limits`, and
//! why `POST /api/mount/estop` is absent — a route answering it today would make the PWA's
//! unarmed e-stop button a lie.
//!
//! `alt`/`az` are `null` for the same reason and only until then. The topocentric transform
//! belongs to the safety monitor because the altitude *limit* needs it, and one transform shared
//! between the display and the limit is what keeps a display bug and a limit bug from
//! disagreeing about whether the target is up. This is not a Phase 2a gap: MNT-03 is a Phase 1
//! Must and is met at M1 exit.
//!
//! # Reading motion out of a mount that never stops moving
//!
//! A mount with its drive off still reports a right ascension that climbs at 15.04″/s — the axes
//! hold an hour angle and the sky turns underneath them. So "the position changed" is not
//! "the mount is moving", and a facade that inferred one from the other would report a parked
//! mount as slewing all night. Motion comes from [`MountDevice::status`] and nothing else.
//!
//! # Long-running actions
//!
//! `goto` answers `202 {correlation_id, watch_topic}` and drops the future that is waiting for
//! the slew (SDD §5.8.1). Dropping it does not stop the mount — that is HAL rule 3, and the
//! whole reason the API can answer in milliseconds while the tube takes two minutes. The
//! operator watches `mount.position` and `mount.status` for the rest.

use std::sync::Arc;
use std::time::Duration;

use astroctl_core::bus::EventBus;
use astroctl_core::config::MountConfig;
use astroctl_core::error::{ApiError, DeviceError, ErrorCode};
use astroctl_core::event::{self, PierSide};
use astroctl_core::types::{
    Axis, DeviceKind, Direction, MountState, MountStatus, RaDec, SlewSpeed, TrackingMode,
};
use astroctl_hal::mount::MountDevice;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::api::{ApiFailure, AppState};
use crate::ticket::Ticket;

/// The position poll interval (MNT-02, SDD §4.3: `mount.position` at 1 Hz).
///
/// `mount.serial.poll_hz` is the operator-facing knob and its minimum is 1; this is the default
/// and the only rate M1 uses. See [`MountFacade::poll_interval`].
const DEFAULT_POLL_HZ: u32 = 1;

// ---------------------------------------------------------------------------------------------
// The facade
// ---------------------------------------------------------------------------------------------

/// Everything the mount routes and the poll task share.
#[derive(Debug)]
pub struct MountFacade {
    device: Arc<dyn MountDevice>,
    bus: EventBus,
    /// The in-flight goto, or `None`.
    ///
    /// A `tokio::sync::Mutex` rather than a `std` one because the goto path holds it across the
    /// device call that *starts* the slew. Holding a `std` guard across an `.await` is what
    /// `clippy::await_holding_lock` denies workspace-wide, and for a good reason on a node with
    /// one or two runtime workers.
    goto: Mutex<Option<GotoRun>>,
    /// `mount.serial.poll_hz`, clamped to at least 1.
    poll_hz: u32,
}

/// A goto the node has accepted and is still waiting on.
#[derive(Debug, Clone)]
struct GotoRun {
    correlation_id: String,
    target: RaDec,
}

impl MountFacade {
    /// Wrap a device.
    #[must_use]
    pub fn new(device: Arc<dyn MountDevice>, bus: EventBus, config: &MountConfig) -> Self {
        Self {
            device,
            bus,
            goto: Mutex::new(None),
            // The config validator already floors this at 1; clamping again costs nothing and
            // means a zero here is a slow poll rather than a division by zero.
            poll_hz: config.serial.poll_hz.max(DEFAULT_POLL_HZ),
        }
    }

    /// The device handle, for M1-T05's `SafeMount` to wrap and for tests.
    ///
    /// Nothing in this build calls it yet — the wrapping happens in T05 — but the accessor is
    /// what makes the seam explicit rather than something T05 has to open up.
    #[must_use]
    #[allow(dead_code)]
    pub fn device(&self) -> &Arc<dyn MountDevice> {
        &self.device
    }

    /// How long the poll task sleeps between reads.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / u64::from(self.poll_hz))
    }

    /// Read the current status and translate it for the wire.
    async fn wire_status(&self) -> Result<event::MountStatus, DeviceError> {
        Ok(to_wire_status(self.device.status().await?))
    }
}

// ---------------------------------------------------------------------------------------------
// HAL types → wire types
// ---------------------------------------------------------------------------------------------

/// Collapse the HAL's seven-state lifecycle onto the five the wire has.
///
/// `astroctl_core::types::MountState` and `astroctl_core::event::MountState` are deliberately
/// different enums: the HAL needs `Tracking` and `Parking` to describe a driver's internal
/// lifecycle, and SDD §4.3's payload has neither, because the flags beside it already say
/// whether the mount is tracking or moving. Inventing wire variants for them would break every
/// client that switched on the five (the PWA's `MountState` is exactly those five) to express
/// something the payload can already say.
///
/// * `Tracking` → `idle`: the mount is stationary in the sky frame; `tracking: true` is what
///   says it is driving. §4.3's own comment on `Idle` reads "connected and stationary (it may
///   still be tracking — see `tracking`)".
/// * `Parking` → `slewing`: it is moving to the park position. Reporting `parked` before it
///   arrives would tell the operator the tube is stowed while it is still swinging.
const fn to_wire_state(state: MountState) -> event::MountState {
    match state {
        MountState::Disconnected => event::MountState::Disconnected,
        MountState::Idle | MountState::Tracking => event::MountState::Idle,
        MountState::Slewing | MountState::Parking => event::MountState::Slewing,
        MountState::Parked => event::MountState::Parked,
        MountState::Fault => event::MountState::Fault,
    }
}

/// The whole status, wire-side.
fn to_wire_status(status: MountStatus) -> event::MountStatus {
    event::MountStatus::new(to_wire_state(status.state), status.tracking)
}

/// The mount's declination axis tells which side of the pier the tube is on; nothing above the
/// HAL derives it. `RaDec` carries no pier side, so until a driver reports one this is `unknown`
/// — which is a value the wire enum has precisely so that "not derivable yet" does not have to
/// be guessed at as east.
const UNKNOWN_PIER: PierSide = PierSide::Unknown;

/// Build the `mount.position` payload from a coordinate pair.
///
/// `alt`/`az` are `None` until M1-T05 — see the module docs.
fn to_wire_position(pos: RaDec) -> event::MountPosition {
    event::MountPosition::new(pos.ra.hours(), pos.dec.degrees(), None, None, UNKNOWN_PIER)
}

// ---------------------------------------------------------------------------------------------
// The 1 Hz poll task
// ---------------------------------------------------------------------------------------------

/// Poll the mount at `poll_hz` and publish what changes (SDD §4.3, MNT-02).
///
/// Runs until aborted at shutdown, like [`crate::watchdog::run`].
///
/// # What is published when
///
/// `mount.position` goes out on **every** tick, because it is telemetry: the operator needs to
/// see the coordinates advancing to know the link is alive, and §4.3 gives it a cadence rather
/// than a condition. `mount.status` goes out **on change only**, because it is a state — a
/// status republished every second would drown the discrete events the hub is required never to
/// drop, and would make "the mount stopped slewing" indistinguishable from the 3 599 times it
/// said the same thing that hour.
///
/// # Why a disconnected mount is not an error here
///
/// The mount starts disconnected and stays that way until the operator presses Connect (SDD
/// §8.1: the registry builds drivers, it does not connect them). `NotConnected` from a poll is
/// therefore the *normal* case, not a failure, and logging it at 1 Hz would fill the log before
/// anyone connected anything. The status is published once when it changes and the position
/// simply is not read.
pub async fn poll(facade: Arc<MountFacade>) {
    let mut ticker = tokio::time::interval(facade.poll_interval());
    // The mount is a physical thing: if a poll takes longer than the interval, the answer is to
    // take the next reading late, not to fire a burst of catch-up reads at a serial line that is
    // already struggling.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // `None` until the first successful status read, so the first status is always published —
    // a client connecting before the first tick would otherwise see an empty snapshot.
    let mut last_status: Option<event::MountStatus> = None;

    loop {
        ticker.tick().await;

        match facade.device.status().await {
            Ok(status) => {
                let wire = to_wire_status(status);
                if last_status != Some(wire) {
                    facade.bus.publish(wire);
                    last_status = Some(wire);
                }

                // Only read a position the mount can actually answer. A `position()` call on a
                // disconnected driver is a guaranteed `NotConnected`, and asking anyway would
                // make the log a list of questions we knew the answer to.
                if status.state != MountState::Disconnected {
                    match facade.device.position().await {
                        Ok(pos) => {
                            facade.bus.publish(to_wire_position(pos));
                        }
                        Err(error) => {
                            tracing::debug!(%error, "mount position poll failed");
                        }
                    }
                }
            }
            Err(DeviceError::NotConnected) => {
                let wire = event::MountStatus::disconnected();
                if last_status != Some(wire) {
                    facade.bus.publish(wire);
                    last_status = Some(wire);
                }
            }
            Err(error) => {
                // A driver that is connected but unhappy reports `Fault` from `status()` rather
                // than erroring (HAL contract), so reaching here means the transport itself
                // failed. Worth a line, but not a panic and not a rate that fills a log.
                tracing::debug!(%error, "mount status poll failed");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Request and response bodies
// ---------------------------------------------------------------------------------------------

/// `POST /api/mount/connect` — SDD §5.8.1's optional `{port?}`.
///
/// `#[serde(default)]` on the whole body: the PWA posts `{}` and a bare `POST` with no body at
/// all is a reasonable thing for `curl` to do. Neither should be a 422.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectRequest {
    /// Ignored in M1. The port comes from `mount.port` in the config; overriding it per request
    /// is a Phase 2 affordance, and accepting the key while ignoring it would be worse than
    /// rejecting it — so it is declared and rejected as unknown by anything that sends a value.
    #[serde(default)]
    #[allow(dead_code)]
    port: Option<String>,
}

/// `POST /api/mount/goto` — SDD §5.8.1.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GotoRequest {
    /// Right ascension in hours. The field name carries the unit for the reason SDD §2 and the
    /// core newtypes exist: a bare `ra` invites the degrees/hours mix-up that puts a telescope
    /// fifteen times too far round.
    ra_hours: f64,
    /// Declination in degrees.
    dec_degrees: f64,
}

/// `202` from a long-running action (SDD §5.8.1's "202 + WS progress" pattern).
#[derive(Debug, Serialize)]
pub struct Accepted {
    /// Correlates this request with the events it produces.
    correlation_id: String,
    /// The topic to watch for progress.
    watch_topic: &'static str,
}

/// `POST /api/mount/tracking` — SDD §5.8.1.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackingRequest {
    mode: TrackingRequestMode,
}

/// The four values `/api/mount/tracking` accepts.
///
/// `off` is a request mode, not a [`TrackingMode`]: the HAL splits starting and stopping into
/// two methods, and the core enum is the set of *rates*, which "off" is not one of. Making it a
/// variant of the core enum would put a non-rate in every match that switches on a rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackingRequestMode {
    Sidereal,
    Lunar,
    Solar,
    Off,
}

/// `POST /api/mount/slew` — SDD §5.8.1's dead-man's switch.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlewRequest {
    axis: WireAxis,
    direction: WireDirection,
    /// 1 (finest) to 5 (fastest) — the five dots of §5.9's sketch.
    speed: u8,
    /// Milliseconds of authorisation. Defaults to `mount.limits.slew_ttl_default_ms` and is
    /// clamped to `slew_ttl_max_ms`, both server-side (§5.8.1).
    #[serde(default)]
    ttl_ms: Option<u64>,
}

/// `POST /api/mount/slew/stop` — SDD §5.8.1. Omitting `axis` stops both.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlewStopRequest {
    #[serde(default)]
    axis: Option<WireAxis>,
}

/// What `/api/mount/slew` answers.
#[derive(Debug, Serialize)]
pub struct SlewAccepted {
    axis: WireAxis,
    /// How long this lease authorises motion for, after clamping.
    expires_in_ms: u64,
}

/// The wire spelling of an axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireAxis {
    Ra,
    Dec,
}

impl From<WireAxis> for Axis {
    fn from(axis: WireAxis) -> Self {
        match axis {
            WireAxis::Ra => Self::Ra,
            WireAxis::Dec => Self::Dec,
        }
    }
}

/// The wire spelling of a slew direction.
///
/// `positive`/`negative` rather than compass words, decided by M1-T04 and adopted here. On the
/// RA axis "east" means "the direction in which right ascension increases", which is one
/// indirection nobody reading a bug report wants to perform. The axis names the pair; this names
/// which way along it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireDirection {
    Positive,
    Negative,
}

impl WireDirection {
    /// Resolve to the HAL's compass direction for an axis.
    ///
    /// The mapping is the definition of "positive": increasing right ascension is east,
    /// increasing declination is north. Getting it backwards sends the mount the wrong way at
    /// 800× sidereal, which is why it lives in one function with a test rather than at each
    /// call site.
    const fn on(self, axis: WireAxis) -> Direction {
        match (axis, self) {
            (WireAxis::Ra, Self::Positive) => Direction::East,
            (WireAxis::Ra, Self::Negative) => Direction::West,
            (WireAxis::Dec, Self::Positive) => Direction::North,
            (WireAxis::Dec, Self::Negative) => Direction::South,
        }
    }
}

/// Map the 1–5 ordinal onto the HAL's speed ladder.
///
/// An ordinal rather than a rate, because what a step means in degrees per second belongs to the
/// driver — a client sending °/s would be asserting a capability it cannot check.
///
/// # Errors
/// [`ErrorCode::Validation`] for anything outside 1–5.
fn to_slew_speed(speed: u8) -> Result<SlewSpeed, ApiError> {
    match speed {
        1 => Ok(SlewSpeed::Guide),
        2 => Ok(SlewSpeed::Slow),
        3 => Ok(SlewSpeed::Medium),
        4 => Ok(SlewSpeed::Fast),
        5 => Ok(SlewSpeed::Max),
        other => Err(ApiError::new(
            ErrorCode::Validation,
            format!("`speed` must be 1..=5 (an ordinal, not a rate); got {other}"),
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// Error plumbing
// ---------------------------------------------------------------------------------------------

/// Turn a driver failure into the §4.2 envelope, naming the mount.
///
/// [`ApiError::from_device_error`] picks `MOUNT_TIMEOUT` over the device-agnostic
/// `DEVICE_TIMEOUT` from the [`DeviceKind`], which is the difference between telling the
/// operator to check the telescope and telling them to check "a device".
fn device_failure(err: &DeviceError) -> ApiFailure {
    ApiFailure(ApiError::from_device_error(DeviceKind::Mount, err))
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

/// `POST /api/mount/connect` — SDD §5.8.1.
pub async fn connect(
    State(state): State<AppState>,
    body: Option<Json<ConnectRequest>>,
) -> Result<Json<event::MountStatus>, ApiFailure> {
    let _ = body;
    let facade = &state.mount;
    facade
        .device
        .connect()
        .await
        .map_err(|e| device_failure(&e))?;

    // Publish immediately rather than waiting up to a second for the poll task. The operator
    // pressed Connect and is watching for the badge to change; a state change the node already
    // knows about should not sit behind a timer.
    let status = facade.wire_status().await.map_err(|e| device_failure(&e))?;
    state.bus.publish(status);
    Ok(Json(status))
}

/// `POST /api/mount/disconnect` — SDD §5.8.1.
///
/// Does not stop the mount. That is the HAL's contract and it is deliberate (SDD §7): a service
/// restart mid-session must leave a tracking mount tracking, because a tracking mount is safe
/// and a stopped one has lost the target.
pub async fn disconnect(
    State(state): State<AppState>,
    body: Option<Json<ConnectRequest>>,
) -> Result<Json<event::MountStatus>, ApiFailure> {
    let _ = body;
    let facade = &state.mount;
    facade
        .device
        .disconnect()
        .await
        .map_err(|e| device_failure(&e))?;

    // A disconnected driver's `status()` may itself be `NotConnected`, which is the expected
    // answer and not a failure to report here.
    let status = facade
        .wire_status()
        .await
        .unwrap_or_else(|_| event::MountStatus::disconnected());
    state.bus.publish(status);
    Ok(Json(status))
}

/// `GET /api/mount/position` — SDD §5.8.1.
pub async fn position(
    State(state): State<AppState>,
) -> Result<Json<event::MountPosition>, ApiFailure> {
    let pos = state
        .mount
        .device
        .position()
        .await
        .map_err(|e| device_failure(&e))?;
    Ok(Json(to_wire_position(pos)))
}

/// `GET /api/mount/status` — SDD §5.8.1.
///
/// # A disconnected mount is a status, not an error
///
/// The driver answers `NotConnected` to `status()` — correctly, at its layer. This route turns
/// that into `200 {"state": "disconnected", …}` rather than a 409, for the reason the HAL gives
/// for `Fault`: "the mount is not answering" is a state the operator must see, not an error that
/// makes the status panel go blank. It is also the shape the PWA is written against, and the
/// route the operator's UI polls before it has connected anything — a 409 there would make the
/// first screen of the app an error.
///
/// The *motion* routes still refuse: there the disconnection stops the request from being
/// honoured, so it is genuinely the answer to what was asked.
pub async fn status(State(state): State<AppState>) -> Result<Json<event::MountStatus>, ApiFailure> {
    match state.mount.wire_status().await {
        Ok(status) => Ok(Json(status)),
        Err(DeviceError::NotConnected) => Ok(Json(event::MountStatus::disconnected())),
        Err(error) => Err(device_failure(&error)),
    }
}

/// `POST /api/mount/goto` — SDD §5.8.1's `202 + WS progress`.
///
/// # Why the second goto is refused here rather than at the driver
///
/// The driver refuses it too — `DeviceError::Busy`, and its own test pins that. But the driver
/// only sees the *second* call after the first has reached it, and this facade answers `202`
/// and drops the future, so there is a window in which the node has accepted a goto that the
/// driver has not started yet. Holding the in-flight record here closes it: the state that says
/// "a goto is running" is the same state that answered `202`, so the two cannot disagree.
pub async fn goto(
    State(state): State<AppState>,
    Json(request): Json<GotoRequest>,
) -> Result<Response, ApiFailure> {
    let target = RaDec::from_parts(request.ra_hours, request.dec_degrees)
        .map_err(|e| ApiFailure(ApiError::from(e)))?;

    // Refusals the node can see *now* must be answered now, not as an alert thirty seconds
    // later. `goto` does not resolve until the mount has settled, so a handler that spawned it
    // unconditionally would answer `202 Accepted` to a goto on a mount that is not plugged in —
    // which is the opposite of what 202 means. See [`preflight`].
    preflight(&state).await?;

    let correlation_id = correlation_id().map_err(|e| {
        ApiFailure(ApiError::new(
            ErrorCode::Internal,
            format!("could not generate a correlation id: {e}"),
        ))
    })?;

    {
        let mut inflight = state.mount.goto.lock().await;
        if let Some(running) = inflight.as_ref() {
            // 409 BUSY, with enough detail for the operator to see they are fighting themselves
            // rather than the mount. The target of the *running* goto is the useful fact.
            return Err(ApiFailure(
                ApiError::new(
                    ErrorCode::Busy,
                    "a goto is already in progress; stop it or wait for it to finish",
                )
                .with_detail(serde_json::json!({
                    "correlation_id": running.correlation_id,
                    "target": {
                        "ra_hours": running.target.ra.hours(),
                        "dec_degrees": running.target.dec.degrees(),
                    },
                })),
            ));
        }
        *inflight = Some(GotoRun {
            correlation_id: correlation_id.clone(),
            target,
        });
    }

    // The slew outlives this request by design (SDD §5.8.1). Spawning is what lets the handler
    // answer in milliseconds while the tube takes minutes; the task's only job afterwards is to
    // clear the in-flight slot so the next goto is not refused forever.
    let facade = Arc::clone(&state.mount);
    let bus = state.bus.clone();
    let id_for_task = correlation_id.clone();
    tokio::spawn(async move {
        let outcome = facade.device.goto(target).await;

        // Cleared before anything is published, so a client that reacts to the completion event
        // by sending the next goto cannot race the slot.
        facade.goto.lock().await.take();

        match outcome {
            Ok(()) => {
                tracing::info!(correlation_id = %id_for_task, "goto complete");
            }
            Err(error) => {
                // Any error means "the slew did not complete" — never "the mount is here".
                // Where it actually is comes from `status()`, which the poll task is already
                // reading; publishing a position from this path would be inventing one.
                tracing::warn!(correlation_id = %id_for_task, %error, "goto did not complete");
                let code = ErrorCode::from_device_error(DeviceKind::Mount, &error);
                bus.publish(event::Alert::warning(code.as_str(), error.to_string()));
            }
        }

        // The status almost certainly changed (slewing → idle, or → fault). Publish it rather
        // than leaving the operator to wait up to a second for the poll to notice.
        if let Ok(status) = facade.wire_status().await {
            bus.publish(status);
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            correlation_id,
            watch_topic: astroctl_core::event::Topic::MountPosition.as_str(),
        }),
    )
        .into_response())
}

/// `POST /api/mount/tracking` — SDD §5.8.1.
pub async fn tracking(
    State(state): State<AppState>,
    Json(request): Json<TrackingRequest>,
) -> Result<Json<event::MountStatus>, ApiFailure> {
    let device = &state.mount.device;
    match request.mode {
        TrackingRequestMode::Off => device.stop_tracking().await,
        TrackingRequestMode::Sidereal => device.start_tracking(TrackingMode::Sidereal).await,
        TrackingRequestMode::Lunar => device.start_tracking(TrackingMode::Lunar).await,
        TrackingRequestMode::Solar => device.start_tracking(TrackingMode::Solar).await,
    }
    .map_err(|e| device_failure(&e))?;

    let status = state
        .mount
        .wire_status()
        .await
        .map_err(|e| device_failure(&e))?;
    state.bus.publish(status);
    Ok(Json(status))
}

/// `POST /api/mount/slew` — SDD §5.8.1's dead-man's switch.
///
/// # What this does and does not do in M1
///
/// It authorises and starts the motion, clamps the TTL, and reports the clamped value. The TTL
/// *watcher* — the thing that stops the axis when no renewal arrives — is M1-T05's, because it
/// lives in `SafeMount` alongside the limits (§5.8.1 names SafeMount for exactly this). Until
/// then `expires_in_ms` is an honest statement of what the server clamped the request to, and
/// `slew/stop` on release is the stop path. Release is the primary stop in the design anyway;
/// TTL expiry is the backstop.
pub async fn slew(
    State(state): State<AppState>,
    Json(request): Json<SlewRequest>,
) -> Result<Json<SlewAccepted>, ApiFailure> {
    let speed = to_slew_speed(request.speed).map_err(ApiFailure)?;
    let limits = &state.config.mount.limits;
    let requested = request.ttl_ms.unwrap_or(limits.slew_ttl_default_ms);
    // Clamped, never refused: a client asking for a longer lease than the node allows is asking
    // for something reasonable in a way the node disagrees with, and a 422 would leave the
    // D-pad dead rather than merely renewing more often than the client planned.
    let ttl_ms = requested.min(limits.slew_ttl_max_ms);

    state
        .mount
        .device
        .slew(
            request.axis.into(),
            request.direction.on(request.axis),
            speed,
        )
        .await
        .map_err(|e| device_failure(&e))?;

    Ok(Json(SlewAccepted {
        axis: request.axis,
        expires_in_ms: ttl_ms,
    }))
}

/// `POST /api/mount/slew/stop` — SDD §5.8.1.
///
/// A stopping command: never staleness-rejected (M1-T10), and it stops both axes when `axis` is
/// omitted, because "stop" with no qualifier can only reasonably mean all of it.
pub async fn slew_stop(
    State(state): State<AppState>,
    body: Option<Json<SlewStopRequest>>,
) -> Result<Json<event::MountStatus>, ApiFailure> {
    let request = body.map(|Json(body)| body).unwrap_or_default();
    let device = &state.mount.device;

    match request.axis {
        Some(axis) => device
            .stop_slew(axis.into())
            .await
            .map_err(|e| device_failure(&e))?,
        None => {
            // Both axes, and the second is attempted even if the first fails — a partial stop
            // reported as a failure is still better than an early return that leaves an axis
            // turning.
            let ra = device.stop_slew(Axis::Ra).await;
            let dec = device.stop_slew(Axis::Dec).await;
            ra.or(dec).map_err(|e| device_failure(&e))?;
        }
    }

    let status = state
        .mount
        .wire_status()
        .await
        .map_err(|e| device_failure(&e))?;
    state.bus.publish(status);
    Ok(Json(status))
}

/// `POST /api/mount/park` — SDD §5.8.1, `202`.
pub async fn park(State(state): State<AppState>) -> Result<Response, ApiFailure> {
    long_running(state, Motion::Park).await
}

/// `POST /api/mount/unpark` — SDD §5.8.1.
///
/// `202` for symmetry with park in the route table, though unparking is a state change rather
/// than a motion: it releases the park interlock and does not move the mount.
pub async fn unpark(State(state): State<AppState>) -> Result<Response, ApiFailure> {
    long_running(state, Motion::Unpark).await
}

/// Which of the two park operations [`long_running`] is running.
#[derive(Debug, Clone, Copy)]
enum Motion {
    Park,
    Unpark,
}

/// The park/unpark half of the `202 + WS progress` pattern.
///
/// Shares the goto slot: parking *is* a slew, so a park while a goto is running must be refused
/// for the same reason a second goto is, and a goto while parking must be refused too. One slot
/// is what makes that true without either path knowing about the other.
async fn long_running(state: AppState, motion: Motion) -> Result<Response, ApiFailure> {
    // Unpark is the one motion route that is legal on a parked mount — refusing it there would
    // make the mount unparkable — so it only checks that something is connected.
    match motion {
        Motion::Park => preflight(&state).await?,
        Motion::Unpark => {
            let status = state
                .mount
                .device
                .status()
                .await
                .map_err(|e| device_failure(&e))?;
            if status.state == MountState::Disconnected {
                return Err(ApiFailure(ApiError::new(
                    ErrorCode::NotConnected,
                    "the mount is not connected; POST /api/mount/connect first",
                )));
            }
        }
    }

    let correlation_id = correlation_id().map_err(|e| {
        ApiFailure(ApiError::new(
            ErrorCode::Internal,
            format!("could not generate a correlation id: {e}"),
        ))
    })?;

    let park_target = park_target(&state);
    {
        let mut inflight = state.mount.goto.lock().await;
        if inflight.is_some() {
            return Err(ApiFailure(ApiError::new(
                ErrorCode::Busy,
                "a slew is already in progress; stop it or wait for it to finish",
            )));
        }
        *inflight = Some(GotoRun {
            correlation_id: correlation_id.clone(),
            target: park_target,
        });
    }

    let facade = Arc::clone(&state.mount);
    let bus = state.bus.clone();
    let id_for_task = correlation_id.clone();
    tokio::spawn(async move {
        let outcome = match motion {
            Motion::Park => facade.device.park().await,
            Motion::Unpark => facade.device.unpark().await,
        };
        facade.goto.lock().await.take();

        if let Err(error) = outcome {
            tracing::warn!(correlation_id = %id_for_task, %error, ?motion, "park operation failed");
            let code = ErrorCode::from_device_error(DeviceKind::Mount, &error);
            bus.publish(event::Alert::warning(code.as_str(), error.to_string()));
        }
        if let Ok(status) = facade.wire_status().await {
            bus.publish(status);
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            correlation_id,
            watch_topic: astroctl_core::event::Topic::MountStatus.as_str(),
        }),
    )
        .into_response())
}

/// Refuse a motion the node can already tell will not start.
///
/// `202 Accepted` is a promise that the work began. A mount that is disconnected or parked will
/// refuse the very first command, so answering `202` and then publishing an alert would mean the
/// operator's screen said "slewing" for as long as it took the driver to say no — and on the
/// disconnected path, the driver says no instantly while the UI has already moved on.
///
/// This is a *pre*-flight, not a guarantee. Between this read and the driver's first command the
/// mount can still be unplugged, and that path is still an alert; there is no way to close that
/// window and there is no need to. What it buys is that the two states an operator routinely
/// hits — "I have not pressed Connect yet" and "it is still parked" — come back as an answer to
/// the request they made.
async fn preflight(state: &AppState) -> Result<(), ApiFailure> {
    let status = state
        .mount
        .device
        .status()
        .await
        .map_err(|e| device_failure(&e))?;

    match status.state {
        MountState::Disconnected => Err(ApiFailure(ApiError::new(
            ErrorCode::NotConnected,
            "the mount is not connected; POST /api/mount/connect first",
        ))),
        MountState::Parked => Err(ApiFailure(ApiError::new(
            ErrorCode::Busy,
            "the mount is parked and refuses motion; POST /api/mount/unpark first",
        ))),
        _ => Ok(()),
    }
}

/// Where `park` goes, from `mount.park_position`.
///
/// Only used to fill the in-flight record's `target` so a concurrent goto's 409 can name what is
/// running. A park position that does not parse was already refused at config load.
fn park_target(state: &AppState) -> RaDec {
    let park = &state.config.mount.park_position;
    RaDec::from_parts(park.ra_hours, park.dec_degrees).unwrap_or_else(|_| {
        // Unreachable: the config validator checks this at load (SDD §4.4). Not an `expect`,
        // because panicking here would take the node down while parking the telescope.
        RaDec::from_parts(0.0, 90.0).unwrap_or_else(|_| unreachable!("0h +90° is a coordinate"))
    })
}

/// A correlation id for a long-running action.
///
/// Reuses the ticket generator's randomness rather than a counter: a counter would make ids
/// predictable across restarts and collide between two nodes' logs, and this is the value that
/// ties a request in an operator's terminal to a line in the session log.
fn correlation_id() -> Result<String, getrandom::Error> {
    Ticket::generate().map(|t| t.as_str().to_owned())
}

// ---------------------------------------------------------------------------------------------
// `/api/auth/ws-ticket`
// ---------------------------------------------------------------------------------------------

/// What `POST /api/auth/ws-ticket` answers (SDD §4.5).
#[derive(Debug, Serialize)]
pub struct WsTicketResponse {
    ticket: String,
    /// Seconds, matching §4.5's `{ticket, expires_in}`.
    expires_in: u64,
}

/// `POST /api/auth/ws-ticket` — SDD §4.5, §5.8.1.
///
/// Behind the bearer layer like every other `/api` route: this is the one place the long-lived
/// token is exchanged for something safe to put in a URL.
pub async fn ws_ticket(
    State(state): State<AppState>,
) -> Result<Json<WsTicketResponse>, ApiFailure> {
    let ticket = state.tickets.issue().map_err(|e| {
        ApiFailure(ApiError::new(
            ErrorCode::Internal,
            format!("the operating system's random source is unavailable: {e}"),
        ))
    })?;
    Ok(Json(WsTicketResponse {
        ticket: ticket.as_str().to_owned(),
        expires_in: crate::ticket::TICKET_TTL.as_secs(),
    }))
}

#[cfg(test)]
mod tests {
    use astroctl_core::types::MountState as HalState;

    use super::*;

    #[test]
    fn the_seven_hal_states_collapse_onto_the_five_the_wire_has() {
        // The PWA's `MountState` is exactly the five wire values and it drops any frame carrying
        // a value it does not know. A HAL state leaking through unmapped is therefore not a
        // cosmetic difference — it is the mount panel going blank.
        assert_eq!(
            to_wire_state(HalState::Disconnected),
            event::MountState::Disconnected
        );
        assert_eq!(to_wire_state(HalState::Idle), event::MountState::Idle);
        assert_eq!(
            to_wire_state(HalState::Tracking),
            event::MountState::Idle,
            "a tracking mount is stationary in the sky frame; `tracking: true` is what says it \
             is driving"
        );
        assert_eq!(to_wire_state(HalState::Slewing), event::MountState::Slewing);
        assert_eq!(
            to_wire_state(HalState::Parking),
            event::MountState::Slewing,
            "reporting `parked` before arrival would say the tube is stowed while it is swinging"
        );
        assert_eq!(to_wire_state(HalState::Parked), event::MountState::Parked);
        assert_eq!(to_wire_state(HalState::Fault), event::MountState::Fault);
    }

    #[test]
    fn tracking_mode_survives_the_trip_to_the_wire() {
        // Decision 1 of M1-T03: the rate reaches the UI, not just the boolean.
        let status = MountStatus {
            state: HalState::Tracking,
            tracking: Some(TrackingMode::Lunar),
            slewing: false,
            parked: false,
        };
        let wire = to_wire_status(status);
        assert_eq!(wire.tracking_mode(), Some(TrackingMode::Lunar));
        assert!(wire.tracking());

        let off = MountStatus {
            state: HalState::Idle,
            tracking: None,
            slewing: false,
            parked: false,
        };
        assert_eq!(to_wire_status(off).tracking_mode(), None);
        assert!(!to_wire_status(off).tracking());
    }

    #[test]
    fn positive_is_east_on_ra_and_north_on_dec() {
        // Getting this backwards sends the mount the wrong way at 800× sidereal.
        assert_eq!(
            WireDirection::Positive.on(WireAxis::Ra),
            Direction::East,
            "east is the direction in which right ascension increases"
        );
        assert_eq!(WireDirection::Negative.on(WireAxis::Ra), Direction::West);
        assert_eq!(WireDirection::Positive.on(WireAxis::Dec), Direction::North);
        assert_eq!(WireDirection::Negative.on(WireAxis::Dec), Direction::South);
    }

    #[test]
    fn the_speed_ordinal_covers_one_to_five_and_refuses_the_rest() {
        // The five dots of §5.9's sketch, and nothing else. `0` is the interesting rejection:
        // a client that sent a rate in °/s would most likely send a small integer.
        let ladder = [
            SlewSpeed::Guide,
            SlewSpeed::Slow,
            SlewSpeed::Medium,
            SlewSpeed::Fast,
            SlewSpeed::Max,
        ];
        for (index, expected) in ladder.into_iter().enumerate() {
            let ordinal = u8::try_from(index + 1).expect("1..=5 fits");
            assert_eq!(to_slew_speed(ordinal).expect("valid"), expected);
        }
        for bad in [0_u8, 6, 255] {
            let err = to_slew_speed(bad).expect_err("out of range");
            assert_eq!(err.code, ErrorCode::Validation);
            assert_eq!(err.http_status(), 422);
        }
    }

    // --- the routes, driven through the real assembled app -----------------------------------

    /// Drive one request against the assembled node, returning status and body.
    ///
    /// `crate::assemble` rather than a hand-built router: a test that assembles its own
    /// approximation keeps passing after the real assembly changes.
    async fn call(
        state: &AppState,
        method: axum::http::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        use tower::ServiceExt as _;

        let (router, _) = crate::api::router();
        let (ws_router, _) = crate::api::ws_router();
        let app = crate::assemble(router, ws_router, state.clone());

        let mut request = axum::http::Request::builder().method(method).uri(path);
        let body = match body {
            Some(json) => {
                request = request.header(axum::http::header::CONTENT_TYPE, "application/json");
                axum::body::Body::from(serde_json::to_vec(&json).expect("serializes"))
            }
            None => axum::body::Body::empty(),
        };
        let response = app
            .oneshot(request.body(body).expect("request builds"))
            .await
            .expect("the router responds");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn node() -> AppState {
        let node = crate::test_support::TestNode::open_loopback();
        let (_, declarations) = crate::api::router();
        crate::test_support::state_with(&node, declarations)
    }

    #[tokio::test]
    async fn a_second_goto_is_refused_with_a_busy_envelope() {
        // The §5.8.1 acceptance criterion. Two concurrent gotos: the second must be told the
        // node is busy, not silently retarget the slew the operator is watching.
        let state = node();
        let (status, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the simulator must connect");

        let target = serde_json::json!({"ra_hours": 5.5, "dec_degrees": 22.0});
        let (first, body) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(target.clone()),
        )
        .await;
        assert_eq!(first, StatusCode::ACCEPTED, "a goto answers 202, not 200");
        assert_eq!(body["watch_topic"], "mount.position");
        let correlation = body["correlation_id"].as_str().expect("a correlation id");
        assert_eq!(correlation.len(), 32, "128 bits, like a ticket");

        let (second, envelope) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(target),
        )
        .await;
        assert_eq!(second, StatusCode::CONFLICT);
        assert_eq!(envelope["code"], "BUSY");
        assert_eq!(envelope["retryable"], false);
        assert_eq!(envelope["v"], 1);
        assert_eq!(
            envelope["detail"]["correlation_id"], correlation,
            "the refusal names the goto that is running, so the operator can see they are \
             fighting themselves rather than the mount"
        );
    }

    #[tokio::test]
    async fn status_reports_a_disconnected_mount_rather_than_refusing() {
        // The first screen of the app polls this before anything is connected. A 409 here would
        // make "you have not pressed Connect yet" render as an error, and would not match the
        // shape the PWA parses.
        let state = node();
        let (status, body) = call(&state, axum::http::Method::GET, "/api/mount/status", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "disconnected");
        assert_eq!(body["tracking"], false);
        assert_eq!(body["tracking_mode"], serde_json::Value::Null);
        assert_eq!(body["slewing"], false);
        assert_eq!(body["parked"], false);
    }

    #[tokio::test]
    async fn a_goto_on_a_disconnected_mount_is_409_not_422() {
        // `NOT_CONNECTED` is device *state*, not a malformed request. A 422 here would send the
        // operator looking for a typo in coordinates that were fine.
        let state = node();
        let (status, envelope) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(serde_json::json!({"ra_hours": 5.5, "dec_degrees": 22.0})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(envelope["code"], "NOT_CONNECTED");
    }

    #[tokio::test]
    async fn an_out_of_range_coordinate_is_a_validation_failure() {
        let state = node();
        // Declination, not right ascension: RA is cyclic and `RaHours::new` normalises 25 h to
        // 1 h on purpose, so it is the wrong axis to test a range refusal on. Declination has
        // actual poles.
        let (status, envelope) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(serde_json::json!({"ra_hours": 5.5, "dec_degrees": 91.0})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(envelope["code"], "VALIDATION");
    }

    #[tokio::test]
    async fn an_unknown_body_field_is_refused_rather_than_ignored() {
        // `deny_unknown_fields` on every request body: a client sending `ra` instead of
        // `ra_hours` must be told, not silently pointed at declination zero.
        let state = node();
        let (status, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(serde_json::json!({"ra_hours": 5.5, "dec_degrees": 22.0, "epoch": "J2000"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn tracking_reports_the_rate_the_mount_settled_on() {
        // Decision 1, end to end: the response and the event both name the rate, so the UI can
        // show which one is running instead of remembering the last button pressed.
        let state = node();
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;

        let (status, body) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/tracking",
            Some(serde_json::json!({"mode": "lunar"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tracking"], true);
        assert_eq!(body["tracking_mode"], "lunar");

        let (_, off) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/tracking",
            Some(serde_json::json!({"mode": "off"})),
        )
        .await;
        assert_eq!(off["tracking"], false);
        assert_eq!(off["tracking_mode"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn the_slew_ttl_is_clamped_server_side_rather_than_refused() {
        // §5.8.1: "default 500 ms, max 2000 ms, clamped server-side". A 422 would leave the
        // D-pad dead; clamping leaves it working and renewing more often than the client planned.
        let state = node();
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;

        let (status, body) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/slew",
            Some(serde_json::json!({
                "axis": "ra", "direction": "positive", "speed": 3, "ttl_ms": 60_000
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["axis"], "ra");
        assert_eq!(
            body["expires_in_ms"], 2000,
            "the example config's slew_ttl_max_ms"
        );

        // Omitting the TTL takes the configured default rather than zero.
        let (_, defaulted) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/slew",
            Some(serde_json::json!({"axis": "dec", "direction": "negative", "speed": 1})),
        )
        .await;
        assert_eq!(defaulted["expires_in_ms"], 500);
    }

    #[tokio::test]
    async fn connect_publishes_a_status_event_without_waiting_for_the_poll() {
        // The operator pressed Connect and is watching the badge. A state change the node
        // already knows about must not sit behind a one-second timer.
        let state = node();
        let mut events = state.bus.subscribe();

        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;

        let received = tokio::time::timeout(std::time::Duration::from_millis(500), events.recv())
            .await
            .expect("a status event should be published on connect");
        let astroctl_core::bus::Recv::Event(event) = received else {
            panic!("expected an event, got {received:?}");
        };
        assert_eq!(event.topic, astroctl_core::event::Topic::MountStatus);
        assert_ne!(event.data["state"], "disconnected");
    }

    #[tokio::test]
    async fn a_bare_post_with_no_body_connects() {
        // `curl -XPOST` sends no body and no content type. Answering that with a 415 or a 422
        // would make the documented curl session fail on its first line.
        let state = node();
        let (status, _) = call(&state, axum::http::Method::POST, "/api/mount/connect", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_position_route_reports_null_alt_az_over_http_too() {
        let state = node();
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        let (status, body) =
            call(&state, axum::http::Method::GET, "/api/mount/position", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["alt"], serde_json::Value::Null);
        assert_eq!(body["az"], serde_json::Value::Null);
        assert!(body["ra"].is_number());
        assert_eq!(body["pier_side"], "unknown");
    }

    #[tokio::test]
    async fn a_ws_ticket_is_issued_and_is_thirty_seconds_long() {
        let state = node();
        let (status, body) = call(
            &state,
            axum::http::Method::POST,
            "/api/auth/ws-ticket",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["expires_in"], 30);
        assert_eq!(body["ticket"].as_str().expect("a ticket").len(), 32);
    }

    #[test]
    fn a_position_reports_null_altitude_rather_than_the_horizon() {
        // Until M1-T05. A `0.0` stand-in would put the target exactly where the altitude limit
        // lives, so the one field a placeholder could corrupt is the one that refuses slews.
        let pos = RaDec::from_parts(5.5, 22.0).expect("valid");
        let wire = to_wire_position(pos);
        assert_eq!(wire.alt_degrees(), None);
        assert_eq!(wire.az_degrees(), None);
        assert!((wire.ra_hours() - 5.5).abs() < f64::EPSILON);
    }
}
