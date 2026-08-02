//! Event schema — SDD §4.3, ADD §6.2 (SES-07, EXT-05).
//!
//! One pipeline, one serialization. The [`Event`] built here is the *identical* object that a
//! WebSocket client receives and that the session JSONL log stores; there is deliberately no
//! second serialization path, because two paths drift and then the log stops describing what
//! the operator actually saw.
//!
//! # Shape
//!
//! ```json
//! {"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"mount.position","data":{ … }}
//! ```
//!
//! `v` is the schema version (SDD §2: every externally visible JSON schema carries one), `ts`
//! is UTC RFC 3339 **with exactly three fractional digits** (SDD §2), `topic` is the closed
//! [`Topic`] enum in its dotted form, and `data` is the topic's payload struct serialized to
//! `serde_json::Value`.
//!
//! # Why the payloads are typed
//!
//! Every topic in §4.3 has a payload struct in this module implementing [`EventPayload`], which
//! binds the struct to exactly one [`Topic`]. Publishing goes through
//! [`EventBus::publish`](crate::bus::EventBus::publish), which takes an `impl EventPayload` — so
//! a call site cannot pair the wrong topic with the wrong body, cannot misspell a field, and
//! cannot omit one. An open `serde_json::json!` at each call site would compile just as well and
//! would put the schema back in the operator's face at 2 a.m. instead of in the compiler's.

use chrono::{DateTime, SecondsFormat, SubsecRound, Utc};
use serde::{Deserialize, Serialize};

// The rate a mount is tracking at, reused rather than re-declared: `mount.status`'s
// `tracking_mode` must be the same closed set the HAL's `start_tracking` accepts, or the event
// could report a rate no driver can be asked for.
use crate::types::TrackingMode;

/// Event schema version carried in every event's `v` field (SDD §4.3).
pub const EVENT_SCHEMA_VERSION: u16 = 1;

/// The `liveview.frame` row of the SDD §4.3 table.
///
/// It is listed in the event table but is **not** a bus topic and has no [`Topic`] variant on
/// purpose: §4.3 and §8.3(5) both say it is a binary JPEG WebSocket frame carried on the
/// dedicated `/ws/liveview` socket and never on `/ws`. Giving it a `Topic` variant would make
/// `Event { topic: LiveviewFrame, data: <JSON> }` representable, which is precisely the thing
/// the connection-separation rule forbids — a 500 KB image must never share the control stream.
///
/// The name still exists as a constant because the WS hub's subscribe/unsubscribe filter
/// (§5.8.3) is stringly addressed by clients, and M1-T09 needs to recognise this one.
pub const LIVEVIEW_FRAME_TOPIC: &str = "liveview.frame";

/// Current UTC time truncated to millisecond resolution.
///
/// Truncating at construction (rather than only at serialization) means an [`Event`] survives a
/// JSON round-trip byte-identically — a log line read back equals the event that was published,
/// which is what makes the session log usable as a replay source.
#[must_use]
pub fn now_millis() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(3)
}

// ---------------------------------------------------------------------------------------------
// Timestamp serialization (SDD §2)
// ---------------------------------------------------------------------------------------------

/// UTC RFC 3339 with exactly three fractional digits.
///
/// chrono's own `Serialize` for `DateTime<Utc>` uses `SecondsFormat::AutoSi`, which drops the
/// fractional part entirely when it happens to be zero — so one event in a thousand would
/// serialize as `…:05Z` and any consumer parsing a fixed-width timestamp would break on it.
/// SDD §2 says "with milliseconds", so we say it always.
pub mod ts_rfc3339_millis {
    use super::{DateTime, SecondsFormat, Utc};
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    /// Serialize as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    pub fn serialize<S: Serializer>(ts: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&ts.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    /// Parse any RFC 3339 instant, normalizing the offset to UTC.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let raw = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&raw)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(D::Error::custom)
    }
}

/// [`ts_rfc3339_millis`] for optional timestamps; `None` serializes as JSON `null`.
///
/// The key is always present. Payload shape is part of the frozen contract, so an absent value
/// is `null`, never a missing key — a UI that destructures the payload must not have to guess.
pub mod ts_rfc3339_millis_opt {
    use super::{DateTime, SecondsFormat, Utc};
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    /// Serialize `Some` as `YYYY-MM-DDTHH:MM:SS.mmmZ`, `None` as `null`.
    pub fn serialize<S: Serializer>(ts: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match ts {
            Some(ts) => s.serialize_str(&ts.to_rfc3339_opts(SecondsFormat::Millis, true)),
            None => s.serialize_none(),
        }
    }

    /// Parse an optional RFC 3339 instant, normalizing the offset to UTC.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        match raw {
            None => Ok(None),
            Some(raw) => DateTime::parse_from_rfc3339(&raw)
                .map(|dt| Some(dt.with_timezone(&Utc)))
                .map_err(D::Error::custom),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Event and Topic
// ---------------------------------------------------------------------------------------------

/// One operator-visible state change (SDD §4.3).
///
/// The same value is what the WS hub writes to a client and what the JSONL sink appends to the
/// session log (ADD §6.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Schema version; always [`EVENT_SCHEMA_VERSION`] for events this build produces.
    pub v: u16,
    /// UTC, RFC 3339, millisecond resolution (SDD §2).
    #[serde(with = "ts_rfc3339_millis")]
    pub ts: DateTime<Utc>,
    /// Which topic this event belongs to.
    pub topic: Topic,
    /// The topic's payload — see the structs in this module.
    pub data: serde_json::Value,
}

impl Event {
    /// Build an event stamped with the current time.
    ///
    /// Prefer [`EventPayload::into_event`] (or `EventBus::publish`) — this constructor accepts
    /// any `data` and so cannot check the payload against the topic.
    #[must_use]
    pub fn new(topic: Topic, data: serde_json::Value) -> Self {
        Self::at(now_millis(), topic, data)
    }

    /// Build an event with an explicit timestamp (replay, tests, back-dated ingest).
    ///
    /// The timestamp is truncated to milliseconds so it round-trips through JSON unchanged.
    #[must_use]
    pub fn at(ts: DateTime<Utc>, topic: Topic, data: serde_json::Value) -> Self {
        Self {
            v: EVENT_SCHEMA_VERSION,
            ts: ts.trunc_subsecs(3),
            topic,
            data,
        }
    }

    /// Render the single JSONL line for this event, newline included.
    ///
    /// Shared by the session-log sink and by anything replaying a log, so the on-disk form has
    /// exactly one definition.
    ///
    /// # Errors
    ///
    /// Propagates a `serde_json` failure. For events built from this module's payload structs
    /// this cannot happen; the signature stays fallible because `data` is an open `Value`.
    pub fn to_jsonl_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// The closed set of JSON event topics (SDD §4.3).
///
/// Serialized in dotted form (`mount.position`, …). Adding a variant is a change to a frozen
/// contract: it needs an SDD §4.3 edit with a version bump in the same change set
/// (`docs/plan/tasks/README.md` rule 2), not a quiet extension here.
///
/// `liveview.frame` from the §4.3 table is deliberately absent — see [`LIVEVIEW_FRAME_TOPIC`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Topic {
    /// Mount coordinates at 1 Hz (MNT-02). Payload: [`MountPosition`].
    #[serde(rename = "mount.position")]
    MountPosition,
    /// Mount state on change. Payload: [`MountStatus`].
    #[serde(rename = "mount.status")]
    MountStatus,
    /// Camera connection/battery/storage, on change and every 60 s. Payload: [`CameraStatus`].
    #[serde(rename = "camera.status")]
    CameraStatus,
    /// Per-frame capture lifecycle. Payload: [`CaptureProgress`].
    #[serde(rename = "capture.progress")]
    CaptureProgress,
    /// A frame is durable on the field node's disk. Payload: [`FrameSaved`].
    #[serde(rename = "frame.saved")]
    FrameSaved,
    /// The stack node holds and verified a frame (REL-13). Payload: [`TransferAcked`].
    #[serde(rename = "transfer.acked")]
    TransferAcked,
    /// Upload queue state, on change and every 30 s. Payload: [`TransferStatus`].
    #[serde(rename = "transfer.status")]
    TransferStatus,
    /// Stack-node health republished by the field node (USB-06). Payload: [`StackStatus`].
    #[serde(rename = "stack.status")]
    StackStatus,
    /// Operator-facing alert. Payload: [`Alert`].
    #[serde(rename = "alert")]
    Alert,
    /// Field-node vitals every 60 s. Payload: [`SystemHealth`].
    #[serde(rename = "system.health")]
    SystemHealth,
}

impl Topic {
    /// Every topic, in SDD §4.3 table order.
    ///
    /// Iterating this is how the WS hub builds its snapshot set and how tests assert the set is
    /// complete; a new variant that is not added here fails the exhaustiveness test.
    pub const ALL: [Topic; 10] = [
        Topic::MountPosition,
        Topic::MountStatus,
        Topic::CameraStatus,
        Topic::CaptureProgress,
        Topic::FrameSaved,
        Topic::TransferAcked,
        Topic::TransferStatus,
        Topic::StackStatus,
        Topic::Alert,
        Topic::SystemHealth,
    ];

    /// The dotted wire name, identical to the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Topic::MountPosition => "mount.position",
            Topic::MountStatus => "mount.status",
            Topic::CameraStatus => "camera.status",
            Topic::CaptureProgress => "capture.progress",
            Topic::FrameSaved => "frame.saved",
            Topic::TransferAcked => "transfer.acked",
            Topic::TransferStatus => "transfer.status",
            Topic::StackStatus => "stack.status",
            Topic::Alert => "alert",
            Topic::SystemHealth => "system.health",
        }
    }

    /// Whether the topic carries self-superseding telemetry.
    ///
    /// §5.8.3 gives `mount.position` a latest-only slot in the per-client queue: a stale
    /// coordinate has no value once a newer one exists, whereas dropping a `frame.saved` loses
    /// information permanently. The hub (M1) consumes this; it lives here so the classification
    /// sits next to the topic definition instead of being re-derived per consumer.
    #[must_use]
    pub const fn is_coalescable(self) -> bool {
        matches!(self, Topic::MountPosition)
    }

    /// Whether the latest event on this topic belongs in the connect snapshot (§5.8.3).
    ///
    /// A stateful topic reports a value that *is currently true* — where the mount is, whether
    /// the camera is attached, how deep the upload queue is — so a client that just connected
    /// needs the most recent one to render anything at all. The rest are occurrences: an
    /// `alert` fired, a `frame.saved` happened, a `transfer.acked` arrived. Replaying those on
    /// every reconnect would show the operator a warning that has already been dealt with and a
    /// frame notification for a frame saved an hour ago, which on a link that reconnects as
    /// often as this one does (REL-10) is a stream of stale news.
    ///
    /// Lives here beside [`is_coalescable`](Self::is_coalescable) so both classifications sit
    /// next to the topic definitions rather than being re-derived in the hub.
    #[must_use]
    pub const fn is_stateful(self) -> bool {
        match self {
            Topic::MountPosition
            | Topic::MountStatus
            | Topic::CameraStatus
            | Topic::CaptureProgress
            | Topic::TransferStatus
            | Topic::StackStatus
            | Topic::SystemHealth => true,
            Topic::FrameSaved | Topic::TransferAcked | Topic::Alert => false,
        }
    }
}

impl std::fmt::Display for Topic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Binds a payload struct to exactly one [`Topic`].
///
/// Implemented once per row of the §4.3 table. `EventBus::publish` accepts `impl EventPayload`,
/// so the topic is chosen by the type of the payload and never by the call site.
pub trait EventPayload: Serialize {
    /// The topic this payload is published on.
    const TOPIC: Topic;

    /// Wrap the payload in an [`Event`] stamped now.
    #[must_use]
    fn into_event(self) -> Event
    where
        Self: Sized,
    {
        Event::new(Self::TOPIC, payload_to_value(&self))
    }

    /// Wrap the payload in an [`Event`] with an explicit timestamp (replay, tests).
    #[must_use]
    fn into_event_at(self, ts: DateTime<Utc>) -> Event
    where
        Self: Sized,
    {
        Event::at(ts, Self::TOPIC, payload_to_value(&self))
    }
}

/// Serialize a payload struct to `Value`.
///
/// Infallible for every payload in this module — they are plain structs of primitives, strings
/// and unit enums, and `serde_json` has no failure mode for those. It stays non-panicking rather
/// than using `expect` because this runs on the telemetry path: if a future payload grows a
/// custom `Serialize` that can fail, the operator should see a malformed-looking event, not lose
/// the process mid-capture.
fn payload_to_value<P: Serialize>(payload: &P) -> serde_json::Value {
    match serde_json::to_value(payload) {
        Ok(value) => value,
        Err(err) => serde_json::json!({ "_serialization_error": err.to_string() }),
    }
}

// ---------------------------------------------------------------------------------------------
// Payload enums
// ---------------------------------------------------------------------------------------------

/// Which side of the pier the mount's OTA sits on (§5.2.3, derived from DEC counts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PierSide {
    /// Counterweight west of the pier — the mount is looking east.
    East,
    /// Counterweight east of the pier — the mount is looking west, past a meridian flip.
    West,
    /// Pier side is not derivable yet (mount not connected, or position not read).
    Unknown,
}

/// Mount lifecycle state accompanying the motion flags in [`MountStatus`].
///
/// SDD §4.3 names the field but not its value set; this is the minimum that distinguishes the
/// states the Phase 1 API can be in. See the crate-level deviation note in the M0-T03 result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountState {
    /// No driver connection; coordinates are unknown.
    Disconnected,
    /// Connected and stationary (it may still be tracking — see `tracking`).
    Idle,
    /// Executing a goto or a manual slew.
    Slewing,
    /// Parked at the park position; motion refused until unparked.
    Parked,
    /// A device or safety fault needs operator acknowledgement.
    Fault,
}

/// Stage of one frame's capture (SDD §4.3 `capture.progress`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// Shutter open.
    Exposing,
    /// Reading the frame off the camera.
    Downloading,
    /// Frame is durable on disk (fsynced and renamed, §5.3.2).
    Saved,
    /// The preview JPEG for this frame has been generated (§5.7).
    PreviewReady,
}

/// Link state of the transfer agent (SDD §4.3 `transfer.status`, §5.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Nothing queued; the stack node is reachable.
    Idle,
    /// An upload is in flight.
    Uploading,
    /// The stack node is unreachable; the queue is absorbing the backlog (a normal state).
    Offline,
}

/// Compute-worker state as reported by the stack node (§5.12.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// No worker is running and none has been needed — they spawn on demand (§5.12.3), so this
    /// is the stack node's normal idle state, not a fault — and the `Default`, since a
    /// supervisor that has done nothing yet is exactly this.
    ///
    /// Distinct from `StackStatus.worker_state = null`, which means the stack node is
    /// *unreachable* and the state is unknown. Collapsing "idle by design" into that null — or
    /// worse, reporting `Ready` for a process that does not exist — would make the health
    /// surface lie in one direction or the other. Added 2026-07-30 for exactly this reason,
    /// when M1-T13's on-demand supervisor met `StackStatus::online()`'s non-optional state.
    #[default]
    Stopped,
    /// Spawned, handshake not yet complete.
    Starting,
    /// Handshake done, idle, accepting jobs.
    Ready,
    /// Executing a job.
    Busy,
    /// Died or timed out; backing off before respawn.
    Restarting,
    /// Will not be respawned (version mismatch, repeated job-fatal crash).
    Failed,
}

/// Alert severity (SDD §4.3 `alert`).
///
/// Three levels, matching the PWA's green/amber/red header treatment (§5.9). SDD §4.3 does not
/// enumerate them — see the M0-T03 result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    /// Worth recording, needs no action (link recovered, session started).
    Info,
    /// Degraded but operating (disk low, clock unsynced, stack node offline).
    Warning,
    /// Capture or safety is compromised and the operator must act.
    Critical,
}

// ---------------------------------------------------------------------------------------------
// Payload structs, one per §4.3 topic
// ---------------------------------------------------------------------------------------------

/// `mount.position` — `{ra, dec, alt, az, pier_side}` at 1 Hz (MNT-02).
///
/// Fields are private with accessors on purpose: raw public `f64` coordinates are the unit bug
/// SDD §2 and M0-T02 exist to prevent. When M0-T02's `RaHours`/`DecDegrees` newtypes land these
/// become those types — both serialize as bare numbers, so the wire shape does not move.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MountPosition {
    #[serde(rename = "ra")]
    ra_hours: f64,
    #[serde(rename = "dec")]
    dec_degrees: f64,
    /// Altitude, or `null` when nothing has computed the topocentric transform yet.
    ///
    /// Nullable because the mount facade (SDD §5.8) publishes this event and the transform
    /// belongs to the safety monitor (§5.4, M1-T05), which wraps the facade rather than living
    /// inside it. Until it does, the honest value is "not known here" — and a `0.0` would put
    /// the target on the horizon, which is exactly where the altitude limit lives, so the one
    /// field a stand-in value could corrupt is the one that decides whether to refuse a slew.
    #[serde(rename = "alt")]
    alt_degrees: Option<f64>,
    /// Azimuth, or `null`. Nullable for the same reason as `alt_degrees`, and always together
    /// with it: they come from one transform, so one being known and the other not is a state
    /// that cannot arise.
    #[serde(rename = "az")]
    az_degrees: Option<f64>,
    pier_side: PierSide,
    /// Degrees the right-ascension **axis** has been driven from the mechanical home pose, or
    /// `null` from a mount with no home reference (M3-T07).
    ///
    /// # Not a coordinate, and not folded
    ///
    /// A mechanical quantity in a payload that is otherwise about the sky — the same company
    /// `pier_side` keeps, and for the same reason: it is not recoverable from `ra`/`dec`. It is
    /// *accumulated rotation*, so an axis wound 215.6° past home reports 215.6 rather than the
    /// congruent −144.4, because what it exists to tell the operator is how much winding the
    /// cables have taken and not where the tube ended up.
    ///
    /// It is here rather than on `mount.status` because it is a measurement that changes
    /// continuously, and `mount.status` is published on change while this is published every
    /// second — an operator holding a D-pad needs a number that moves while they hold it.
    #[serde(rename = "ra_travel")]
    ra_travel_degrees: Option<f64>,
    /// The declination axis's travel. Nullable together with
    /// [`ra_travel_degrees`](Self::ra_travel_degrees) — they come from one read, so one being
    /// known and the other not is a state that cannot arise.
    #[serde(rename = "dec_travel")]
    dec_travel_degrees: Option<f64>,
}

impl MountPosition {
    /// Build a position report. Units are in the parameter names and are not interchangeable.
    #[must_use]
    pub fn new(
        ra_hours: f64,
        dec_degrees: f64,
        alt_degrees: Option<f64>,
        az_degrees: Option<f64>,
        pier_side: PierSide,
    ) -> Self {
        Self {
            ra_hours,
            dec_degrees,
            alt_degrees,
            az_degrees,
            pier_side,
            ra_travel_degrees: None,
            dec_travel_degrees: None,
        }
    }

    /// Attach the axes' travel from home (M3-T07).
    ///
    /// A separate builder rather than two more positional parameters: `new` already takes four
    /// bare `f64`s in two different units, and a sixth and seventh would be two more chances to
    /// transpose a pair that the compiler cannot tell apart.
    #[must_use]
    pub const fn with_travel(mut self, ra_degrees: f64, dec_degrees: f64) -> Self {
        self.ra_travel_degrees = Some(ra_degrees);
        self.dec_travel_degrees = Some(dec_degrees);
        self
    }

    /// The right-ascension axis's travel from home in degrees, or `None` if unknown.
    #[must_use]
    pub const fn ra_travel_degrees(&self) -> Option<f64> {
        self.ra_travel_degrees
    }

    /// The declination axis's travel from home in degrees, or `None` if unknown.
    #[must_use]
    pub const fn dec_travel_degrees(&self) -> Option<f64> {
        self.dec_travel_degrees
    }

    /// Right ascension, hours.
    #[must_use]
    pub fn ra_hours(&self) -> f64 {
        self.ra_hours
    }

    /// Declination, degrees.
    #[must_use]
    pub fn dec_degrees(&self) -> f64 {
        self.dec_degrees
    }

    /// Altitude in degrees, or `None` until the topocentric transform lands (M1-T05).
    #[must_use]
    pub fn alt_degrees(&self) -> Option<f64> {
        self.alt_degrees
    }

    /// Azimuth in degrees, or `None` until the topocentric transform lands (M1-T05).
    #[must_use]
    pub fn az_degrees(&self) -> Option<f64> {
        self.az_degrees
    }

    /// Pier side.
    #[must_use]
    pub fn pier_side(&self) -> PierSide {
        self.pier_side
    }
}

impl EventPayload for MountPosition {
    const TOPIC: Topic = Topic::MountPosition;
}

/// `mount.status` — `{state, tracking, tracking_mode, slewing, parked}`, on change.
///
/// `slewing` and `parked` are derived from `state` by the constructors, so the payload cannot
/// report `state: "parked"` alongside `parked: false`. §4.3 carries both the state and the flags
/// (the flags are what the PWA binds to); making one the source of the others is what keeps the
/// redundancy from becoming a contradiction. `tracking` is derived from `tracking_mode` for the
/// same reason.
///
/// # Why `tracking_mode` exists (M1-T03; SDD §4.3 change note)
///
/// The payload carried `tracking: bool` and nothing else, so a UI could show *whether* the
/// mount was tracking but not *which rate it settled on*. M1-T04's `TrackingControl` handled
/// that correctly and at a cost: rather than highlight the last button pressed — optimistic UI
/// wearing a different hat, and wrong after any reconnect — it left every rate button
/// unhighlighted and said so in a comment asking for this field.
///
/// The mount is the only thing that knows its rate: a driver may refuse `Lunar` as
/// `Unsupported`, or a goto may suspend tracking and resume it at whatever was running before.
/// Deriving the rate from the last accepted command is therefore guessing, and it guesses wrong
/// in exactly the cases an operator would want to check.
///
/// `tracking` is kept alongside it rather than replaced. It is what most of the UI binds to, a
/// `null`-vs-string check is a worse thing to ask of every consumer than a boolean, and the
/// field is part of the frozen §4.3 payload — removing it would break the PWA that M1-T04
/// already shipped against it, for no gain.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MountStatus {
    state: MountState,
    tracking: bool,
    /// The rate being driven, or `null` when tracking is off.
    ///
    /// A separate field rather than an enum-with-`off` variant because `tracking: false` and
    /// `tracking_mode: null` must agree by construction; see [`MountStatus::new`].
    tracking_mode: Option<TrackingMode>,
    slewing: bool,
    parked: bool,
}

impl MountStatus {
    /// Build a status report from the authoritative state plus the rate being driven.
    ///
    /// `tracking` is derived from `tracking_mode`, so the two cannot contradict each other —
    /// the same discipline `slewing`/`parked` get from `state`.
    #[must_use]
    pub fn new(state: MountState, tracking_mode: Option<TrackingMode>) -> Self {
        Self {
            state,
            tracking: tracking_mode.is_some(),
            tracking_mode,
            slewing: matches!(state, MountState::Slewing),
            parked: matches!(state, MountState::Parked),
        }
    }

    /// No driver connection.
    #[must_use]
    pub fn disconnected() -> Self {
        Self::new(MountState::Disconnected, None)
    }

    /// Lifecycle state.
    #[must_use]
    pub fn state(&self) -> MountState {
        self.state
    }

    /// Whether a tracking rate is being driven.
    #[must_use]
    pub fn tracking(&self) -> bool {
        self.tracking
    }

    /// Which rate is being driven, or `None` when tracking is off.
    #[must_use]
    pub fn tracking_mode(&self) -> Option<TrackingMode> {
        self.tracking_mode
    }

    /// Whether an axis is slewing.
    #[must_use]
    pub fn slewing(&self) -> bool {
        self.slewing
    }

    /// Whether the mount is parked.
    #[must_use]
    pub fn parked(&self) -> bool {
        self.parked
    }
}

impl EventPayload for MountStatus {
    const TOPIC: Topic = Topic::MountStatus;
}

/// `camera.status` — `{connected, battery_pct, charging, storage_free_mb}`, on change + 60 s.
///
/// `battery_pct` and `storage_free_mb` are `null` while disconnected rather than `0`: a zeroed
/// battery renders as an empty gauge, which is a lie the operator would act on.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraStatus {
    connected: bool,
    battery_pct: Option<u8>,
    charging: bool,
    storage_free_mb: Option<u64>,
}

impl CameraStatus {
    /// Camera present and answering.
    #[must_use]
    pub fn connected(battery_pct: u8, charging: bool, storage_free_mb: u64) -> Self {
        Self {
            connected: true,
            battery_pct: Some(battery_pct),
            charging,
            storage_free_mb: Some(storage_free_mb),
        }
    }

    /// No camera; battery and storage are unknown, not zero.
    #[must_use]
    pub fn disconnected() -> Self {
        Self {
            connected: false,
            battery_pct: None,
            charging: false,
            storage_free_mb: None,
        }
    }

    /// Whether the driver holds a live connection.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Battery charge, percent, when known.
    #[must_use]
    pub fn battery_pct(&self) -> Option<u8> {
        self.battery_pct
    }

    /// Whether the camera reports external power.
    #[must_use]
    pub fn charging(&self) -> bool {
        self.charging
    }

    /// Free space on the camera's card, MB, when known.
    #[must_use]
    pub fn storage_free_mb(&self) -> Option<u64> {
        self.storage_free_mb
    }
}

impl EventPayload for CameraStatus {
    const TOPIC: Topic = Topic::CameraStatus;
}

/// `capture.progress` — `{frame_id, state, elapsed_s}`, on change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureProgress {
    frame_id: String,
    state: CaptureState,
    elapsed_s: f64,
}

impl CaptureProgress {
    /// Build a progress report. `frame_id` is the zero-padded per-session id (`light_00042`).
    #[must_use]
    pub fn new(frame_id: impl Into<String>, state: CaptureState, elapsed_s: f64) -> Self {
        Self {
            frame_id: frame_id.into(),
            state,
            elapsed_s,
        }
    }

    /// Shutter open.
    #[must_use]
    pub fn exposing(frame_id: impl Into<String>, elapsed_s: f64) -> Self {
        Self::new(frame_id, CaptureState::Exposing, elapsed_s)
    }

    /// Reading the frame off the camera.
    #[must_use]
    pub fn downloading(frame_id: impl Into<String>, elapsed_s: f64) -> Self {
        Self::new(frame_id, CaptureState::Downloading, elapsed_s)
    }

    /// Frame durable on disk.
    #[must_use]
    pub fn saved(frame_id: impl Into<String>, elapsed_s: f64) -> Self {
        Self::new(frame_id, CaptureState::Saved, elapsed_s)
    }

    /// Preview JPEG generated.
    #[must_use]
    pub fn preview_ready(frame_id: impl Into<String>, elapsed_s: f64) -> Self {
        Self::new(frame_id, CaptureState::PreviewReady, elapsed_s)
    }

    /// Frame id this progress refers to.
    #[must_use]
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }

    /// Capture stage.
    #[must_use]
    pub fn state(&self) -> CaptureState {
        self.state
    }

    /// Seconds since the capture started.
    #[must_use]
    pub fn elapsed_s(&self) -> f64 {
        self.elapsed_s
    }
}

impl EventPayload for CaptureProgress {
    const TOPIC: Topic = Topic::CaptureProgress;
}

/// `frame.saved` — `{frame_id, path, size_bytes, sha256}`, per frame.
///
/// Emitted only after the write-ahead ordering of ADD §9.2 completes (bytes fsynced and renamed),
/// because the transfer agent treats this event as proof of local durability (§5.10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameSaved {
    frame_id: String,
    path: std::path::PathBuf,
    size_bytes: u64,
    sha256: String,
}

impl FrameSaved {
    /// Build a durability report. `sha256` is normalized to lowercase hex so that the transfer
    /// agent's echo comparison against the stack node (§5.10.2) cannot fail on case alone.
    #[must_use]
    pub fn new(
        frame_id: impl Into<String>,
        path: impl Into<std::path::PathBuf>,
        size_bytes: u64,
        sha256: impl AsRef<str>,
    ) -> Self {
        Self {
            frame_id: frame_id.into(),
            path: path.into(),
            size_bytes,
            sha256: sha256.as_ref().to_ascii_lowercase(),
        }
    }

    /// Frame id.
    #[must_use]
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }

    /// Absolute path of the frame in the session directory.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Frame size in bytes.
    #[must_use]
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// Lowercase hex SHA-256 of the frame.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

impl EventPayload for FrameSaved {
    const TOPIC: Topic = Topic::FrameSaved;
}

/// `transfer.acked` — `{frame_id, sha256, acked_at, queue_depth}`, per frame.
///
/// The only event that makes a field-node frame reclaim-eligible (REL-13): it means the stack
/// node has the bytes on disk, fsynced, checksum matched (§5.11).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferAcked {
    frame_id: String,
    sha256: String,
    #[serde(with = "ts_rfc3339_millis")]
    acked_at: DateTime<Utc>,
    queue_depth: u64,
}

impl TransferAcked {
    /// Build an ack report; `acked_at` is truncated to milliseconds like every other timestamp.
    #[must_use]
    pub fn new(
        frame_id: impl Into<String>,
        sha256: impl AsRef<str>,
        acked_at: DateTime<Utc>,
        queue_depth: u64,
    ) -> Self {
        Self {
            frame_id: frame_id.into(),
            sha256: sha256.as_ref().to_ascii_lowercase(),
            acked_at: acked_at.trunc_subsecs(3),
            queue_depth,
        }
    }

    /// Frame id.
    #[must_use]
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }

    /// Lowercase hex SHA-256 echoed by the stack node and verified against ours.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// When the ack was recorded.
    #[must_use]
    pub fn acked_at(&self) -> DateTime<Utc> {
        self.acked_at
    }

    /// Rows still `queued` after this ack.
    #[must_use]
    pub fn queue_depth(&self) -> u64 {
        self.queue_depth
    }
}

impl EventPayload for TransferAcked {
    const TOPIC: Topic = Topic::TransferAcked;
}

/// `transfer.status` — `{state, queue_depth, oldest_queued_age_s, last_ack_ts}`, on change + 30 s.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransferStatus {
    state: TransferState,
    queue_depth: u64,
    /// `null` when the queue is empty — there is no oldest item, and `0` would read as "fresh".
    oldest_queued_age_s: Option<f64>,
    #[serde(with = "ts_rfc3339_millis_opt")]
    last_ack_ts: Option<DateTime<Utc>>,
}

impl TransferStatus {
    /// Build a queue report.
    #[must_use]
    pub fn new(
        state: TransferState,
        queue_depth: u64,
        oldest_queued_age_s: Option<f64>,
        last_ack_ts: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            state,
            queue_depth,
            oldest_queued_age_s,
            last_ack_ts: last_ack_ts.map(|ts| ts.trunc_subsecs(3)),
        }
    }

    /// Link/queue state.
    #[must_use]
    pub fn state(&self) -> TransferState {
        self.state
    }

    /// Rows in `queued` state.
    #[must_use]
    pub fn queue_depth(&self) -> u64 {
        self.queue_depth
    }

    /// Age of the oldest queued frame, seconds, when the queue is non-empty.
    #[must_use]
    pub fn oldest_queued_age_s(&self) -> Option<f64> {
        self.oldest_queued_age_s
    }

    /// When the last ack arrived, if any ack has.
    #[must_use]
    pub fn last_ack_ts(&self) -> Option<DateTime<Utc>> {
        self.last_ack_ts
    }
}

impl EventPayload for TransferStatus {
    const TOPIC: Topic = Topic::TransferStatus;
}

/// `stack.status` — `{connected, session_frame_count, last_preview_ts, worker_state, restarts}`,
/// on change + 30 s.
///
/// Republished by the field node from the stack node's health so the PWA has one event source
/// (USB-06). While the stack node is unreachable the field node has no worker state to report,
/// so `worker_state` is `null` rather than a guess.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StackStatus {
    connected: bool,
    session_frame_count: u64,
    #[serde(with = "ts_rfc3339_millis_opt")]
    last_preview_ts: Option<DateTime<Utc>>,
    worker_state: Option<WorkerState>,
    restarts: u32,
}

impl StackStatus {
    /// Stack node reachable and reporting.
    #[must_use]
    pub fn online(
        session_frame_count: u64,
        last_preview_ts: Option<DateTime<Utc>>,
        worker_state: WorkerState,
        restarts: u32,
    ) -> Self {
        Self {
            connected: true,
            session_frame_count,
            last_preview_ts: last_preview_ts.map(|ts| ts.trunc_subsecs(3)),
            worker_state: Some(worker_state),
            restarts,
        }
    }

    /// Stack node unreachable; the counts are the last known ones, the worker state is unknown.
    #[must_use]
    pub fn offline(session_frame_count: u64, last_preview_ts: Option<DateTime<Utc>>) -> Self {
        Self {
            connected: false,
            session_frame_count,
            last_preview_ts: last_preview_ts.map(|ts| ts.trunc_subsecs(3)),
            worker_state: None,
            restarts: 0,
        }
    }

    /// Whether the field node currently reaches the stack node.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Frames the stack node holds for the current session.
    #[must_use]
    pub fn session_frame_count(&self) -> u64 {
        self.session_frame_count
    }

    /// When the stack node last produced a preview.
    #[must_use]
    pub fn last_preview_ts(&self) -> Option<DateTime<Utc>> {
        self.last_preview_ts
    }

    /// Compute-worker state, when known.
    #[must_use]
    pub fn worker_state(&self) -> Option<WorkerState> {
        self.worker_state
    }

    /// Worker restarts so far — the quiet failure §5.12.3 wants visible.
    #[must_use]
    pub fn restarts(&self) -> u32 {
        self.restarts
    }
}

impl EventPayload for StackStatus {
    const TOPIC: Topic = Topic::StackStatus;
}

/// `alert` — `{severity, code, message}`, as needed.
///
/// `code` is the stable machine-readable identifier the UI switches on (`SLEW_TTL_EXPIRED`,
/// `DISK_FULL`, …); `message` is the human sentence and may be reworded freely. Once M0-T02's
/// closed `ErrorCode` enum lands, `code` becomes that type — it serializes as the same
/// SCREAMING_SNAKE_CASE string, so the wire shape does not move.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    severity: AlertSeverity,
    code: String,
    message: String,
}

impl Alert {
    /// Build an alert. `code` is normalized to uppercase so a lowercase call site cannot create
    /// a second spelling of an existing code.
    #[must_use]
    pub fn new(severity: AlertSeverity, code: impl AsRef<str>, message: impl Into<String>) -> Self {
        Self {
            severity,
            code: code.as_ref().to_ascii_uppercase(),
            message: message.into(),
        }
    }

    /// Informational.
    #[must_use]
    pub fn info(code: impl AsRef<str>, message: impl Into<String>) -> Self {
        Self::new(AlertSeverity::Info, code, message)
    }

    /// Degraded but operating.
    #[must_use]
    pub fn warning(code: impl AsRef<str>, message: impl Into<String>) -> Self {
        Self::new(AlertSeverity::Warning, code, message)
    }

    /// Capture or safety compromised.
    #[must_use]
    pub fn critical(code: impl AsRef<str>, message: impl Into<String>) -> Self {
        Self::new(AlertSeverity::Critical, code, message)
    }

    /// Severity.
    #[must_use]
    pub fn severity(&self) -> AlertSeverity {
        self.severity
    }

    /// Machine-readable code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Operator-facing message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl EventPayload for Alert {
    const TOPIC: Topic = Topic::Alert;
}

/// `system.health` — `{disk_free_gb, clock_synced, uptime_s}`, every 60 s.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemHealth {
    disk_free_gb: f64,
    clock_synced: bool,
    uptime_s: u64,
}

impl SystemHealth {
    /// Build a vitals report.
    #[must_use]
    pub fn new(disk_free_gb: f64, clock_synced: bool, uptime_s: u64) -> Self {
        Self {
            disk_free_gb,
            clock_synced,
            uptime_s,
        }
    }

    /// Free space on the session volume, GB (REL-12 thresholds compare against this).
    #[must_use]
    pub fn disk_free_gb(&self) -> f64 {
        self.disk_free_gb
    }

    /// Whether the system clock is disciplined (REL-14).
    #[must_use]
    pub fn clock_synced(&self) -> bool {
        self.clock_synced
    }

    /// Process uptime, seconds.
    #[must_use]
    pub fn uptime_s(&self) -> u64 {
        self.uptime_s
    }
}

impl EventPayload for SystemHealth {
    const TOPIC: Topic = Topic::SystemHealth;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 29, 21, 4, 5)
            .single()
            .expect("2026-07-29T21:04:05Z is a valid unambiguous UTC instant")
            + chrono::Duration::milliseconds(123)
    }

    /// Golden: the exact bytes a WS client and the session log both see.
    ///
    /// `data` keys are alphabetical because `serde_json::Value`'s object is a `BTreeMap` — that
    /// ordering is part of the golden precisely so a switch to `preserve_order` shows up here.
    #[test]
    fn golden_mount_position_event() {
        let event = MountPosition::new(5.5675, -5.3897, Some(41.25), Some(132.75), PierSide::West)
            .with_travel(215.6, 12.0)
            .into_event_at(fixed_ts());

        assert_eq!(
            serde_json::to_string(&event).expect("event serializes"),
            r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"mount.position","data":{"alt":41.25,"az":132.75,"dec":-5.3897,"dec_travel":12.0,"pier_side":"west","ra":5.5675,"ra_travel":215.6}}"#
        );
    }

    /// Travel is **not** folded on the wire, pinned as a golden because a serializer or a later
    /// refactor that normalised it would be silently wrong (M3-T07).
    ///
    /// 215.6° and −144.4° are the same mechanical angle and very different amounts of cable. The
    /// number exists to tell an operator how much winding the axis has taken, so a value past
    /// half a turn has to survive the trip to the browser as a value past half a turn.
    #[test]
    fn mount_position_travel_past_half_a_turn_reaches_the_wire_unfolded() {
        let event = MountPosition::new(0.0, 90.0, None, None, PierSide::Unknown)
            .with_travel(215.6, 0.0)
            .into_event_at(fixed_ts());
        let json = serde_json::to_string(&event).expect("event serializes");
        assert!(json.contains(r#""ra_travel":215.6"#), "{json}");
        assert!(!json.contains("144.4"), "{json}");
    }

    /// The shape the mount facade emits until M1-T05's topocentric transform wraps it.
    ///
    /// Pinned as a golden because `null` here is a contract, not an accident: the PWA renders
    /// "—" for an unknown altitude, and a serializer that skipped the key instead would make the
    /// field indistinguishable from a node too old to have it. The same holds for
    /// `ra_travel`/`dec_travel`, which are `null` from a mount with no home reference — the
    /// simulator, or any INDI or Alpaca device (M3-T07).
    #[test]
    fn golden_mount_position_with_no_horizontal_transform() {
        let event = MountPosition::new(5.5675, -5.3897, None, None, PierSide::Unknown)
            .into_event_at(fixed_ts());

        assert_eq!(
            serde_json::to_string(&event).expect("event serializes"),
            r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"mount.position","data":{"alt":null,"az":null,"dec":-5.3897,"dec_travel":null,"pier_side":"unknown","ra":5.5675,"ra_travel":null}}"#
        );
    }

    /// Golden: one line per remaining topic, so every payload shape is pinned, not just one.
    #[test]
    fn golden_every_topic_payload() {
        let ts = fixed_ts();
        let cases: Vec<(Event, &str)> = vec![
            (
                MountStatus::new(MountState::Slewing, Some(TrackingMode::Sidereal))
                    .into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"mount.status","data":{"parked":false,"slewing":true,"state":"slewing","tracking":true,"tracking_mode":"sidereal"}}"#,
            ),
            // Tracking off: `tracking_mode` is `null`, not absent. Same contract as `alt`/`az`.
            (
                MountStatus::new(MountState::Idle, None).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"mount.status","data":{"parked":false,"slewing":false,"state":"idle","tracking":false,"tracking_mode":null}}"#,
            ),
            (
                CameraStatus::connected(87, false, 51_200).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"camera.status","data":{"battery_pct":87,"charging":false,"connected":true,"storage_free_mb":51200}}"#,
            ),
            (
                CameraStatus::disconnected().into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"camera.status","data":{"battery_pct":null,"charging":false,"connected":false,"storage_free_mb":null}}"#,
            ),
            (
                CaptureProgress::preview_ready("light_00042", 12.5).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"capture.progress","data":{"elapsed_s":12.5,"frame_id":"light_00042","state":"preview_ready"}}"#,
            ),
            (
                FrameSaved::new(
                    "light_00042",
                    "/srv/sessions/s/frames/light_00042.cr3",
                    25_600_000,
                    "ABCD",
                )
                .into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"frame.saved","data":{"frame_id":"light_00042","path":"/srv/sessions/s/frames/light_00042.cr3","sha256":"abcd","size_bytes":25600000}}"#,
            ),
            (
                TransferAcked::new("light_00042", "abcd", ts, 3).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"transfer.acked","data":{"acked_at":"2026-07-29T21:04:05.123Z","frame_id":"light_00042","queue_depth":3,"sha256":"abcd"}}"#,
            ),
            (
                TransferStatus::new(TransferState::Uploading, 3, Some(42.0), Some(ts))
                    .into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"transfer.status","data":{"last_ack_ts":"2026-07-29T21:04:05.123Z","oldest_queued_age_s":42.0,"queue_depth":3,"state":"uploading"}}"#,
            ),
            (
                TransferStatus::new(TransferState::Idle, 0, None, None).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"transfer.status","data":{"last_ack_ts":null,"oldest_queued_age_s":null,"queue_depth":0,"state":"idle"}}"#,
            ),
            (
                StackStatus::online(12, Some(ts), WorkerState::Busy, 2).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"stack.status","data":{"connected":true,"last_preview_ts":"2026-07-29T21:04:05.123Z","restarts":2,"session_frame_count":12,"worker_state":"busy"}}"#,
            ),
            (
                StackStatus::offline(12, None).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"stack.status","data":{"connected":false,"last_preview_ts":null,"restarts":0,"session_frame_count":12,"worker_state":null}}"#,
            ),
            (
                Alert::warning("slew_ttl_expired", "manual slew stopped: no renewal")
                    .into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"alert","data":{"code":"SLEW_TTL_EXPIRED","message":"manual slew stopped: no renewal","severity":"warning"}}"#,
            ),
            (
                SystemHealth::new(180.5, true, 3600).into_event_at(ts),
                r#"{"v":1,"ts":"2026-07-29T21:04:05.123Z","topic":"system.health","data":{"clock_synced":true,"disk_free_gb":180.5,"uptime_s":3600}}"#,
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(
                serde_json::to_string(&event).expect("event serializes"),
                expected,
                "golden mismatch for topic {}",
                event.topic
            );
        }
    }

    /// The topic set is the frozen contract of SDD §4.3 — assert the strings, not just the count.
    #[test]
    fn topic_strings_match_sdd_table() {
        let expected = [
            "mount.position",
            "mount.status",
            "camera.status",
            "capture.progress",
            "frame.saved",
            "transfer.acked",
            "transfer.status",
            "stack.status",
            "alert",
            "system.health",
        ];
        assert_eq!(Topic::ALL.len(), expected.len());
        for (topic, name) in Topic::ALL.iter().zip(expected) {
            assert_eq!(topic.as_str(), name);
            assert_eq!(topic.to_string(), name);
            // `as_str` and the serde representation must never diverge.
            assert_eq!(
                serde_json::to_string(topic).expect("topic serializes"),
                format!("\"{name}\"")
            );
            assert_eq!(
                serde_json::from_str::<Topic>(&format!("\"{name}\"")).expect("topic parses"),
                *topic
            );
        }
    }

    /// `liveview.frame` is in the §4.3 table but is not a bus topic (§8.3(5)).
    #[test]
    fn liveview_frame_is_not_a_bus_topic() {
        assert_eq!(LIVEVIEW_FRAME_TOPIC, "liveview.frame");
        assert!(serde_json::from_str::<Topic>(r#""liveview.frame""#).is_err());
        assert!(!Topic::ALL
            .iter()
            .any(|t| t.as_str() == LIVEVIEW_FRAME_TOPIC));
    }

    /// SDD §2: milliseconds always, including when they are zero.
    #[test]
    fn timestamp_always_carries_three_fractional_digits() {
        let whole_second = Utc
            .with_ymd_and_hms(2026, 7, 29, 21, 4, 5)
            .single()
            .expect("valid instant");
        let event = SystemHealth::new(1.0, true, 1).into_event_at(whole_second);
        let json = serde_json::to_string(&event).expect("event serializes");
        assert!(
            json.contains(r#""ts":"2026-07-29T21:04:05.000Z""#),
            "expected zero-padded milliseconds, got {json}"
        );
    }

    /// Sub-millisecond input is truncated at construction so the log round-trips exactly.
    #[test]
    fn event_round_trips_through_json() {
        let ts = fixed_ts() + chrono::Duration::microseconds(456);
        let event = Alert::critical("DISK_FULL", "session volume below critical").into_event_at(ts);
        let line = event.to_jsonl_line().expect("line renders");
        assert!(line.ends_with('\n'));

        let parsed: Event = serde_json::from_str(line.trim_end()).expect("line parses");
        assert_eq!(parsed, event);
        assert_eq!(parsed.ts, fixed_ts());
        assert_eq!(parsed.v, EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn payload_topic_binding_is_exhaustive() {
        // One payload per topic; if a topic gains no payload the match below stops compiling.
        for topic in Topic::ALL {
            let built = match topic {
                Topic::MountPosition => {
                    MountPosition::new(0.0, 0.0, None, None, PierSide::Unknown).into_event()
                }
                Topic::MountStatus => MountStatus::disconnected().into_event(),
                Topic::CameraStatus => CameraStatus::disconnected().into_event(),
                Topic::CaptureProgress => {
                    CaptureProgress::exposing("light_00001", 0.0).into_event()
                }
                Topic::FrameSaved => FrameSaved::new("light_00001", "/tmp/f", 1, "ab").into_event(),
                Topic::TransferAcked => {
                    TransferAcked::new("light_00001", "ab", now_millis(), 0).into_event()
                }
                Topic::TransferStatus => {
                    TransferStatus::new(TransferState::Idle, 0, None, None).into_event()
                }
                Topic::StackStatus => StackStatus::offline(0, None).into_event(),
                Topic::Alert => Alert::info("OK", "fine").into_event(),
                Topic::SystemHealth => SystemHealth::new(1.0, true, 1).into_event(),
            };
            assert_eq!(built.topic, topic);
            assert!(built.data.is_object(), "{topic} payload must be an object");
        }
    }

    #[test]
    fn mount_status_flags_cannot_contradict_state() {
        let parked = MountStatus::new(MountState::Parked, None);
        assert!(parked.parked() && !parked.slewing());

        let slewing = MountStatus::new(MountState::Slewing, Some(TrackingMode::Sidereal));
        assert!(slewing.slewing() && !slewing.parked() && slewing.tracking());

        // The rate and the boolean are one field's two views, so they cannot disagree.
        for mode in [
            None,
            Some(TrackingMode::Sidereal),
            Some(TrackingMode::Lunar),
            Some(TrackingMode::Solar),
        ] {
            let status = MountStatus::new(MountState::Idle, mode);
            assert_eq!(status.tracking(), mode.is_some());
            assert_eq!(status.tracking_mode(), mode);
        }

        assert_eq!(
            MountStatus::disconnected().state(),
            MountState::Disconnected
        );
    }

    #[test]
    fn only_mount_position_coalesces() {
        for topic in Topic::ALL {
            assert_eq!(topic.is_coalescable(), topic == Topic::MountPosition);
        }
    }

    #[test]
    fn the_snapshot_set_is_the_seven_stateful_topics() {
        // Pinned as a literal list rather than derived, because this set is a wire contract: the
        // PWA reduces a topic *absent* from the snapshot back to unknown, so moving a topic
        // between these two groups silently changes what a reconnecting client displays.
        let stateful: Vec<Topic> = Topic::ALL.into_iter().filter(|t| t.is_stateful()).collect();
        assert_eq!(
            stateful,
            vec![
                Topic::MountPosition,
                Topic::MountStatus,
                Topic::CameraStatus,
                Topic::CaptureProgress,
                Topic::TransferStatus,
                Topic::StackStatus,
                Topic::SystemHealth,
            ]
        );
        // The three occurrences, named so a reader sees why each is excluded.
        for occurrence in [Topic::Alert, Topic::FrameSaved, Topic::TransferAcked] {
            assert!(
                !occurrence.is_stateful(),
                "{occurrence} is something that happened, not something that is true"
            );
        }
    }
}
