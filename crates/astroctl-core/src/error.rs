//! Error model — SDD §4.2.
//!
//! Three types, in the order a failure travels:
//!
//! * [`CoreError`] — a value rejected by this crate's own constructors (§4.1).
//! * [`DeviceError`] — what every HAL trait method returns (§4.2, verbatim). Drivers speak
//!   this and nothing else; the strings are operator-facing (SDD §2 "transparency").
//! * [`ApiError`] — the JSON envelope every non-2xx response carries, keyed by the closed
//!   [`ErrorCode`] enum the PWA switches on.
//!
//! The HTTP mapping of §4.2 is [`ErrorCode::http_status`] — a total function over the enum,
//! so a new code cannot be added without deciding its status, and the UI's contract cannot
//! drift from the server's behaviour without a compile error here.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::DeviceKind;

// ---------------------------------------------------------------------------------------
// CoreError
// ---------------------------------------------------------------------------------------

/// A value rejected by a domain-type constructor (SDD §4.1).
///
/// Not a device failure and not a transport failure — the caller handed this crate a number
/// that is not a coordinate. It maps to [`ErrorCode::Validation`] (422).
#[derive(thiserror::Error, Debug, Clone)]
pub enum CoreError {
    /// A coordinate, angle or rate outside its domain, or not a finite number.
    #[error("invalid {quantity}: {value} is not {expected}")]
    InvalidCoordinate {
        /// Which quantity was rejected, e.g. `"declination"`.
        quantity: &'static str,
        /// The offending value, echoed so the operator sees what was actually sent.
        value: f64,
        /// The domain it failed, phrased to complete "… is not {expected}".
        expected: &'static str,
    },
}

impl CoreError {
    /// Builds an [`CoreError::InvalidCoordinate`]; the constructors' single entry point.
    #[must_use]
    pub const fn invalid_coordinate(
        quantity: &'static str,
        value: f64,
        expected: &'static str,
    ) -> Self {
        Self::InvalidCoordinate {
            quantity,
            value,
            expected,
        }
    }
}

// ---------------------------------------------------------------------------------------
// DeviceError — SDD §4.2, verbatim
// ---------------------------------------------------------------------------------------

/// What every HAL trait method returns (SDD §4.2).
///
/// The variants and their messages are a frozen contract shared with the UI — see
/// [`ErrorCode::from_device_error`] for how each one reaches the wire.
#[derive(thiserror::Error, Debug)]
pub enum DeviceError {
    /// The device is not connected.
    #[error("device not connected")]
    NotConnected,
    /// The device did not answer within its per-operation timeout.
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    /// A well-formed exchange the driver could not make sense of.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The device understood the command and refused it.
    #[error("device rejected command: {0}")]
    Rejected(String),
    /// The serial/USB layer below the protocol failed.
    #[error("transport error: {0}")]
    Transport(String),
    /// The device does not implement this capability.
    #[error("unsupported by this device")]
    Unsupported,
    /// The device is busy with something that must finish first, e.g. a slew.
    #[error("busy: {0}")]
    Busy(&'static str),
    /// A motion that had started was stopped before it finished — an emergency stop, a safety
    /// limit, or an operator stop took the axes.
    ///
    /// Added by M1-T03 because the alternative was `Rejected`, which maps to
    /// `DEVICE_REJECTED`/422 and therefore tells the operator their *request* was bad at the
    /// exact moment the truth is that their e-stop worked. The simulator's author flagged this
    /// in the M1-T02 handoff: "there is no `Aborted`" was a gap in this enum, not a driver
    /// quirk. Nothing about an aborted goto is a client error, so it must not be a 4xx that
    /// blames the client — see [`ErrorCode::Aborted`].
    ///
    /// The string names what took the axes, so an operator reading a log can tell an e-stop
    /// from a meridian limit without correlating timestamps.
    #[error("aborted: {0}")]
    Aborted(String),
    /// A configured safety limit refused the motion **before it reached the device** (SDD §5.4,
    /// MNT-15/MNT-16).
    ///
    /// Added by M1-T05 for the same reason M1-T03 added [`Aborted`](Self::Aborted): the nearest
    /// existing variant was `Rejected`, which maps to `DEVICE_REJECTED`/422 and would tell the
    /// operator "the mount refused your command" when the mount was never asked — and, worse,
    /// would hide the one fact they need, which is *which limit* and therefore what to change.
    /// [`ErrorCode::LimitAltitude`] and [`ErrorCode::LimitMeridian`] already existed in this
    /// file with a 403 mapping and no producer; this is the producer.
    ///
    /// **No driver may return this.** Drivers do no limit checking (see the `astroctl-hal`
    /// mount docs) — mechanical limits the device itself enforces are `Rejected`. The only
    /// producer is `astroctl-safety`'s `SafeMount`, which implements the same trait and wraps
    /// the driver (ADR-11).
    #[error("{limit} limit: {detail}")]
    LimitViolation {
        /// Which limit refused it.
        limit: Limit,
        /// What was compared against what, in the operator's units.
        detail: String,
    },
}

/// A configured motion limit (SDD §5.4).
///
/// Two variants because they are two different operator actions: an altitude refusal means
/// "point somewhere higher", a meridian stop means "the mount is about to hit the pier". One
/// shared code would make the alert and the 403 say the same undifferentiated thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Below `mount.limits.min_altitude_degrees` (MNT-15).
    Altitude,
    /// Past `mount.limits.meridian_limit_minutes` (MNT-16).
    Meridian,
    /// Further from the mechanical home pose than `mount.limits.max_travel_from_home_degrees`
    /// (M3-T07).
    ///
    /// A third variant rather than a reuse, for the reason the first two are separate: the
    /// operator action is different again. Altitude says "point higher", meridian says "flip or
    /// park", and this one says "you are winding the cables — drive back toward home". A refusal
    /// that borrowed one of the other codes would send the operator to the wrong remedy while an
    /// axis is a quarter-turn from where they think it is.
    Travel,
    /// The rig would reach the tripod or the ground (SDD §5.4.3, Layer 2).
    ///
    /// A fourth variant for the reason the first three are separate: the remedy is different
    /// again, and it is the most urgent of the four. Altitude says "point higher", meridian says
    /// "flip or park", travel says "drive back toward home" — this one says "stop, the metal is
    /// about to touch something", and the operator needs to look at the rig rather than at the
    /// screen.
    Collision,
}

impl Limit {
    /// The wire code this limit is reported as — a 403 in both cases (SDD §4.2).
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::Altitude => ErrorCode::LimitAltitude,
            Self::Meridian => ErrorCode::LimitMeridian,
            Self::Travel => ErrorCode::LimitTravel,
            Self::Collision => ErrorCode::LimitCollision,
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Altitude => "altitude",
            Self::Meridian => "meridian",
            Self::Travel => "travel-from-home",
            Self::Collision => "collision",
        })
    }
}

// ---------------------------------------------------------------------------------------
// ErrorCode — the closed enum shared with the UI
// ---------------------------------------------------------------------------------------

/// The closed set of machine-readable error codes (SDD §4.2).
///
/// Serialized in `SCREAMING_SNAKE_CASE`; [`ErrorCode::as_str`] is the same spelling and is
/// asserted equal to the serde form by test, so log lines and JSON never disagree.
///
/// This is a frozen contract: the PWA switches on these strings to choose what to tell the
/// operator. Adding a variant is an additive change that requires a status and a retryable
/// decision in the two `match`es below; removing or renaming one is a breaking change.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    // --- device state (409) ---
    /// A device operation was attempted while disconnected.
    NotConnected,
    /// The device does not implement the requested capability.
    Unsupported,
    /// The device or the orchestrator is busy with a conflicting operation.
    Busy,
    /// A motion that had started was stopped before it finished (M1-T03; SDD §4.2).
    ///
    /// The 409 twin of [`Cancelled`](Self::Cancelled), and separate from it for the reason
    /// §4.2 keeps `WORKER_CRASHED` separate from `INTERNAL`: the two send the operator to
    /// different places. `CANCELLED` is a *stacking job* the supervisor gave up on; `ABORTED`
    /// is the *telescope* stopping mid-slew because an e-stop, a safety limit or a manual stop
    /// took the axes. Rendering worker copy for an e-stop would be worse than saying nothing.
    ///
    /// Deliberately **not** retryable. The request was valid, so 422 would be a lie — but the
    /// thing that stopped the motion was almost always a safety action, and a client that
    /// re-issued the goto automatically would drive the mount straight back into it.
    Aborted,
    // --- device communication (502) ---
    /// The mount stopped answering altogether — REL-02's watchdog verdict (M1-T17).
    ///
    /// Distinct from [`MountTimeout`](Self::MountTimeout), and the distinction is the whole
    /// point. One missed reply is a timeout: the operator's move is to wait a second and look
    /// again, and the facade already retries on their behalf. `mount.serial.heartbeat_misses`
    /// consecutive misses is a *link*, and their move is to walk out to the rig — the tube may
    /// still be slewing, because nothing about an unplugged cable stops a motor.
    ///
    /// Alert-only in M1: the watchdog publishes it on `alert`, and no route returns it. The
    /// status and retryable answers below are therefore hypothetical, and are the ones
    /// `DEVICE_TRANSPORT` already gives for the same condition — a lost link is worth retrying,
    /// which is what a reconnect is.
    MountLinkLost,
    /// The mount did not respond in time.
    MountTimeout,
    /// The camera did not respond in time.
    CameraTimeout,
    /// Some other device did not respond in time.
    DeviceTimeout,
    /// The serial/USB transport to a device failed.
    DeviceTransport,
    /// A device answered with something the driver could not parse.
    DeviceProtocol,
    /// The *other node* is not answering — the `/stack/*` proxy (ADR-07) and the transfer agent
    /// (SDD §5.10).
    ///
    /// §4.2 has defined this since change note 1.9.0 and nothing implemented it, so the proxy
    /// answered `DEVICE_TRANSPORT` — precisely the conflation the row was added to prevent:
    /// "conflating them tells the operator to check a cable when the problem is a tunnel". Found
    /// by M1-T14, which needs the stack panel to distinguish "the stacking server is down" from
    /// "a cable came out" while a capture is running.
    NodeUnreachable,
    // --- rejected input (422) ---
    /// The device understood the command and refused it.
    DeviceRejected,
    /// The request body or parameters failed validation, coordinates included.
    Validation,
    /// A motion-initiating command arrived older than `max_command_age_ms` (SDD §5.8.1).
    CommandStale,
    /// An uploaded frame's hash did not match its metadata (SDD §5.11.2).
    ChecksumMismatch,
    // --- safety (403) ---
    /// The target is below the configured minimum altitude (MNT-15).
    LimitAltitude,
    /// The mount is past the configured meridian limit (MNT-16).
    LimitMeridian,
    /// A manual slew would drive an axis past the configured travel from home (M3-T07).
    LimitTravel,
    /// The rig would reach the tripod or the ground (SDD §5.4.3, Layer 2).
    LimitCollision,
    /// A manual slew's dead-man's-switch TTL expired (SDD §5.8.1).
    SlewTtlExpired,
    // --- auth (401) ---
    /// Missing or wrong bearer token (SEC-02).
    Auth,
    // --- resources ---
    /// A frame id arrived twice with different content (SDD §5.11.2).
    FrameIdConflict,
    /// Free space is below the configured critical threshold (REL-12).
    DiskFull,
    /// The addressed resource does not exist.
    NotFound,
    // --- worker supervision (M1-T13; SDD §5.12) ---
    //
    // Added while the PWA was still one screen, because these were about to collapse onto
    // `Internal` — and "the stacking software has a bug" and "the Python worker crashed and is
    // being restarted" send the operator to entirely different places. The supervisor is the
    // only producer; the codes exist so its failures survive the trip through the API envelope.
    /// The job was cancelled — by the operator or by shutdown — before it produced a result.
    Cancelled,
    /// No worker is available to take the job (spawn failed, or restarts are backing off).
    WorkerUnavailable,
    /// The worker died mid-job; the supervisor is handling the restart.
    WorkerCrashed,
    /// The job exceeded `workers.job_timeout_seconds` and the worker was told to stop.
    WorkerTimeout,
    /// An unexpected internal failure.
    Internal,
}

impl ErrorCode {
    /// Every variant, in declaration order — the table-driven tests and the UI's code list
    /// both read from here.
    pub const ALL: &'static [ErrorCode] = &[
        ErrorCode::NotConnected,
        ErrorCode::Unsupported,
        ErrorCode::Busy,
        ErrorCode::Aborted,
        ErrorCode::MountLinkLost,
        ErrorCode::MountTimeout,
        ErrorCode::CameraTimeout,
        ErrorCode::DeviceTimeout,
        ErrorCode::DeviceTransport,
        ErrorCode::DeviceProtocol,
        ErrorCode::NodeUnreachable,
        ErrorCode::DeviceRejected,
        ErrorCode::Validation,
        ErrorCode::CommandStale,
        ErrorCode::ChecksumMismatch,
        ErrorCode::LimitAltitude,
        ErrorCode::LimitMeridian,
        ErrorCode::LimitTravel,
        ErrorCode::LimitCollision,
        ErrorCode::SlewTtlExpired,
        ErrorCode::Auth,
        ErrorCode::FrameIdConflict,
        ErrorCode::DiskFull,
        ErrorCode::NotFound,
        ErrorCode::Cancelled,
        ErrorCode::WorkerUnavailable,
        ErrorCode::WorkerCrashed,
        ErrorCode::WorkerTimeout,
        ErrorCode::Internal,
    ];

    /// The wire spelling, identical to the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotConnected => "NOT_CONNECTED",
            Self::Unsupported => "UNSUPPORTED",
            Self::Busy => "BUSY",
            Self::Aborted => "ABORTED",
            Self::MountLinkLost => "MOUNT_LINK_LOST",
            Self::MountTimeout => "MOUNT_TIMEOUT",
            Self::CameraTimeout => "CAMERA_TIMEOUT",
            Self::DeviceTimeout => "DEVICE_TIMEOUT",
            Self::DeviceTransport => "DEVICE_TRANSPORT",
            Self::DeviceProtocol => "DEVICE_PROTOCOL",
            Self::NodeUnreachable => "NODE_UNREACHABLE",
            Self::DeviceRejected => "DEVICE_REJECTED",
            Self::Validation => "VALIDATION",
            Self::CommandStale => "COMMAND_STALE",
            Self::ChecksumMismatch => "CHECKSUM_MISMATCH",
            Self::LimitAltitude => "LIMIT_ALTITUDE",
            Self::LimitMeridian => "LIMIT_MERIDIAN",
            Self::LimitTravel => "LIMIT_TRAVEL",
            Self::LimitCollision => "LIMIT_COLLISION",
            Self::SlewTtlExpired => "SLEW_TTL_EXPIRED",
            Self::Auth => "AUTH",
            Self::FrameIdConflict => "FRAME_ID_CONFLICT",
            Self::DiskFull => "DISK_FULL",
            Self::NotFound => "NOT_FOUND",
            Self::Cancelled => "CANCELLED",
            Self::WorkerUnavailable => "WORKER_UNAVAILABLE",
            Self::WorkerCrashed => "WORKER_CRASHED",
            Self::WorkerTimeout => "WORKER_TIMEOUT",
            Self::Internal => "INTERNAL",
        }
    }

    /// The HTTP status this code is returned with — the mapping table of SDD §4.2.
    ///
    /// A `u16` rather than a framework status type: `astroctl-core` sits below the API layer
    /// and must not drag axum into every crate in the workspace (ADD §5.6).
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            // NotConnected / Unsupported → 409, plus Busy, which §4.2 leaves unmapped and
            // which is the same class of answer: the device is fine, its state is wrong.
            Self::NotConnected | Self::Unsupported | Self::Busy => 409,
            // Timeout / Transport → 502 "device side". Protocol joins them: an unparseable
            // reply is an upstream-device failure, not a client mistake.
            Self::MountTimeout
            | Self::CameraTimeout
            | Self::DeviceTimeout
            | Self::DeviceTransport
            | Self::DeviceProtocol
            // A dead link is a transport failure that has stopped being intermittent; the same
            // 502 the individual failures behind it were already answering.
            | Self::MountLinkLost
            // The other *node*, not a device — but the same 502: something upstream of this
            // process did not answer, and the client's move is to retry, not to fix its request.
            | Self::NodeUnreachable => 502,
            // Rejected / validation → 422.
            Self::DeviceRejected
            | Self::Validation
            | Self::CommandStale
            | Self::ChecksumMismatch => 422,
            // Safety-limit rejection → 403.
            Self::LimitAltitude
            | Self::LimitMeridian
            | Self::LimitTravel
            | Self::LimitCollision
            | Self::SlewTtlExpired => 403,
            // Auth failure → 401.
            Self::Auth => 401,
            // A frame id re-used with different content is a conflict with stored state.
            Self::FrameIdConflict => 409,
            // SDD §5.11.2: ingest refuses below the critical threshold with 507.
            Self::DiskFull => 507,
            Self::NotFound => 404,
            // Cancellation is the operator's own act arriving back; nothing failed. 409 rather
            // than a 4xx blame code: the request was fine, the state moved under it.
            Self::Cancelled => 409,
            // Same reasoning as Cancelled, one layer down: the goto was valid and an e-stop or
            // a limit took the axes out from under it. 422 was the status this replaces, and it
            // told the operator their request was malformed while their e-stop was working.
            Self::Aborted => 409,
            // Worker failures are upstream-of-the-API failures, like the device 502s: the
            // caller's request was valid and the thing behind the API could not serve it.
            Self::WorkerUnavailable | Self::WorkerCrashed | Self::WorkerTimeout => 502,
            Self::Internal => 500,
        }
    }

    /// Whether a client may sensibly retry the same request unchanged.
    ///
    /// SDD §4.2 states this only for the 502 device-side pair; the rest follow from it —
    /// nothing that blames the request or the operator's configuration gets retried.
    #[must_use]
    pub const fn retryable(self) -> bool {
        match self {
            Self::MountTimeout
            | Self::CameraTimeout
            | Self::DeviceTimeout
            | Self::DeviceTransport
            // A cable that came out goes back in, and the operator's own retry is the reconnect.
            // Retryable also keeps the PWA from offering "give up" as the only affordance next to
            // an alert whose whole purpose is to send someone outside to fix it.
            | Self::MountLinkLost
            // A tunnel that is down comes back — that is what REL-10 is about — and the
            // transfer agent's whole design assumes a retryable "the other node is not
            // answering" (§5.10.1 keeps such a frame queued rather than failing it).
            | Self::NodeUnreachable => true,
            // The supervisor restarts a crashed or unavailable worker on its own (§5.12.3), so
            // the same request later is expected to succeed. A worker *timeout* is not retried:
            // the same job hitting the same ceiling repeats deterministically, like Protocol.
            Self::WorkerUnavailable | Self::WorkerCrashed => true,
            // Aborted joins these: a motion stopped by a safety action must not be re-issued
            // automatically, or the client drives the mount back into whatever stopped it.
            Self::Cancelled | Self::WorkerTimeout | Self::Aborted => false,
            // A protocol error repeats deterministically until the driver or firmware
            // changes; retrying it just burns the operator's link budget.
            Self::DeviceProtocol
            | Self::NotConnected
            | Self::Unsupported
            | Self::Busy
            | Self::DeviceRejected
            | Self::Validation
            | Self::CommandStale
            | Self::ChecksumMismatch
            | Self::LimitAltitude
            | Self::LimitMeridian
            | Self::LimitTravel
            | Self::LimitCollision
            | Self::SlewTtlExpired
            | Self::Auth
            | Self::FrameIdConflict
            | Self::DiskFull
            | Self::NotFound
            | Self::Internal => false,
        }
    }

    /// Maps a driver failure onto a wire code (SDD §4.2).
    ///
    /// `kind` exists because §4.2's own example is `MOUNT_TIMEOUT` while [`DeviceError`] is
    /// device-agnostic: the operator needs to be told *what* stopped answering, and the
    /// enum alone cannot say.
    #[must_use]
    pub const fn from_device_error(kind: DeviceKind, err: &DeviceError) -> Self {
        match err {
            DeviceError::NotConnected => Self::NotConnected,
            DeviceError::Timeout(_) => match kind {
                DeviceKind::Mount => Self::MountTimeout,
                DeviceKind::Camera => Self::CameraTimeout,
                DeviceKind::GuideCamera => Self::DeviceTimeout,
            },
            DeviceError::Protocol(_) => Self::DeviceProtocol,
            DeviceError::Rejected(_) => Self::DeviceRejected,
            DeviceError::Transport(_) => Self::DeviceTransport,
            DeviceError::Unsupported => Self::Unsupported,
            DeviceError::LimitViolation { limit, .. } => limit.code(),
            DeviceError::Busy(_) => Self::Busy,
            DeviceError::Aborted(_) => Self::Aborted,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------------------
// ApiError — the envelope
// ---------------------------------------------------------------------------------------

/// The JSON envelope every non-2xx API response carries (SDD §4.2).
///
/// ```json
/// { "v": 1, "code": "MOUNT_TIMEOUT", "message": "mount did not respond within 2.0s",
///   "detail": {"axis": "ra", "command": "j"}, "retryable": true }
/// ```
///
/// `detail` is always present (`null` when there is nothing to say) so the shape of the
/// envelope does not vary with the failure.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// Schema version (SDD §2: every externally visible JSON schema carries one).
    pub v: u16,
    /// The machine-readable code the UI switches on.
    pub code: ErrorCode,
    /// Operator-facing message; safe to display verbatim.
    pub message: String,
    /// Structured context for the UI and the logs, or `null`.
    pub detail: Option<serde_json::Value>,
    /// Whether an unchanged retry may succeed.
    pub retryable: bool,
}

impl ApiError {
    /// Current envelope schema version.
    pub const SCHEMA_VERSION: u16 = 1;

    /// Builds an envelope, taking `retryable` from the code's default.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            v: Self::SCHEMA_VERSION,
            code,
            message: message.into(),
            detail: None,
            retryable: code.retryable(),
        }
    }

    /// Attaches structured context, e.g. `{"axis": "ra", "command": "j"}`.
    #[must_use]
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Overrides the code's default retryability.
    ///
    /// For the rare case where the handler knows better than the code does — a timeout on a
    /// non-idempotent operation, say, where a blind retry could double-execute.
    #[must_use]
    pub const fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// The HTTP status this envelope must be sent with (SDD §4.2).
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        self.code.http_status()
    }

    /// Builds an envelope from a driver failure, naming the device that failed.
    #[must_use]
    pub fn from_device_error(kind: DeviceKind, err: &DeviceError) -> Self {
        Self::new(ErrorCode::from_device_error(kind, err), err.to_string())
    }
}

impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        Self::new(ErrorCode::Validation, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DeviceError, SDD §4.2 verbatim ----------------------------------------------

    #[test]
    fn device_error_messages_are_the_spec_strings() {
        let cases: Vec<(DeviceError, &str)> = vec![
            (DeviceError::NotConnected, "device not connected"),
            (
                DeviceError::Timeout(Duration::from_millis(500)),
                "timeout after 500ms",
            ),
            (
                DeviceError::Protocol("unexpected reply '!3'".to_owned()),
                "protocol error: unexpected reply '!3'",
            ),
            (
                DeviceError::Rejected("axis busy".to_owned()),
                "device rejected command: axis busy",
            ),
            (
                DeviceError::Transport("/dev/ttyUSB0: no such device".to_owned()),
                "transport error: /dev/ttyUSB0: no such device",
            ),
            (DeviceError::Unsupported, "unsupported by this device"),
            (
                DeviceError::Busy("slew in progress"),
                "busy: slew in progress",
            ),
            (
                DeviceError::Aborted("goto aborted by an emergency stop".to_owned()),
                "aborted: goto aborted by an emergency stop",
            ),
            (
                DeviceError::LimitViolation {
                    limit: Limit::Altitude,
                    detail: "target altitude -12.0° is below the configured 15.0°".to_owned(),
                },
                "altitude limit: target altitude -12.0° is below the configured 15.0°",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    // --- ErrorCode → HTTP, the SDD §4.2 table ----------------------------------------

    #[test]
    fn error_code_http_mapping_matches_sdd_4_2() {
        // Every row here is a literal claim from SDD §4.2 or the section it names.
        let table: &[(ErrorCode, u16, bool)] = &[
            // "NotConnected/Unsupported → 409"
            (ErrorCode::NotConnected, 409, false),
            (ErrorCode::Unsupported, 409, false),
            // "Timeout/Transport → 502 (device side, retryable: true)"
            (ErrorCode::MountTimeout, 502, true),
            (ErrorCode::CameraTimeout, 502, true),
            (ErrorCode::DeviceTimeout, 502, true),
            (ErrorCode::DeviceTransport, 502, true),
            // "Rejected/validation → 422"
            (ErrorCode::DeviceRejected, 422, false),
            (ErrorCode::Validation, 422, false),
            // "safety-limit rejection → 403 with code: LIMIT_ALTITUDE"
            (ErrorCode::LimitAltitude, 403, false),
            // "auth failure → 401"
            (ErrorCode::Auth, 401, false),
            // §5.8.1 / M1-T10: stale motion command → 422 COMMAND_STALE
            (ErrorCode::CommandStale, 422, false),
            // §5.11.2: checksum mismatch → 422, disk critical → 507
            (ErrorCode::ChecksumMismatch, 422, false),
            (ErrorCode::DiskFull, 507, false),
            // M1-T03: a goto stopped by an e-stop is 409, not the 422 it used to be. The row is
            // here rather than only in `every_code_has_a_plausible_status` because the *status*
            // is the whole point of the code existing — 422 was the defect.
            (ErrorCode::Aborted, 409, false),
            // M1-T05: the meridian limit's row, so both safety codes are pinned rather than
            // only the one §4.2 spells out.
            (ErrorCode::LimitMeridian, 403, false),
            (ErrorCode::SlewTtlExpired, 403, false),
        ];
        for &(code, status, retryable) in table {
            assert_eq!(code.http_status(), status, "status for {code}");
            assert_eq!(code.retryable(), retryable, "retryable for {code}");
        }
    }

    /// M1-T05. The safety wrapper's refusal must not arrive as a 422 blaming the request: the
    /// request was well formed, and the answer the operator needs is *which limit*.
    #[test]
    fn a_limit_violation_reaches_the_wire_as_its_own_403() {
        for (limit, expected) in [
            (Limit::Altitude, ErrorCode::LimitAltitude),
            (Limit::Meridian, ErrorCode::LimitMeridian),
        ] {
            let error = DeviceError::LimitViolation {
                limit,
                detail: "…".to_owned(),
            };
            let code = ErrorCode::from_device_error(DeviceKind::Mount, &error);
            assert_eq!(code, expected);
            assert_eq!(code.http_status(), 403, "for {limit}");
            assert!(
                !code.retryable(),
                "re-issuing the same motion drives the mount back into the limit"
            );
            // The envelope carries the detail, not a generic string: "below 15°" is the whole
            // actionable content.
            let envelope = ApiError::from_device_error(DeviceKind::Mount, &error);
            assert!(envelope.message.contains('…'), "{}", envelope.message);
        }
    }

    #[test]
    fn every_code_has_a_plausible_status() {
        for &code in ErrorCode::ALL {
            let status = code.http_status();
            assert!(
                (400..=599).contains(&status),
                "{code} mapped to {status}, which is not an error status"
            );
            // Only device-side failures are retryable, and they are all 502.
            if code.retryable() {
                assert_eq!(status, 502, "{code} is retryable but not a 502");
            }
        }
    }

    #[test]
    fn all_lists_every_variant_exactly_once() {
        let mut seen = ErrorCode::ALL.to_vec();
        let before = seen.len();
        seen.sort_unstable_by_key(|c| c.as_str());
        seen.dedup();
        assert_eq!(seen.len(), before, "ErrorCode::ALL contains a duplicate");
        // Adding a variant without adding it to ALL leaves this count stale on purpose:
        // the number is the review checkpoint for a frozen contract. 20 → 24 was the worker
        // vocabulary (SDD §4.2 change note, 2026-07-30) — approved before M1-T04 builds the
        // PWA switch, i.e. while extending was still cheap. 24 → 25 is `ABORTED` (M1-T03,
        // SDD §4.2 change note): a goto stopped by an e-stop was answering 422 DEVICE_REJECTED,
        // which blames the operator's request for their own safety action working. 25 → 26 is
        // `NODE_UNREACHABLE`, which §4.2 has specified since change note 1.9.0 with no producer
        // — the `/stack/*` proxy was answering `DEVICE_TRANSPORT`, the exact conflation that row
        // was written to prevent. Found by M1-T14 (the stack panel has to tell "the stacking
        // server is down" from "a cable came out" while a capture is running). 26 → 27 is
        // `MOUNT_LINK_LOST` (M1-T17, SDD §4.2 change note 1.29.0): REL-02's watchdog had no code
        // to fire, and `mount.serial.heartbeat_misses` had been shipped and range-validated since
        // M0 while being read by nothing — a validated key that does nothing tells the operator a
        // protection exists. Unlike the four above it, no route returns this one; it is
        // alert-only, like `DISK_FULL`'s warning twin, and the row is here so `alert.code` can
        // draw from the same closed set §4.2 says it draws from. 27 → 28 is `LIMIT_TRAVEL`
        // (M3-T07, SDD §4.2): manual slew had no bound on how far it could wind an axis from the
        // home pose, and an operator reached 215.6° holding a D-pad. It is a third *limit* rather
        // than a reuse of the two existing ones because the remedy is different — not "point
        // higher" and not "flip or park" but "drive back toward home" — and a refusal that
        // borrowed one of their codes would send the operator to the wrong action.
        //
        // 28 → 29 is `LIMIT_COLLISION` (SDD §5.4.3, Layer 2), and a fourth limit for the same
        // reason there was a third: the remedy is different again, and it is the only one of the
        // four that is not about where the telescope is pointing. "The metal is about to touch the
        // tripod" sends the operator to the rig with their hands, not to the screen — and unlike
        // the other three it cannot be recovered from by driving the other way.
        assert_eq!(before, 29);
    }

    #[test]
    fn wire_spelling_matches_as_str_for_every_code() {
        for &code in ErrorCode::ALL {
            let json = serde_json::to_value(code).expect("serializes");
            assert_eq!(
                json,
                serde_json::Value::String(code.as_str().to_owned()),
                "serde and as_str disagree for {code:?}"
            );
            let back: ErrorCode = serde_json::from_value(json).expect("deserializes");
            assert_eq!(back, code);
            assert_eq!(code.to_string(), code.as_str());
        }
    }

    #[test]
    fn unknown_codes_are_refused() {
        // Closed enum: a code the UI invents does not silently become a valid envelope.
        assert!(serde_json::from_str::<ErrorCode>("\"TELEPORT_FAILED\"").is_err());
    }

    // --- DeviceError → ErrorCode ------------------------------------------------------

    #[test]
    fn device_errors_map_to_codes_by_device_kind() {
        use DeviceKind::{Camera, GuideCamera, Mount};
        let cases: Vec<(DeviceKind, DeviceError, ErrorCode)> = vec![
            (Mount, DeviceError::NotConnected, ErrorCode::NotConnected),
            (
                Mount,
                DeviceError::Timeout(Duration::from_millis(500)),
                ErrorCode::MountTimeout,
            ),
            (
                Camera,
                DeviceError::Timeout(Duration::from_secs(30)),
                ErrorCode::CameraTimeout,
            ),
            (
                GuideCamera,
                DeviceError::Timeout(Duration::from_secs(1)),
                ErrorCode::DeviceTimeout,
            ),
            (
                Mount,
                DeviceError::Protocol("bad frame".to_owned()),
                ErrorCode::DeviceProtocol,
            ),
            (
                Mount,
                DeviceError::Rejected("!1".to_owned()),
                ErrorCode::DeviceRejected,
            ),
            (
                Camera,
                DeviceError::Transport("usb gone".to_owned()),
                ErrorCode::DeviceTransport,
            ),
            (Camera, DeviceError::Unsupported, ErrorCode::Unsupported),
            (
                Mount,
                DeviceError::Busy("slew in progress"),
                ErrorCode::Busy,
            ),
        ];
        for (kind, err, expected) in cases {
            assert_eq!(ErrorCode::from_device_error(kind, &err), expected);
        }
    }

    // --- ApiError ---------------------------------------------------------------------

    #[test]
    fn envelope_matches_the_sdd_4_2_example() {
        let doc = serde_json::json!({
            "v": 1,
            "code": "MOUNT_TIMEOUT",
            "message": "mount did not respond within 2.0s",
            "detail": {"axis": "ra", "command": "j"},
            "retryable": true
        });
        let parsed: ApiError = serde_json::from_value(doc.clone()).expect("deserializes");
        assert_eq!(parsed.code, ErrorCode::MountTimeout);
        assert!(parsed.retryable);
        assert_eq!(parsed.http_status(), 502);

        let built = ApiError::new(ErrorCode::MountTimeout, "mount did not respond within 2.0s")
            .with_detail(serde_json::json!({"axis": "ra", "command": "j"}));
        assert_eq!(built, parsed);
        assert_eq!(serde_json::to_value(&built).expect("serializes"), doc);
    }

    #[test]
    fn envelope_round_trips_with_null_detail() {
        let err = ApiError::new(ErrorCode::Auth, "missing bearer token");
        let json = serde_json::to_value(&err).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "v": 1, "code": "AUTH", "message": "missing bearer token",
                "detail": null, "retryable": false
            })
        );
        assert_eq!(
            serde_json::from_value::<ApiError>(json).expect("deserializes"),
            err
        );
        assert_eq!(err.http_status(), 401);
    }

    #[test]
    fn envelope_from_device_error_carries_the_driver_message() {
        let err = ApiError::from_device_error(
            DeviceKind::Mount,
            &DeviceError::Timeout(Duration::from_millis(500)),
        );
        assert_eq!(err.code, ErrorCode::MountTimeout);
        assert_eq!(err.message, "timeout after 500ms");
        assert!(err.retryable);
        assert_eq!(err.http_status(), 502);
    }

    #[test]
    fn envelope_from_core_error_is_validation() {
        let core = CoreError::invalid_coordinate("declination", 91.0, "within [-90, +90] degrees");
        assert_eq!(
            core.to_string(),
            "invalid declination: 91 is not within [-90, +90] degrees"
        );
        let api = ApiError::from(core);
        assert_eq!(api.code, ErrorCode::Validation);
        assert_eq!(api.http_status(), 422);
        assert!(!api.retryable);
    }

    #[test]
    fn retryable_can_be_overridden_for_non_idempotent_operations() {
        let err =
            ApiError::new(ErrorCode::MountTimeout, "goto may have started").with_retryable(false);
        assert!(!err.retryable);
        assert!(err.code.retryable(), "the code's own default is unchanged");
    }
}
