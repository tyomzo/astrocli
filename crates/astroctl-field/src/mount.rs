//! The mount facade and its routes — SDD §5.8.1's mount rows, §4.3's two mount topics.
//!
//! This is the first vertical slice: HTTP in, `Arc<dyn MountDevice>` at the bottom, events out
//! on the bus at 1 Hz. Everything above the HAL that wants to know where the telescope is reads
//! it from here.
//!
//! # What the facade owns, and what it deliberately does not
//!
//! It owns the poll task and the in-flight goto. It does **not** own safety, and after M1-T05 it
//! does not own the device either: the handle it holds is an
//! [`Arc<SafeMount>`](astroctl_safety::SafeMount), and per ADR-11 that *is* the mount as far as
//! everything above the HAL is concerned. So nothing in this file validates a target against
//! `mount.limits` — by the time a request reaches a handler here, the object it is about to call
//! is the thing that enforces them, for this route and for every future caller equally.
//!
//! Two things the routes below do reach for by name rather than through the trait, because the
//! [`MountDevice`] signatures have no room for them:
//!
//! * `alt`/`az` in `mount.position` (MNT-03), from the same topocentric transform the altitude
//!   limit uses. One transform shared between the display and the limit is what keeps a display
//!   bug and a limit bug from disagreeing about whether a target is up.
//! * the manual-slew TTL, which is a parameter of `/api/mount/slew` and not of
//!   [`MountDevice::slew`] — the dead-man's switch lives above the HAL (SDD §5.8.1).
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
use astroctl_safety::SafeMount;
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
    /// The safety wrapper (ADR-11) — a concrete type, not `Arc<dyn MountDevice>`.
    ///
    /// The trait object would be enough for every command below, and was until M1-T05. It is not
    /// enough for the two things the trait has no signature for: the topocentric transform behind
    /// `alt`/`az`, and the manual-slew TTL. Downcasting to reach them would be the same coupling
    /// with a runtime failure mode, so the type is named.
    device: Arc<SafeMount>,
    bus: EventBus,
    /// The in-flight goto, or `None`.
    ///
    /// A `tokio::sync::Mutex` rather than a `std` one because the goto path holds it across the
    /// device call that *starts* the slew. Holding a `std` guard across an `.await` is what
    /// `clippy::await_holding_lock` denies workspace-wide, and for a good reason on a node with
    /// one or two runtime workers.
    goto: Mutex<Option<GotoRun>>,
    /// The task waiting on the in-flight motion, so shutdown can stop *waiting* for it.
    ///
    /// Tracked for one reason: that task holds an [`EventBus`] handle, which is a broadcast
    /// sender, and `main`'s shutdown drops every sender so the session log's subscriber closes
    /// and flushes. A goto in flight would otherwise keep one alive and cost the tail of the
    /// night's event log — a two-minute slew is exactly when a service restart is most likely to
    /// land.
    ///
    /// A `std` mutex because nothing is awaited under it.
    inflight: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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
    /// Wrap the safety wrapper.
    #[must_use]
    pub fn new(device: Arc<SafeMount>, bus: EventBus, config: &MountConfig) -> Self {
        Self {
            device,
            bus,
            goto: Mutex::new(None),
            inflight: std::sync::Mutex::new(None),
            // The config validator already floors this at 1; clamping again costs nothing and
            // means a zero here is a slow poll rather than a division by zero.
            poll_hz: config.serial.poll_hz.max(DEFAULT_POLL_HZ),
        }
    }

    /// How long the poll task sleeps between reads.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / u64::from(self.poll_hz))
    }

    /// Remember the task waiting on a motion, replacing any finished one.
    fn track_inflight(&self, handle: tokio::task::JoinHandle<()>) {
        let mut slot = self
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(handle);
    }

    /// Stop waiting on an in-flight motion, at shutdown.
    ///
    /// **Does not stop the mount.** Aborting this task drops a future that is awaiting
    /// `MountDevice::goto`, and HAL rule 3 says dropping that future never stops hardware — the
    /// tube keeps slewing, which is precisely what SDD §7 wants from a service restart. What
    /// stops is the node's *waiting*, and with it the `EventBus` handle the task was holding.
    ///
    /// The safety wrapper's own background watch holds a second such handle and is **not** this
    /// method's business: `SafeMount` stops it in `Drop`, so the invariant holds for anything
    /// that drops the facade, including a test and a future caller that never learns this method
    /// exists. Adding a third thing to remember here is how the first two were forgotten.
    pub fn abort_inflight(&self) {
        if let Some(handle) = self
            .inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.abort();
        }
    }

    /// Release the in-flight record, but only if it is still the one `correlation_id` names.
    ///
    /// The guard is what makes a *late* motion task harmless. A task clears the slot when its
    /// device future resolves, and that can be long after the motion itself ended: an e-stop
    /// aborts the slew immediately, but a driver is entitled to resolve the future whenever it
    /// notices (the HAL only promises that dropping it does not stop the mount). If a slow task
    /// took the slot unconditionally it would remove the record of a *newer* goto that had since
    /// been accepted — and the node would then answer `202` to a third one, believing nothing was
    /// running, while two were.
    async fn release_goto(&self, correlation_id: &str) {
        let mut slot = self.goto.lock().await;
        if slot
            .as_ref()
            .is_some_and(|run| run.correlation_id == correlation_id)
        {
            slot.take();
        }
    }

    /// Forget any in-flight motion — the e-stop path.
    ///
    /// An emergency stop ends every motion the node was tracking, by definition, so the record of
    /// one is void the moment it lands. Clearing it is what lets the operator issue the next goto
    /// straight away.
    ///
    /// Without this the node stays `BUSY` until the aborted goto's *future* resolves, which is a
    /// driver's business and can be much later: measured against the simulator, an e-stop 1 s into
    /// a 57 s slew left `/api/mount/goto` answering `409 BUSY` for the remaining 56 seconds while
    /// `/api/mount/status` said `idle`. Two parts of the same API disagreeing about whether the
    /// telescope is moving is bad on its own; doing it for a minute after an emergency stop, when
    /// the operator is trying to recover, is worse.
    async fn forget_inflight_motion(&self) {
        self.goto.lock().await.take();
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

/// Build the `mount.position` payload from a coordinate pair (MNT-02, MNT-03).
///
/// `alt`/`az` come from the safety wrapper's transform, which is the same call the altitude limit
/// makes (SDD §5.4). That is the point of routing a display concern through the safety layer: the
/// number the operator reads and the number a slew is refused on are produced by one function, so
/// "it says 20° but it will not slew" cannot happen.
fn to_wire_position(safety: &SafeMount, pos: RaDec) -> event::MountPosition {
    let horizontal = safety.horizontal(pos);
    event::MountPosition::new(
        pos.ra.hours(),
        pos.dec.degrees(),
        Some(horizontal.alt.degrees()),
        Some(horizontal.az.degrees()),
        UNKNOWN_PIER,
    )
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
                            facade.bus.publish(to_wire_position(&facade.device, pos));
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
    Ok(Json(to_wire_position(&state.mount.device, pos)))
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

    // The same reasoning, for the safety limit — and it is the reason `SafeMount::check_goto`
    // exists. The wrapper refuses a below-horizon target inside `goto`, which is what makes the
    // limit hold for every caller (ADR-11); but this route spawns that call and answers `202`
    // before it runs, so without asking first the operator would get "accepted" followed by an
    // alert, instead of MNT-15's 403 `LIMIT_ALTITUDE` as the answer to what they asked.
    //
    // Asking is not enforcing: if this line were deleted the mount would still refuse the slew.
    // What would break is the answer.
    state
        .mount
        .device
        .check_goto(target)
        .map_err(|e| device_failure(&e))?;

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
    let task = tokio::spawn(async move {
        let outcome = facade.device.goto(target).await;

        // Cleared before anything is published, so a client that reacts to the completion event
        // by sending the next goto cannot race the slot — and cleared *by id*, so a task that
        // resolves late cannot evict a newer goto's record. See [`release_goto`].
        facade.release_goto(&id_for_task).await;

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

    // Tracked so shutdown can stop waiting on it and release its bus handle; see
    // `MountFacade::abort_inflight`.
    state.mount.track_inflight(task);

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
/// The lease is granted by `SafeMount`, which is also what stops the axis when no renewal
/// arrives. The clamp of §5.8.1 ("default 500 ms, max 2000 ms, clamped server-side") lives there
/// too, in one function: this route reports what the wrapper resolved, so the window the operator's
/// app renews against and the window the node is enforcing are the same number rather than two
/// computations of the same rule.
pub async fn slew(
    State(state): State<AppState>,
    Json(request): Json<SlewRequest>,
) -> Result<Json<SlewAccepted>, ApiFailure> {
    let speed = to_slew_speed(request.speed).map_err(ApiFailure)?;
    // Clamped, never refused: a client asking for a longer lease than the node allows is asking
    // for something reasonable in a way the node disagrees with, and a 422 would leave the D-pad
    // dead rather than merely renewing more often than the client planned.
    let ttl = state.mount.device.resolve_ttl(request.ttl_ms);

    state
        .mount
        .device
        .slew_for(
            request.axis.into(),
            request.direction.on(request.axis),
            speed,
            ttl,
        )
        .await
        .map_err(|e| device_failure(&e))?;

    Ok(Json(SlewAccepted {
        axis: request.axis,
        // `u64` rather than `u128`: the clamp is at most `slew_ttl_max_ms`, which the config
        // validator bounds to 10 000.
        expires_in_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
    }))
}

/// `POST /api/mount/estop` — SDD §5.8.2, REL-01, PRF-12, MNT-08.
///
/// # Everything unusual about this handler is the point
///
/// **No body extractor.** §5.8.2 says "auth only, no JSON parsing — empty body accepted", and the
/// way to get that in axum is to declare no body argument at all: nothing can then reject the
/// request before the handler runs. `curl -X POST` with no body, the PWA's `keepalive` fetch with
/// no body, and a client that sends a stray JSON object all reach the same line of code. A
/// `Json<T>` extractor here would turn a malformed body into a 422 *instead of stopping the
/// telescope*, which is the failure this shape exists to make impossible.
///
/// **It takes no lock and reads no state first.** The call to `emergency_stop` is the first thing
/// that happens. §5.8.2 budgets 20 ms from handler to bytes on the wire and §5.2.4 gives the stop
/// its own lane to the serial task; a handler that read `status()` first — to answer more
/// helpfully, say — would put the operator's stop behind a position poll on a line that is busy
/// precisely when they are reaching for this button.
///
/// **A transport failure is still reported, and is still a 502.** The wrapper has already tried
/// every axis by then (the HAL contract), and an operator who gets an error from the e-stop needs
/// to know to cut power rather than to press it again.
pub async fn estop(State(state): State<AppState>) -> Result<Json<EmergencyStopped>, ApiFailure> {
    let stopped = state.mount.device.emergency_stop().await;

    // After the stop, never before it — this takes a lock, and nothing may sit between the
    // operator's request and the driver call. See [`MountFacade::forget_inflight_motion`] for the
    // minute-long `BUSY` this prevents.
    state.mount.forget_inflight_motion().await;

    stopped.map_err(|e| device_failure(&e))?;
    Ok(Json(EmergencyStopped { stopped: true }))
}

/// What `/api/mount/estop` answers.
///
/// A body rather than a `204`, and one field rather than a status snapshot: the operator's app
/// must not render "the mount stopped" from its own request (SDD §5.9's no-optimistic-mutation
/// rule) — it renders that from the `alert` and the `mount.status` the node publishes. What this
/// body says is only that the node received the request and the driver accepted it, which is the
/// one thing the reply is entitled to claim.
#[derive(Debug, Serialize)]
pub struct EmergencyStopped {
    stopped: bool,
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
    let task = tokio::spawn(async move {
        let outcome = match motion {
            Motion::Park => facade.device.park().await,
            Motion::Unpark => facade.device.unpark().await,
        };
        facade.release_goto(&id_for_task).await;

        if let Err(error) = outcome {
            tracing::warn!(correlation_id = %id_for_task, %error, ?motion, "park operation failed");
            let code = ErrorCode::from_device_error(DeviceKind::Mount, &error);
            bus.publish(event::Alert::warning(code.as_str(), error.to_string()));
        }
        if let Ok(status) = facade.wire_status().await {
            bus.publish(status);
        }
    });

    state.mount.track_inflight(task);

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

    async fn node() -> AppState {
        let node = crate::test_support::TestNode::open_loopback();
        let (_, declarations) = crate::api::router();
        crate::test_support::state_with(&node, declarations).await
    }

    /// Wait until the tube is actually moving, not merely until the node says `slewing`.
    ///
    /// Load-bearing for the e-stop tests, and the reason is a trap worth naming twice.
    ///
    /// The driver reserves both axes *before* its opening exchange and reports `slewing: true`
    /// from that moment, so `status` says the mount is moving during the ~48 ms in which it is
    /// still talking to the controller. A stop issued in that window is caught by the driver's
    /// own post-exchange abort check, which makes `goto` return **early** and release the
    /// in-flight slot on its way out — the tidy path, not the one the operator meets. Both e-stop
    /// regression tests below passed against a deliberately broken fix while they waited on
    /// `status`, and only started failing once they waited on the *position*, which cannot change
    /// until the axis plans are installed and running.
    async fn until_moving(state: &AppState) {
        let start = state
            .mount
            .device
            .position()
            .await
            .expect("a starting position");
        for _ in 0..400 {
            if let Ok(now) = state.mount.device.position().await {
                let moved = (now.ra.hours() - start.ra.hours()).abs() * 15.0
                    + (now.dec.degrees() - start.dec.degrees()).abs();
                if moved > 0.5 {
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the simulator never moved");
    }

    /// A target that is above the example config's altitude limit at **every** hour of the day.
    ///
    /// Circumpolar from the example site (Oslo, latitude 59.9°): at declination +70° the lowest
    /// the target ever gets is 39.9°, well clear of the configured 15°. Every test below that
    /// needs a goto to actually *start* uses it.
    ///
    /// This is not fussiness. Two of M1-T03's goto fixtures were chosen before there was an
    /// altitude limit and are latitude-blind — `dec −30°` never rises above 0.1° from Oslo, and
    /// `dec +22°` is above the limit for part of the day and below it for the rest. The first
    /// failed the moment this task landed; the second would have passed in the afternoon and
    /// failed at two in the morning, which is the worst way for a test to be wrong.
    fn circumpolar_target() -> serde_json::Value {
        serde_json::json!({"ra_hours": 12.0, "dec_degrees": 70.0})
    }

    #[tokio::test]
    async fn a_second_goto_is_refused_with_a_busy_envelope() {
        // The §5.8.1 acceptance criterion. Two concurrent gotos: the second must be told the
        // node is busy, not silently retarget the slew the operator is watching.
        let state = node().await;
        let (status, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the simulator must connect");

        let target = circumpolar_target();
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
        let state = node().await;
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
        let state = node().await;
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
        let state = node().await;
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
        let state = node().await;
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
        let state = node().await;
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
        let state = node().await;
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
        let state = node().await;
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
        let state = node().await;
        let (status, _) = call(&state, axum::http::Method::POST, "/api/mount/connect", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_position_route_carries_alt_az_for_the_configured_site() {
        // MNT-03, and the half of the M1-T05 acceptance criterion that is about the wire: the
        // fields M1-T03 left `null` are populated, and by the same transform the limit uses (the
        // agreement with an independent reference is asserted in `astroctl-safety`, against
        // astropy, where the transform lives).
        let state = node().await;
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

        let alt = body["alt"].as_f64().expect("an altitude: {body}");
        let az = body["az"].as_f64().expect("an azimuth: {body}");
        assert!(
            (-90.0..=90.0).contains(&alt),
            "altitude out of range: {alt}"
        );
        assert!((0.0..360.0).contains(&az), "azimuth out of range: {az}");
        assert!(body["ra"].is_number());
        // Still `unknown`: SDD §5.2.3 derives pier side from the declination counters *in the
        // driver*, and no Phase 1 driver reports it. Guessing "east" would be worse than saying
        // so, because the meridian limit is documented as consuming this value.
        assert_eq!(body["pier_side"], "unknown");

        // The number on the wire is the number the safety layer computed, not a second opinion.
        let position = state.mount.device.position().await.expect("a position");
        let horizontal = state.mount.device.horizontal(position);
        assert!(
            (alt - horizontal.alt.degrees()).abs() < 1.0,
            "the wire says {alt}° and the safety layer says {}°",
            horizontal.alt.degrees()
        );
    }

    /// A target that is below the example config's horizon limit at this instant.
    ///
    /// Computed rather than written down: "below the horizon" is a fact about the clock, and a
    /// fixture pair picked in July is above the horizon in January. A test that started failing
    /// six months after it was written would be blamed on anything but the sky.
    fn below_the_limit(state: &AppState) -> RaDec {
        let site = astroctl_safety::Site::from_config(&state.config.site);
        let now = chrono::Utc::now();
        let lst_hours = astroctl_safety::local_sidereal_degrees(site, now) / 15.0;
        // Twelve hours from the local sidereal time puts the target on the far side of the
        // meridian, and a declination as far south as the site's latitude allows puts it well
        // under the horizon from Oslo.
        let target = RaDec::from_parts((lst_hours + 12.0).rem_euclid(24.0), -45.0)
            .expect("a valid coordinate");
        let altitude = astroctl_safety::horizontal(target, site, now).alt.degrees();
        assert!(
            altitude < state.config.mount.limits.min_altitude_degrees,
            "the fixture target is at {altitude}°, not below the configured limit"
        );
        target
    }

    #[tokio::test]
    async fn a_goto_below_the_altitude_limit_is_403_and_the_mount_is_never_commanded() {
        // MNT-15's acceptance criterion, through the whole stack: the envelope, the status, and —
        // the part that matters — that the telescope was not asked to move. A wrapper that
        // refused *after* forwarding would pass the first two assertions while slewing into the
        // ground.
        let state = node().await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        let before = state.mount.device.position().await.expect("a position");

        let target = below_the_limit(&state);
        let (status, envelope) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(serde_json::json!({
                "ra_hours": target.ra.hours(),
                "dec_degrees": target.dec.degrees(),
            })),
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(envelope["code"], "LIMIT_ALTITUDE");
        assert_eq!(envelope["retryable"], false);
        assert_eq!(envelope["v"], 1);
        assert!(
            envelope["message"]
                .as_str()
                .is_some_and(|m| m.contains("min_altitude_degrees")),
            "the refusal must name the setting that caused it: {envelope}"
        );

        // The simulator has no command log, so "never commanded" is asserted against what a
        // command would have changed: the mount is not slewing and has not moved. A goto that
        // reached the driver would have set both.
        let after = state.mount.device.status().await.expect("a status");
        assert!(!after.slewing, "a refused goto started a slew");
        assert_eq!(after.state, MountState::Idle);
        let position = state.mount.device.position().await.expect("a position");
        assert!(
            (position.dec.degrees() - before.dec.degrees()).abs() < 0.5,
            "the mount moved: {before:?} → {position:?}"
        );
    }

    // --- the e-stop route (SDD §5.8.2, REL-01, MNT-08) ----------------------------------------

    #[tokio::test]
    async fn the_estop_route_accepts_a_request_with_no_body_at_all() {
        // §5.8.2: "auth only, no JSON parsing — empty body accepted". `curl -X POST` sends no
        // body and no content type, and the PWA's `keepalive` fetch sends none either. A 415 or a
        // 422 here would be the e-stop failing on the most ordinary way to call it.
        let state = node().await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;

        let (status, body) = call(&state, axum::http::Method::POST, "/api/mount/estop", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["stopped"], true);
    }

    #[tokio::test]
    async fn the_estop_route_ignores_a_body_it_was_sent_anyway() {
        // The other half of "no JSON parsing": a client that sends something must not be refused
        // for it. There is no request shape that turns this route into a 4xx.
        let state = node().await;
        let (status, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/estop",
            Some(serde_json::json!({"anything": "at all"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_estop_route_answers_while_a_goto_is_mid_flight_and_stops_it() {
        // The load-bearing one. A goto is a two-minute motion the node has already answered `202`
        // to; the operator then reaches for the button. Two things must hold, and only the second
        // is obvious: the route has to *answer* (it is not queued behind the slew), and the slew
        // has to *stop*.
        let state = node().await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        let mut events = state.bus.subscribe();

        let (accepted, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        assert_eq!(accepted, StatusCode::ACCEPTED, "a goto must be in flight");
        assert!(
            state.mount.device.status().await.expect("status").slewing,
            "the simulator should be slewing before the stop"
        );

        let issued = std::time::Instant::now();
        let (status, body) = call(&state, axum::http::Method::POST, "/api/mount/estop", None).await;
        let latency = issued.elapsed();

        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !state.mount.device.status().await.expect("status").slewing,
            "the mount was still slewing after the e-stop"
        );
        // A ceiling, not the budget. SDD §5.8.2 allows 20 ms from handler to wire and the
        // simulator spends 16 ms of that modelling the round trip, which leaves too little margin
        // to assert against on a shared CI box that may be compiling something else. The 20 ms
        // figure is measured on the running binary (see the M1-T05 result notes); what this
        // asserts is the property CI can hold: the stop did not wait for the slew.
        assert!(
            latency < std::time::Duration::from_millis(500),
            "the e-stop took {latency:?}, which means it queued behind something"
        );

        // The operator's app renders the stop from the event stream, never from its own request
        // (SDD §5.9). So the alert has to be on the bus, or the button has nothing to confirm
        // against.
        let alerted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let astroctl_core::bus::Recv::Event(event) = events.recv().await {
                    if event.topic == astroctl_core::event::Topic::Alert
                        && event.data["code"] == "EMERGENCY_STOP"
                    {
                        return event;
                    }
                } else {
                    panic!("the bus closed before the e-stop alert arrived");
                }
            }
        })
        .await
        .expect("an EMERGENCY_STOP alert");
        assert_eq!(alerted.data["severity"], "critical");
    }

    #[tokio::test]
    async fn a_goto_is_accepted_again_immediately_after_an_emergency_stop() {
        // Found on the running node, not in review. An e-stop one second into a 57-second slew
        // left `/api/mount/goto` answering `409 BUSY` for the remaining 56 seconds — while
        // `/api/mount/status` reported `idle`, because the mount really had stopped. The node was
        // holding an in-flight record whose task would not resolve until the *originally planned*
        // finish, and the operator recovering from an emergency stop is the last person who
        // should be told to wait for a slew that is not happening.
        let state = node().await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        let (accepted, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        assert_eq!(accepted, StatusCode::ACCEPTED);
        until_moving(&state).await;

        call(&state, axum::http::Method::POST, "/api/mount/estop", None).await;

        let (again, body) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        assert_eq!(
            again,
            StatusCode::ACCEPTED,
            "a goto after an emergency stop was refused: {body}"
        );
    }

    #[tokio::test]
    async fn a_late_motion_task_does_not_evict_a_newer_gotos_record() {
        // The hazard the fix above introduces if it is done carelessly. The aborted goto's task is
        // still running and will clear the in-flight slot when its future finally resolves; if it
        // cleared the slot unconditionally it would remove the *second* goto's record, and the
        // node would then answer `202` to a third while two were in flight — the exact confusion
        // the in-flight slot exists to prevent (§5.8.1).
        let state = node().await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        until_moving(&state).await;
        call(&state, axum::http::Method::POST, "/api/mount/estop", None).await;
        let (_, second) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        let second_id = second["correlation_id"].as_str().expect("an id").to_owned();

        // Give the first task every chance to resolve and clear the slot it no longer owns.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let (third, envelope) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        assert_eq!(third, StatusCode::CONFLICT, "{envelope}");
        assert_eq!(
            envelope["detail"]["correlation_id"], second_id,
            "the surviving record must be the second goto's, not the aborted one's"
        );
    }

    #[tokio::test]
    async fn an_interrupted_goto_reports_aborted_rather_than_a_rejected_request() {
        // M1-T02's handoff defect, end to end through the e-stop route: `ABORTED`/409 says
        // something stopped the mount, `DEVICE_REJECTED`/422 would say the operator's goto was
        // malformed at the exact moment their emergency stop worked.
        let state = node().await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        let mut events = state.bus.subscribe();
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        call(&state, axum::http::Method::POST, "/api/mount/estop", None).await;

        let aborted = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let astroctl_core::bus::Recv::Event(event) = events.recv().await {
                    if event.topic == astroctl_core::event::Topic::Alert
                        && event.data["code"] == "ABORTED"
                    {
                        return true;
                    }
                } else {
                    return false;
                }
            }
        })
        .await;
        assert_eq!(
            aborted,
            Ok(true),
            "the interrupted goto should alert ABORTED"
        );
    }

    #[tokio::test]
    async fn the_slew_route_grants_a_lease_the_safety_layer_is_enforcing() {
        // The route and the wrapper must agree about the window, because the app renews against
        // what the route reported and the wrapper stops the axis by what it recorded. One
        // resolution, in `SafeMount::resolve_ttl`, is what makes them the same number.
        let state = node().await;
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
            Some(serde_json::json!({"axis": "ra", "direction": "positive", "speed": 2})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["expires_in_ms"].as_u64(),
            Some(
                state
                    .mount
                    .device
                    .resolve_ttl(None)
                    .as_millis()
                    .try_into()
                    .expect("the configured TTL fits")
            ),
        );
    }

    #[tokio::test]
    async fn shutdown_releases_the_bus_handle_that_an_in_flight_goto_holds() {
        // A regression test for a hang that reached a running node: the task awaiting a goto
        // holds an `EventBus` handle, which is a broadcast *sender*, so `main`'s shutdown could
        // not close the session log's subscriber while a slew was in flight. It cost a full
        // flush timeout and the tail of the night's event log — and a two-minute slew is exactly
        // when a service restart lands.
        //
        // The property under test is the one `main` depends on: once the facade and the state
        // are gone, a subscriber sees `Closed`. If it does not, some sender survived.
        let state = node().await;
        call(
            &state,
            axum::http::Method::POST,
            "/api/mount/connect",
            Some(serde_json::json!({})),
        )
        .await;
        let (accepted, _) = call(
            &state,
            axum::http::Method::POST,
            "/api/mount/goto",
            Some(circumpolar_target()),
        )
        .await;
        assert_eq!(accepted, StatusCode::ACCEPTED, "a goto must be in flight");

        let mut events = state.bus.subscribe();
        let mount = Arc::clone(&state.mount);
        mount.abort_inflight();
        drop(state);
        drop(mount);

        // The aborted task is dropped by the runtime, not synchronously by `abort`, so the
        // sender it held goes a scheduling turn later.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(events.recv().await, astroctl_core::bus::Recv::Closed) {
                    return;
                }
            }
        })
        .await;
        assert!(
            closed.is_ok(),
            "an EventBus handle outlived the facade, so the session log could not flush"
        );
    }

    #[tokio::test]
    async fn a_ws_ticket_is_issued_and_is_thirty_seconds_long() {
        let state = node().await;
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

    #[tokio::test]
    async fn the_wire_position_carries_the_safety_layers_own_altitude() {
        // Not a tolerance — an identity. The `mount.position` payload and the altitude limit read
        // the same function, so the assertion is that the two numbers are bit-for-bit the same
        // rather than close: anything looser would let a second transform creep in later and
        // still pass.
        let state = node().await;
        let pos = RaDec::from_parts(5.5, 22.0).expect("valid");
        let wire = to_wire_position(&state.mount.device, pos);
        let horizontal = state.mount.device.horizontal(pos);

        assert_eq!(wire.alt_degrees(), Some(horizontal.alt.degrees()));
        assert_eq!(wire.az_degrees(), Some(horizontal.az.degrees()));
        assert!((wire.ra_hours() - 5.5).abs() < f64::EPSILON);
    }
}
