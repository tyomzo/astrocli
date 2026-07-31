//! `SyntaCodec` — the Sky-Watcher wire protocol, and nothing else (SDD §5.2.2, M3-T01).
//!
//! Pure functions over bytes and strings. No port, no runtime, no clock, no `unsafe`. That is
//! what lets T-COD-1 walk the entire 24-bit value domain and every three-nibble status word in a
//! unit test instead of sampling them, and it is why this module has no dependency beyond
//! `astroctl-core`'s [`Axis`](astroctl_core::types::Axis) and `thiserror`.
//!
//! # The frame
//!
//! ```text
//! request   :  <opcode> <axis> <payload>  \r
//! success   =  <payload>                  \r
//! refusal   !  <digit>                    \r
//! ```
//!
//! Opcode case is the safety boundary: lower case inquires, upper case moves things
//! (SDD §5.2.2). [`Frame::is_action`] and [`Command::is_action`] expose it so M3-T02 can enforce
//! on the raw stream what the spike harnesses enforced on themselves.
//!
//! # Four things this module refuses to make easy
//!
//! **A payload of unknown width.** `:g` answers two characters, `:f` three, a register six, an
//! action none. There is no `decode_payload`; each command decodes its own reply, so a decoder
//! pointed at the wrong reply fails loudly rather than returning a plausible number.
//!
//! **A motion mode built from an integer.** The mode digit's speed bit inverts its meaning
//! depending on the motion kind, so arithmetic gets one of the four values wrong, and the cost
//! of getting it wrong is the verified 16× high-speed ratio rather than a direction. See
//! [`MotionMode`].
//!
//! **An absolute goto target.** The protocol has no such opcode. `H` is a relative increment
//! whose sign lives in `G`, and [`Move`] holds both halves so they cannot disagree.
//!
//! **A goto started without checking what was programmed.** The mount answers a request for a
//! nonexistent axis with plausible data, so nothing but a readback can catch a mis-addressed or
//! mis-encoded write. [`command::StartMotion::goto`] takes a [`GotoVerified`], and
//! [`GotoProgram::verify`] is its only source.
//!
//! # Provenance
//!
//! `testdata/synta_vectors.txt` carries every frame this module claims to produce or parse,
//! labelled `verified` (seen on the operator's own HEQ5) or `derived` (read from the vendor
//! protocol or `indi-eqmod`, or reconstructed from a hardware *decode* rather than a captured
//! byte string). M3-T05 upgrades the second kind. The distinction is data because a comment
//! saying "probably right" cannot be counted.

pub mod command;
pub mod error;
pub mod frame;
pub mod goto;
pub mod hex;
pub mod motion;
pub mod status;
pub mod value;

pub use command::{
    Command, GetAxisStatus, GetBreakPoint, GetCountsPerRevolution, GetFirmwareVersion,
    GetGotoTarget, GetHighSpeedRatio, GetPosition, GetStepPeriod, GetTimerFrequency, Initialise,
    InstantStop, SetBreakPointIncrement, SetGotoIncrement, SetGuideRate, SetMotionMode,
    SetStepPeriod, StartMotion, StopMotion,
};
pub use error::{EncodeError, MountError, ProtocolError};
pub use frame::{axis_digit, axis_from_digit, decode_reply, Frame, Reply, MAX_FRAME_LEN};
pub use goto::{
    GotoObservation, GotoProgram, GotoVerified, ReadbackMismatch, ReadbackVerified, Register,
};
pub use hex::{
    decode_u24, decode_u24_plain, decode_u8_plain, encode_u24, FirmwareVersion, MountModel, U24,
    U24_CHARS, U24_MAX,
};
pub use motion::{MotionDirection, MotionKind, MotionMode, SpeedClass};
pub use status::{AxisStatus, STATUS_CHARS};
pub use value::{
    Counts, CountsPerRev, GuideRate, HighSpeedRatio, Move, StepPeriod, TimerFrequency,
};
