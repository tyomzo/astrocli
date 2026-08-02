//! `SafeMount` — the mount facade the API and the orchestrator see (ADR-11, SDD §5.4).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use astroctl_core::bus::EventBus;
use astroctl_core::config::{FieldConfig, MountLimits};
use astroctl_core::error::{DeviceError, ErrorCode, Limit};
use astroctl_core::event::Alert;
use astroctl_core::types::{
    Axis, DeviceInfo, Direction, GuideRate, MountCapabilities, MountStatus, MountTravel, PierSide,
    RaDec, SlewSpeed, TrackingMode,
};
use astroctl_hal::mount::MountDevice;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::horizontal::{self, Site, DEGREES_PER_MINUTE_OF_TIME};

/// How often the background check looks at a mount that is moving (SDD §5.4: "background limit
/// check at 2 Hz while manual slew active").
///
/// This is a *ceiling* on the sleep, not the only wakeup: a TTL deadline that falls sooner wakes
/// the task at the deadline itself. T-SLW-1 allows the axis 100 ms of slack after the TTL, and a
/// 2 Hz poll alone would spend up to 500 ms of it before even noticing.
const CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// The shortest the watch will ever sleep.
///
/// Found by sabotage rather than by review. Deliberately breaking [`expire_leases`] to check that
/// T-SLW-1 had teeth left a lease in the table whose deadline was already in the past — and the
/// loop then computed a wake instant that had already passed, `sleep_until` returned without
/// yielding, and the task busy-spun a core at 100 % with the mount still moving. The test did not
/// fail; it hung, which on a Raspberry Pi driving a telescope is the worse outcome of the two.
///
/// [`expire_leases`] removes a lease before it awaits anything, so the invariant that makes that
/// unreachable holds today. This floor is what stops a future edit from making it reachable
/// again, and one millisecond against T-SLW-1's 100 ms of slack is not a cost.
const MIN_TICK: Duration = Duration::from_millis(1);

/// How far ahead of the current position a manual slew is checked, in degrees.
///
/// A manual slew has no target coordinate — the "target" MNT-15 speaks of is wherever the axis is
/// heading. One degree is far enough to be well outside the mount's pointing error and near enough
/// that the check is about the next second of motion rather than about the far side of the sky.
const LOOKAHEAD_DEGREES: f64 = 1.0;

/// The largest hour-angle step between two ticks that still counts as the sky turning.
///
/// The meridian watch fires on a *crossing*, so it needs to tell "the target drifted past the
/// limit" from "a goto moved the tube somewhere else entirely" and from the ±180° wrap. Sidereal
/// motion is 0.0021° per tick, so a degree is three orders of magnitude of headroom; a goto
/// crossing the limit is not a crossing this watch should act on, because the operator asked for
/// it and the altitude check already passed the target.
const CROSSING_STEP_CEILING_DEGREES: f64 = 1.0;

/// Alert code for a stop this wrapper performed on its own initiative.
///
/// A free string rather than an [`ErrorCode`]: SDD §4.2's enum is a frozen contract with the UI
/// and has no e-stop member, because an emergency stop is not a request that failed. The
/// watchdog's `DISK_LOW` sets the same precedent.
const EMERGENCY_STOP: &str = "EMERGENCY_STOP";

// ---------------------------------------------------------------------------------------------
// The wrapper
// ---------------------------------------------------------------------------------------------

/// Every caller's path to the mount (ADR-11, SDD §5.4).
///
/// # Why this implements [`MountDevice`] rather than sitting beside one
///
/// ADR-11: enforcement lives *below* the API, so that a limit cannot be bypassed by reaching the
/// driver another way — the LLM tool layer (Phase 2c), a future script, a test harness and the
/// REST handlers all hold `Arc<dyn MountDevice>`, and the object at the end of that pointer is
/// this one. A safety module the API had to remember to call would be correct in review and
/// wrong the first time somebody added a route.
///
/// # What it enforces
///
/// * **Altitude (MNT-15).** A goto target below `mount.limits.min_altitude_degrees` is refused
///   before a single byte reaches the driver. A manual slew is refused when the direction it was
///   asked to move in would take the tube below the limit — see [`SafeMount::slew_for`] for why
///   the check is directional rather than positional.
/// * **The dead-man's switch (SDD §5.8.1).** Manual slew is authorised for `ttl_ms` at a time; an
///   identical repeat extends the deadline without touching the motors; silence stops the axis.
/// * **Meridian (MNT-16).** While the mount is tracking, an hour angle crossing
///   `mount.limits.meridian_limit_minutes` past the meridian stops tracking and alerts.
/// * **Travel from home (M3-T07).** A manual slew that would drive an axis further than
///   `mount.limits.max_travel_from_home_degrees` from the mechanical home pose is refused before
///   the driver is called, and an axis that reaches the limit *during* a hold is stopped. See
///   [`Shared::winds_past_limit`] for why the check is directional and
///   [`SafeMount::slew_for`] for why it must also run in the watch.
///
/// # What it deliberately does not gate
///
/// `emergency_stop`, `stop_slew`, `stop_tracking`, `disconnect` and `park` — every one of them
/// either stops motion or stows the telescope. A safety layer that could refuse a stop would be
/// the most dangerous component in the system. `park` in particular is checked by nothing: the
/// park position comes from the operator's own configuration, and refusing to park a mount
/// because its park position is low is refusing the one action that makes it safe.
///
/// `sync` and `guide_pulse` pass through as well: neither moves the tube more than an arcsecond,
/// and a guide loop that had its corrections refused would fight the limit at 1 Hz.
pub struct SafeMount {
    shared: Arc<Shared>,
}

/// What the wrapper and its background watch both need.
///
/// Separated from [`SafeMount`] so the watch task can hold an `Arc` of *this* rather than of the
/// wrapper. That is not a style choice: the watch holds an [`EventBus`] handle, and an
/// `Arc<SafeMount>` inside a task the `SafeMount` owns would be a reference cycle that keeps a
/// broadcast sender alive forever — which stalls the event log's flush at shutdown for its whole
/// timeout and costs the tail of the night's telemetry (the M1-T03 follow-up fix, `78519e4`).
/// With the cycle broken, `Drop for SafeMount` is enough to guarantee the invariant.
struct Shared {
    inner: Arc<dyn MountDevice>,
    limits: MountLimits,
    site: Site,
    bus: EventBus,
    motion: Mutex<Motion>,
}

/// What the mount has been authorised to do, and by whom.
#[derive(Debug, Default)]
struct Motion {
    /// The live manual-slew authorisation per axis.
    ra: Option<Lease>,
    dec: Option<Lease>,
    /// Whether tracking has been started through this wrapper — the meridian watch's trigger.
    tracking: bool,
    /// The background watch, while there is anything to watch.
    watch: Option<JoinHandle<()>>,
}

/// One axis's authorisation to move, and when it runs out.
#[derive(Debug, Clone, Copy)]
struct Lease {
    /// The instant the authorisation lapses. A renewal moves this and nothing else.
    deadline: Instant,
    dir: Direction,
    speed: SlewSpeed,
    /// The window this lease was granted for, for the alert message.
    ttl: Duration,
}

impl Motion {
    fn axis(&mut self, axis: Axis) -> &mut Option<Lease> {
        match axis {
            Axis::Ra => &mut self.ra,
            Axis::Dec => &mut self.dec,
        }
    }

    const fn lease(&self, axis: Axis) -> Option<Lease> {
        match axis {
            Axis::Ra => self.ra,
            Axis::Dec => self.dec,
        }
    }

    /// Is there anything for the background task to watch?
    const fn needs_watching(&self) -> bool {
        self.ra.is_some() || self.dec.is_some() || self.tracking
    }

    /// The soonest lease deadline, if any.
    fn next_deadline(&self) -> Option<Instant> {
        match (self.ra, self.dec) {
            (Some(ra), Some(dec)) => Some(ra.deadline.min(dec.deadline)),
            (Some(only), None) | (None, Some(only)) => Some(only.deadline),
            (None, None) => None,
        }
    }

    fn clear_leases(&mut self) {
        self.ra = None;
        self.dec = None;
    }
}

impl std::fmt::Debug for SafeMount {
    /// Hand-written because [`EventBus`] and `Arc<dyn MountDevice>` are in the struct and the
    /// derive would recurse into the whole driver. The [`MountDevice`] supertrait requires it, and
    /// SDD §2's logging convention is that state is inspectable — so what is printed is the state
    /// a support question is about: the limits in force and what is currently authorised.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let motion = self.shared.lock();
        f.debug_struct("SafeMount")
            .field("inner", &self.shared.inner)
            .field(
                "min_altitude_degrees",
                &self.shared.limits.min_altitude_degrees,
            )
            .field(
                "meridian_limit_minutes",
                &self.shared.limits.meridian_limit_minutes,
            )
            .field("ra_lease", &motion.ra)
            .field("dec_lease", &motion.dec)
            .field("tracking", &motion.tracking)
            .finish()
    }
}

impl Drop for SafeMount {
    /// Stops the background watch when the last handle to this wrapper goes.
    ///
    /// This is the shutdown invariant rather than a tidy-up: the watch holds an [`EventBus`]
    /// handle, which is a broadcast *sender*, and the session log's writer only closes and
    /// flushes once every sender is gone (SDD §7 step 5). Doing it in `Drop` rather than in an
    /// explicit `shutdown()` means the binary cannot forget — and the binary did forget, twice,
    /// in M1-T03.
    fn drop(&mut self) {
        if let Some(watch) = self.shared.lock().watch.take() {
            watch.abort();
        }
    }
}

impl Shared {
    /// The motion state.
    ///
    /// Poisoning would mean a panic inside one of these short synchronous sections — a bug in
    /// this file rather than a condition to recover from, and recovering is the safe direction:
    /// refusing to lock would make every stop path unavailable.
    fn lock(&self) -> MutexGuard<'_, Motion> {
        self.motion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Where `target` would be in the sky right now.
    fn horizontal_at(&self, target: RaDec, at: DateTime<Utc>) -> astroctl_core::types::AltAz {
        horizontal::horizontal(target, self.site, at)
    }

    /// Refuse `target` if it is below the configured horizon (MNT-15).
    fn check_altitude(&self, target: RaDec, at: DateTime<Utc>) -> Result<(), DeviceError> {
        let altitude = self.horizontal_at(target, at).alt.degrees();
        if altitude < self.limits.min_altitude_degrees {
            return Err(altitude_violation(
                altitude,
                self.limits.min_altitude_degrees,
            ));
        }
        Ok(())
    }

    /// The altitude one lookahead step along, when that step would descend below the limit.
    ///
    /// `None` means the motion is allowed: either it climbs, or it stays above the limit, or the
    /// axis/direction pair is nonsense the driver should be the one to refuse.
    ///
    /// One implementation, two callers — [`SafeMount::slew_for`]'s pre-flight check and the
    /// background watch's continuous one. Two would be two chances to disagree about the same
    /// motion half a second apart.
    fn descends_below_limit(
        &self,
        from: RaDec,
        axis: Axis,
        dir: Direction,
        at: DateTime<Utc>,
    ) -> Option<f64> {
        let ahead = self.predict(from, axis, dir)?;
        let ahead_altitude = self.horizontal_at(ahead, at).alt.degrees();
        let here_altitude = self.horizontal_at(from, at).alt.degrees();
        // The guard needs no special case for where the prediction came from, and that is worth
        // stating because the first M3-T08 draft added one. "Permit anything that climbs"
        // (obligation 5) is sound exactly when `ahead` is sound; the 2026-08-02 defect was never
        // this comparison, it was being handed a prediction from a frame that could not see the
        // branch. Fix the prediction and the rule is correct again — for both sources, since a
        // device with no mechanical state is one whose motion really is celestially monotonic.
        (ahead_altitude < self.limits.min_altitude_degrees && ahead_altitude <= here_altitude)
            .then_some(ahead_altitude)
    }

    /// One degree of the commanded motion, from the best frame available (M3-T08).
    ///
    /// Asks the driver first — it is the only layer holding the mechanical state that decides which
    /// way declination moves (SDD §5.4.1) — and falls back to [`celestial_lookahead`] for a driver
    /// that has none.
    fn predict(&self, from: RaDec, axis: Axis, dir: Direction) -> Option<RaDec> {
        self.inner
            .motion_lookahead(axis, dir, LOOKAHEAD_DEGREES)
            .or_else(|| celestial_lookahead(from, axis, dir))
    }

    /// The travel one axis has accumulated, when moving `dir` would make it worse (M3-T07).
    ///
    /// `None` means the motion is allowed: the driver has no home reference, the axis is inside
    /// the limit, or the direction asked for is the one that unwinds.
    ///
    /// # The check is directional, for the same reason the altitude check is
    ///
    /// A bare `travel >= limit` comparison would be simpler and would trap the mount at exactly
    /// the moment somebody needs to drive it back: an axis already past the limit could not be
    /// moved at all, and the only recovery left is the one the operator was reduced to on
    /// 2026-08-02 — power off, loosen the clutch, unwind by hand. So the limit refuses the
    /// direction that winds and always permits the one that unwinds. Which sky direction that is
    /// depends on the hemisphere and the mechanical branch, so the driver states it
    /// ([`AxisTravel::homeward`](astroctl_core::types::AxisTravel::homeward)) rather than exporting
    /// the inputs for a second implementation here to get backwards.
    ///
    /// One implementation, two callers — the pre-flight check and the watch — for the reason
    /// [`Self::descends_below_limit`] gives.
    fn winds_past_limit(&self, axis: Axis, dir: Direction) -> Option<f64> {
        let travel = self.inner.axis_travel()?.axis(axis);
        (travel.degrees >= self.limits.max_travel_from_home_degrees && dir != travel.homeward)
            .then_some(travel.degrees)
    }

    /// Publish an alert for something this wrapper did on its own.
    fn alert(&self, code: &str, message: String) {
        tracing::warn!(alert = code, %message, "the safety wrapper stopped the mount");
        self.bus.publish(Alert::warning(code, message));
    }
}

/// The refusal an altitude check produces.
///
/// Both numbers are in the message because "below the limit" without them sends the operator to
/// the configuration file to find out what the limit *is*, in the dark, at the telescope.
fn altitude_violation(altitude: f64, minimum: f64) -> DeviceError {
    DeviceError::LimitViolation {
        limit: Limit::Altitude,
        detail: format!(
            "the target is at altitude {altitude:.1}°, below the configured minimum of \
             {minimum:.1}° (mount.limits.min_altitude_degrees)"
        ),
    }
}

/// The refusal a travel check produces (M3-T07).
///
/// Both numbers are in the message, as in [`altitude_violation`], and the homeward direction is
/// too: an operator who is refused needs to know which way is out, in the dark, holding a D-pad
/// that has just stopped responding to one of its buttons.
fn travel_violation(axis: Axis, travelled: f64, limit: f64, homeward: Direction) -> DeviceError {
    DeviceError::LimitViolation {
        limit: Limit::Travel,
        detail: format!(
            "the {} axis is {travelled:.1}° from home, at or past the configured maximum of \
             {limit:.1}° (mount.limits.max_travel_from_home_degrees); slew {} to unwind it",
            axis_name(axis),
            direction_name(homeward)
        ),
    }
}

/// What an operator calls a slew direction.
const fn direction_name(dir: Direction) -> &'static str {
    match dir {
        Direction::North => "north",
        Direction::South => "south",
        Direction::East => "east",
        Direction::West => "west",
    }
}

impl SafeMount {
    /// Wrap `inner` with an explicit set of limits and an observing site.
    ///
    /// Every threshold is a parameter and none is written down in this crate: a limit compiled
    /// into the binary is a limit the operator cannot change when they move the telescope to a
    /// site with a hill on the southern horizon. The binary calls
    /// [`from_config`](Self::from_config); this signature exists so a test can vary the thresholds
    /// and demonstrate that the behaviour follows them.
    #[must_use]
    pub fn new(
        inner: Arc<dyn MountDevice>,
        limits: MountLimits,
        site: Site,
        bus: EventBus,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner,
                limits,
                site,
                bus,
                motion: Mutex::new(Motion::default()),
            }),
        }
    }

    /// Wrap `inner` with the limits and site the operator configured (`mount.limits`, `site`).
    #[must_use]
    pub fn from_config(inner: Arc<dyn MountDevice>, config: &FieldConfig, bus: EventBus) -> Self {
        Self::new(
            inner,
            config.mount.limits,
            Site::from_config(&config.site),
            bus,
        )
    }

    /// The wrapped driver, for callers that need the identity of the thing underneath.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn MountDevice> {
        &self.shared.inner
    }

    /// Where `position` is in the sky as seen from the configured site, right now (MNT-03).
    ///
    /// This is the *same* call the altitude limit makes, which is the point: the alt/az the
    /// operator reads in `mount.position` and the alt/az the limit refuses a slew on cannot
    /// disagree, because there is one implementation and it is [`horizontal::horizontal`].
    #[must_use]
    pub fn horizontal(&self, position: RaDec) -> astroctl_core::types::AltAz {
        self.shared.horizontal_at(position, Utc::now())
    }

    /// Would a goto to `target` be refused by a limit? Asked *before* the API answers `202`.
    ///
    /// # Why this exists rather than only the check inside [`goto`](MountDevice::goto)
    ///
    /// SDD §5.8.1 makes goto a `202 + WS progress` route: the handler starts the slew, answers
    /// immediately and drops the future (a goto can take two minutes). SDD §5.4 puts the altitude
    /// refusal inside the wrapper's `goto`. Those two are incompatible as written — the refusal
    /// happens in the spawned task, *after* the node has already told the operator `202 Accepted`,
    /// so MNT-15's "rejected with 403 `LIMIT_ALTITUDE`" arrives as an alert half a second later
    /// and the answer to the request is a promise that the slew began. Neither section mentions
    /// the other; the gap was found by writing the acceptance test.
    ///
    /// This is the resolution, and it is deliberately *not* a second implementation of the rule:
    /// the check is the same [`Shared::check_altitude`] the enforcing path calls. The route asks
    /// the question early so it can answer it properly; the wrapper still refuses the motion for
    /// every caller, including one that never asks. Removing this method would make the API
    /// answer wrong, not unsafe — which is why it is an inherent method and not a trait one.
    ///
    /// Synchronous and side-effect free: it touches no device and takes no lock.
    ///
    /// # Errors
    /// [`DeviceError::LimitViolation`] if the target is below `min_altitude_degrees`.
    pub fn check_goto(&self, target: RaDec) -> Result<(), DeviceError> {
        self.shared.check_altitude(target, Utc::now())
    }

    /// Resolve a client's requested slew TTL against the configuration (SDD §5.8.1).
    ///
    /// Absent → `slew_ttl_default_ms`; longer than `slew_ttl_max_ms` → clamped, never refused. A
    /// 422 would leave the D-pad dead over a disagreement about how often to renew.
    ///
    /// The *only* implementation of that rule. The `/api/mount/slew` route calls this for the
    /// number it reports back as `expires_in_ms`, so the lease the wrapper is enforcing and the
    /// lease the operator's app is renewing against are the same value by construction.
    #[must_use]
    pub fn resolve_ttl(&self, requested_ms: Option<u64>) -> Duration {
        let limits = &self.shared.limits;
        Duration::from_millis(
            requested_ms
                .unwrap_or(limits.slew_ttl_default_ms)
                .min(limits.slew_ttl_max_ms),
        )
    }

    /// Start or renew a manual slew, authorised for `ttl` (SDD §5.8.1, §5.4).
    ///
    /// # The renewal path touches neither the motors nor the serial line
    ///
    /// A repeat with identical axis, direction and speed while the lease is live moves the
    /// deadline and returns. That is what makes the dead-man's switch affordable: the app renews
    /// at 2 Hz for as long as a thumb is on the D-pad, and a limit check or a motor command on
    /// every renewal would put four extra exchanges per second on a 9600-baud line and make the
    /// mount stutter under the operator's finger.
    ///
    /// # The altitude check is directional
    ///
    /// A manual slew has no target, so the check is on where the axis is *heading*: one degree
    /// along, in the commanded direction. A positional check — "refuse to slew while below the
    /// limit" — would be simpler and would trap the mount below its own horizon limit with no way
    /// to drive it back up, which is the failure an operator meets at 2 a.m. after a goto they
    /// stopped halfway.
    ///
    /// # The travel check here is necessary and **not sufficient**, which is why the watch has one
    ///
    /// The renewal path above returns before any check runs, and it has to: the app renews at
    /// 2 Hz for as long as a thumb is on the D-pad. So a hold that begins inside the travel limit
    /// is never re-examined here however long it lasts — and a hold that lasts is exactly how an
    /// axis reached 215.6° from home. This check catches the press that starts already past the
    /// limit; [`check_limits`] catches the axis winding past it under a thumb that never lifted.
    /// Neither alone is the requirement.
    ///
    /// # Errors
    /// [`DeviceError::LimitViolation`] before anything reaches the device if the motion would
    /// descend below `min_altitude_degrees`, plus whatever the driver itself returns.
    pub async fn slew_for(
        &self,
        axis: Axis,
        dir: Direction,
        speed: SlewSpeed,
        ttl: Duration,
    ) -> Result<(), DeviceError> {
        let now = Instant::now();
        {
            let mut motion = self.shared.lock();
            if let Some(lease) = motion.lease(axis) {
                if lease.dir == dir && lease.speed == speed && lease.deadline > now {
                    motion.axis(axis).replace(Lease {
                        deadline: now + ttl,
                        dir,
                        speed,
                        ttl,
                    });
                    return Ok(());
                }
            }
        }

        // A new authorisation, so the limits are checked — and checked *before* the device is
        // commanded, which is the MNT-15 acceptance criterion. `position()` is the one device
        // call this makes first; it is a read, and a read cannot move a telescope. It also
        // refreshes the counters the travel check reads, which is why that check follows it.
        let from = self.shared.inner.position().await?;
        if let Some(altitude) = self
            .shared
            .descends_below_limit(from, axis, dir, Utc::now())
        {
            return Err(altitude_violation(
                altitude,
                self.shared.limits.min_altitude_degrees,
            ));
        }
        if let Some(travelled) = self.shared.winds_past_limit(axis, dir) {
            return Err(travel_violation(
                axis,
                travelled,
                self.shared.limits.max_travel_from_home_degrees,
                // Safe to unwrap the same way the check did: `winds_past_limit` only answers
                // `Some` when the driver reported travel.
                self.shared
                    .inner
                    .axis_travel()
                    .map_or(dir, |travel| travel.axis(axis).homeward),
            ));
        }

        self.shared.inner.slew(axis, dir, speed).await?;

        let mut motion = self.shared.lock();
        motion.axis(axis).replace(Lease {
            deadline: Instant::now() + ttl,
            dir,
            speed,
            ttl,
        });
        self.ensure_watch(&mut motion);
        Ok(())
    }

    /// Start the background watch if something needs watching and it is not already running.
    ///
    /// Called with the lock held, so the decision and the spawn are one atomic act: a watch that
    /// is exiting because nothing needs watching takes the same lock to clear its own slot, and
    /// the two orders both produce exactly one running watch.
    fn ensure_watch(&self, motion: &mut MutexGuard<'_, Motion>) {
        if motion.watch.is_some() || !motion.needs_watching() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        motion.watch = Some(tokio::spawn(watch(shared)));
    }

    /// Forget every authorisation — used by the stop paths, which are never gated.
    fn release_all(&self) {
        let mut motion = self.shared.lock();
        motion.clear_leases();
        motion.tracking = false;
    }
}

/// Where one degree of motion along `axis` in `dir` would put the tube — **celestial fallback**.
///
/// `None` for a direction that is not on the axis (`slew(Ra, North)`): that is a caller bug and
/// the driver is the layer that refuses it, with a message about the axis rather than about the
/// horizon.
///
/// # This is the wrong frame, and is used only when the right one is unavailable (M3-T08)
///
/// It assumes `north` raises declination. That holds on the normal branch and inverts past the
/// pole, so on a mount that has crossed it this predicts a tube climbing while the real one
/// descends — and because [`Shared::descends_below_limit`]'s guard then never sees a descent, the
/// altitude limit does not merely mis-estimate, it permits unconditionally. Measured on hardware
/// 2026-08-02: a declination slew from the home pose ran the full 180° travel limit with the tube
/// ending 60° below the horizon.
///
/// The fix is not to repair this function — the branch it needs is *not recoverable* from the
/// `RaDec` it is given (SDD §5.4.1). [`MountDevice::motion_lookahead`] is the real predictor and
/// [`Shared::predict`] prefers it; this survives for devices that have no mechanical state to
/// project, where it is not an approximation but the device's actual model: the simulator holds an
/// `RaDec` and moves it, so north really does raise declination there. What it cannot describe is
/// a *German equatorial* behind a driver that will not say which way its metal turns.
fn celestial_lookahead(from: RaDec, axis: Axis, dir: Direction) -> Option<RaDec> {
    let (ra_hours, dec_degrees) = match (axis, dir) {
        // East is the direction in which right ascension increases (the same mapping the API's
        // `positive`/`negative` uses, and the same one the simulator asserts on).
        (Axis::Ra, Direction::East) => (
            from.ra.hours() + LOOKAHEAD_DEGREES / 15.0,
            from.dec.degrees(),
        ),
        (Axis::Ra, Direction::West) => (
            from.ra.hours() - LOOKAHEAD_DEGREES / 15.0,
            from.dec.degrees(),
        ),
        (Axis::Dec, Direction::North) => (
            from.ra.hours(),
            (from.dec.degrees() + LOOKAHEAD_DEGREES).min(90.0),
        ),
        (Axis::Dec, Direction::South) => (
            from.ra.hours(),
            (from.dec.degrees() - LOOKAHEAD_DEGREES).max(-90.0),
        ),
        _ => return None,
    };
    // `RaHours::new` normalises, so stepping east past 24 h is 0 h rather than a failure.
    RaDec::from_parts(ra_hours.rem_euclid(24.0), dec_degrees).ok()
}

// ---------------------------------------------------------------------------------------------
// The background watch
// ---------------------------------------------------------------------------------------------

/// The dead-man's switch and the continuous limit check, in one task (SDD §5.4).
///
/// One task rather than one per axis: the two obligations wake on the same schedule and both need
/// the same position read, and two tasks would take two positions from the device and could act
/// on different ones.
///
/// It runs only while something needs watching and exits otherwise, so an idle node has no timer
/// and a mount that is merely connected costs nothing. It reads the mount's position itself
/// rather than subscribing to the API layer's 1 Hz poll, deliberately: ADD §9.3 layers the safety
/// system *independently of the operator link*, and a limit check that stops working when the
/// telemetry path stops working is not a limit check.
async fn watch(shared: Arc<Shared>) {
    let mut last_hour_angle: Option<f64> = None;

    loop {
        let wake = {
            let mut motion = shared.lock();
            if !motion.needs_watching() {
                // Cleared under the same lock a `slew_for` would take to spawn a replacement, so
                // exactly one watch exists whichever order those two happen in.
                motion.watch = None;
                return;
            }
            let now = Instant::now();
            let ceiling = now + CHECK_INTERVAL;
            // Floored at `MIN_TICK`, so a deadline that is already in the past cannot turn this
            // loop into a busy-spin — see the constant.
            motion
                .next_deadline()
                .map_or(ceiling, |d| d.min(ceiling))
                .max(now + MIN_TICK)
        };
        tokio::time::sleep_until(wake).await;

        expire_leases(&shared).await;
        check_limits(&shared, &mut last_hour_angle).await;
    }
}

/// Stop any axis whose authorisation has run out (SDD §5.8.1, T-SLW-1).
async fn expire_leases(shared: &Arc<Shared>) {
    let now = Instant::now();
    let expired: Vec<(Axis, Lease)> = {
        let mut motion = shared.lock();
        [Axis::Ra, Axis::Dec]
            .into_iter()
            .filter_map(|axis| {
                let lease = motion.lease(axis)?;
                (lease.deadline <= now).then(|| {
                    motion.axis(axis).take();
                    (axis, lease)
                })
            })
            .collect()
    };

    for (axis, lease) in expired {
        // Stop first, alert second. The operator's mount is moving without authorisation and the
        // alert is a description of what was done about it.
        if let Err(error) = shared.inner.stop_slew(axis).await {
            tracing::error!(
                %error, ?axis,
                "the slew TTL expired and the axis could not be stopped — escalating to an \
                 emergency stop"
            );
            // The one place this wrapper escalates on its own. An axis that will not take a
            // ramped stop is the scenario the priority lane exists for, and leaving it turning
            // because the polite stop failed is not an option (REL-02).
            let _ = shared.inner.emergency_stop().await;
        }
        shared.alert(
            ErrorCode::SlewTtlExpired.as_str(),
            format!(
                "the {} axis stopped: no renewal arrived within {} ms of the last one, so the \
                 authorisation to move lapsed",
                axis_name(axis),
                lease.ttl.as_millis()
            ),
        );
    }
}

/// The 2 Hz altitude and meridian checks (MNT-15, MNT-16).
async fn check_limits(shared: &Arc<Shared>, last_hour_angle: &mut Option<f64>) {
    let (leases, tracking) = {
        let motion = shared.lock();
        (
            [(Axis::Ra, motion.ra), (Axis::Dec, motion.dec)],
            motion.tracking,
        )
    };
    if leases.iter().all(|(_, lease)| lease.is_none()) && !tracking {
        return;
    }

    let position = match shared.inner.position().await {
        Ok(position) => position,
        Err(error) => {
            // Not an alert and not a stop. A position read can fail for a beat on a link that is
            // otherwise fine, and the dead-man's switch already bounds how long an unwatched
            // manual slew can continue — at most one TTL, which is 2 s at the configured
            // maximum. Serial-loss-during-motion is the watchdog's escalation (REL-02), not
            // this task's.
            tracing::debug!(%error, "the safety watch could not read the mount's position");
            return;
        }
    };
    let now = Utc::now();

    for (axis, lease) in leases.into_iter().filter_map(|(a, l)| l.map(|l| (a, l))) {
        // Altitude first, travel second, and whichever fires stops the axis: the two are
        // independent reasons to stop the same motion and the operator needs the one that fired.
        let breach = shared
            .descends_below_limit(position, axis, lease.dir, now)
            .map(|altitude| {
                (
                    Limit::Altitude,
                    format!(
                        "the {} axis stopped: continuing would take the telescope to altitude \
                         {altitude:.1}°, below the configured minimum of {:.1}°",
                        axis_name(axis),
                        shared.limits.min_altitude_degrees
                    ),
                )
            })
            .or_else(|| {
                // M3-T07. This is the check that catches a wind-up in progress: the renewal path
                // in `slew_for` short-circuits before any limit runs, so nothing else in the
                // system looks at an axis again once the operator's thumb is down.
                let travelled = shared.winds_past_limit(axis, lease.dir)?;
                Some((
                    Limit::Travel,
                    format!(
                        "the {} axis stopped: it is {travelled:.1}° from the home pose, at the \
                         configured maximum of {:.1}° (mount.limits.max_travel_from_home_degrees). \
                         Slew the other way to unwind it",
                        axis_name(axis),
                        shared.limits.max_travel_from_home_degrees
                    ),
                ))
            });
        let Some((limit, message)) = breach else {
            continue;
        };
        {
            let mut motion = shared.lock();
            motion.axis(axis).take();
        }
        if let Err(error) = shared.inner.stop_slew(axis).await {
            tracing::error!(%error, ?axis, %limit, "could not stop an axis at a motion limit");
        }
        shared.alert(limit.code().as_str(), message);
    }

    if tracking {
        check_meridian(shared, position, now, last_hour_angle).await;
    }
}

/// Stop tracking when the mount crosses the meridian limit (MNT-16).
///
/// # Why a crossing and not a comparison
///
/// The limit protects against the tube driving into the pier, which happens when a German
/// equatorial mount *tracks* past the meridian on the side it started on. Whether a given hour
/// angle is dangerous therefore depends on which side of the pier the tube is — and in Phase 1
/// nothing reports that: SDD §5.2.3 derives `pier_side` from the declination counters in the
/// driver, and neither the simulator nor any Phase 1 driver does so, which is why
/// `mount.position` carries `pier_side: "unknown"`.
///
/// A bare `hour_angle > limit` comparison would therefore stop tracking the instant an operator
/// acquired any target in the western sky — a target that is perfectly safe, because the mount
/// flipped to reach it. Watching for the *crossing* needs no pier side to be correct: the tube was
/// on one side of the limit and is now on the other, under sidereal motion, which is exactly the
/// situation MNT-16 describes. A mount that was already past the limit when tracking started is
/// left alone.
///
/// **This is a Phase 1 approximation and the limit it accepts is real**: a mount that starts
/// tracking already past the limit on the *wrong* side is not protected. When a driver reports
/// pier side (SDD §5.2.3 says the meridian limit consumes it), this becomes a comparison again,
/// with the side deciding the sign.
async fn check_meridian(
    shared: &Arc<Shared>,
    position: RaDec,
    now: DateTime<Utc>,
    last_hour_angle: &mut Option<f64>,
) {
    let hour_angle = horizontal::hour_angle_degrees(position, shared.site, now);
    let limit = shared.limits.meridian_limit_minutes * DEGREES_PER_MINUTE_OF_TIME;
    let previous = last_hour_angle.replace(hour_angle);

    let Some(previous) = previous else { return };
    let step = hour_angle - previous;
    let crossed = previous <= limit
        && hour_angle > limit
        && (0.0..=CROSSING_STEP_CEILING_DEGREES).contains(&step);
    if !crossed {
        return;
    }

    {
        let mut motion = shared.lock();
        motion.tracking = false;
    }
    if let Err(error) = shared.inner.stop_tracking().await {
        tracing::error!(%error, "could not stop tracking at the meridian limit");
    }
    shared.alert(
        Limit::Meridian.code().as_str(),
        format!(
            "tracking stopped: the telescope has reached {:.0} minutes past the meridian, the \
             configured limit (mount.limits.meridian_limit_minutes). Flip the mount or park it \
             before starting again",
            shared.limits.meridian_limit_minutes
        ),
    );
}

/// What an operator calls an axis.
const fn axis_name(axis: Axis) -> &'static str {
    match axis {
        Axis::Ra => "right ascension",
        Axis::Dec => "declination",
    }
}

// ---------------------------------------------------------------------------------------------
// MountDevice
// ---------------------------------------------------------------------------------------------

#[async_trait]
impl MountDevice for SafeMount {
    async fn connect(&self) -> Result<(), DeviceError> {
        self.shared.inner.connect().await
    }

    /// Forwarded ungated, and it forgets every authorisation.
    ///
    /// Not because disconnecting is dangerous — it does not stop the mount, by design (SDD §7) —
    /// but because a lease outliving the link would have the watch stopping an axis it can no
    /// longer reach, once every 500 ms, for as long as the node runs.
    async fn disconnect(&self) -> Result<(), DeviceError> {
        let result = self.shared.inner.disconnect().await;
        self.release_all();
        result
    }

    async fn position(&self) -> Result<RaDec, DeviceError> {
        self.shared.inner.position().await
    }

    async fn status(&self) -> Result<MountStatus, DeviceError> {
        self.shared.inner.status().await
    }

    /// Refuses a target below the horizon limit **before the driver is called** (MNT-15).
    async fn goto(&self, target: RaDec) -> Result<(), DeviceError> {
        self.shared.check_altitude(target, Utc::now())?;
        self.shared.inner.goto(target).await
    }

    async fn sync(&self, pos: RaDec) -> Result<(), DeviceError> {
        self.shared.inner.sync(pos).await
    }

    /// Starts tracking and arms the meridian watch (MNT-16).
    async fn start_tracking(&self, mode: TrackingMode) -> Result<(), DeviceError> {
        self.shared.inner.start_tracking(mode).await?;
        let mut motion = self.shared.lock();
        motion.tracking = true;
        self.ensure_watch(&mut motion);
        Ok(())
    }

    /// A stopping command: never gated, always attempted.
    async fn stop_tracking(&self) -> Result<(), DeviceError> {
        let result = self.shared.inner.stop_tracking().await;
        self.shared.lock().tracking = false;
        result
    }

    /// A manual slew on the configured default TTL.
    ///
    /// The trait carries no TTL — the dead-man's switch is a layer above the HAL — so a caller
    /// reaching this wrapper through `dyn MountDevice` gets `mount.limits.slew_ttl_default_ms`.
    /// The API route calls [`SafeMount::slew_for`] with the operator's own value instead. Both
    /// paths are governed; the difference is only whose number decides the window.
    async fn slew(&self, axis: Axis, dir: Direction, speed: SlewSpeed) -> Result<(), DeviceError> {
        self.slew_for(axis, dir, speed, self.resolve_ttl(None))
            .await
    }

    /// A stopping command: never gated, and it releases the axis's authorisation.
    async fn stop_slew(&self, axis: Axis) -> Result<(), DeviceError> {
        let result = self.shared.inner.stop_slew(axis).await;
        self.shared.lock().axis(axis).take();
        result
    }

    async fn guide_pulse(
        &self,
        axis: Axis,
        dir: Direction,
        duration_ms: u32,
        rate: GuideRate,
    ) -> Result<(), DeviceError> {
        self.shared
            .inner
            .guide_pulse(axis, dir, duration_ms, rate)
            .await
    }

    async fn park(&self) -> Result<(), DeviceError> {
        let result = self.shared.inner.park().await;
        self.release_all();
        result
    }

    async fn unpark(&self) -> Result<(), DeviceError> {
        self.shared.inner.unpark().await
    }

    /// Forwarded verbatim, first, before anything else happens (SDD §5.4, §5.8.2, REL-01).
    ///
    /// The driver call is the **first** statement, ahead of every lock this wrapper owns. That
    /// ordering is the requirement: SDD §5.8.2 budgets 20 ms from handler to wire and §5.2.4 gives
    /// the e-stop its own lane to the serial task precisely so it cannot queue behind an in-flight
    /// position poll. A wrapper that took its own mutex first would reintroduce, one layer up, the
    /// queue the driver went to some trouble to avoid.
    ///
    /// Bookkeeping happens afterwards and cannot fail the call: leases are dropped so the watch
    /// does not issue a stop for an axis that has already been stopped, and the alert tells the
    /// operator's app that the stop landed — which is what the header button renders, since the
    /// UI never treats its own request as evidence of what the mount did.
    async fn emergency_stop(&self) -> Result<(), DeviceError> {
        let result = self.shared.inner.emergency_stop().await;

        self.release_all();
        match &result {
            Ok(()) => self.shared.bus.publish(Alert::critical(
                EMERGENCY_STOP,
                "emergency stop: all motion halted and tracking is off",
            )),
            Err(error) => self.shared.bus.publish(Alert::critical(
                EMERGENCY_STOP,
                format!("emergency stop did not complete: {error}. Cut power to the mount"),
            )),
        };
        result
    }

    /// Forwarded unchanged — the wrapper holds no mechanical state of its own.
    fn pier_side(&self) -> Option<PierSide> {
        self.shared.inner.pier_side()
    }

    /// Forwarded unchanged: the wrapper adds no mechanical knowledge, it only consumes the
    /// driver's. Present so that a caller holding a `SafeMount` as `dyn MountDevice` sees the same
    /// body-frame surface the driver offers (SDD §5.4.2).
    fn motion_lookahead(&self, axis: Axis, dir: Direction, degrees: f64) -> Option<RaDec> {
        self.shared.inner.motion_lookahead(axis, dir, degrees)
    }

    fn axis_travel(&self) -> Option<MountTravel> {
        self.shared.inner.axis_travel()
    }

    fn capabilities(&self) -> MountCapabilities {
        self.shared.inner.capabilities()
    }

    fn device_info(&self) -> DeviceInfo {
        self.shared.inner.device_info()
    }
}
