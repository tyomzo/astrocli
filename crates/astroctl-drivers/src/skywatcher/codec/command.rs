//! One type per command, each knowing its own frame and its own reply.
//!
//! # The shape M3-T02 consumes
//!
//! Every command implements [`Command`], which pairs an encoder with a decoder and an
//! associated response type. That lets the serial task have exactly one exchange method:
//!
//! ```ignore
//! async fn send<C: Command>(&self, cmd: C) -> Result<C::Response, DeviceError>;
//! ```
//!
//! and get `Counts` back from a `GetPosition`, `AxisStatus` from a `GetAxisStatus`, and `()`
//! from a `StartMotion`, with no downcasting and no place for a caller to point the wrong
//! decoder at a reply. The width non-uniformity — six characters for a register, three for
//! status, two for the high-speed ratio, none for an action — lives inside the individual
//! [`Command::decode`] implementations, which is why there is no general "decode a payload"
//! function anywhere in this crate.
//!
//! [`Command::OPCODE`] is exposed for M3-T02's write gate: SDD §5.2.2 makes opcode case the
//! safety boundary, and the spike harnesses refused to put an uppercase byte on the wire unless
//! the experiment allow-listed it. A serial task can apply the same rule to typed commands.
//!
//! # What is deliberately absent
//!
//! *No absolute-target command.* The protocol has none: `H` is a **relative increment** and the
//! `S` opcode SDD §5.2.2 used to list does not exist. That correction came out of
//! `spikes/skywatcher-heq5/ENCODINGS.md`, and the absence is now structural — there is no type
//! here that takes an absolute target, so a driver cannot express one by accident. Absolute
//! targeting is arithmetic M3-T03 does, ending in a [`Move`].
//!
//! *No free-form inquiry.* The mount answers seven lowercase opcodes this design does not use
//! (`c d r s`, plus `h i m`, which it now does). They are not reachable from here, because an
//! escape hatch that takes an opcode character would also take an axis character, and the mount
//! does not validate axis characters — `:j9` answers `=000080` for an axis that does not exist.
//! Their captured values live in `testdata/synta_vectors.txt` as decode-only evidence instead.
//!
//! *No `E`, `O`, `U`.* They exist in the vendor protocol and this design does not use them
//! (SDD §5.2.2). `E` in particular writes the axis position counter, which is a way to lose the
//! mount's idea of where it is; if M3-T05 wants it for a wraparound experiment, it should arrive
//! with a reason.

use astroctl_core::types::Axis;

use super::error::ProtocolError;
use super::frame::Frame;
use super::hex::{decode_u24, decode_u24_plain, decode_u8_plain, encode_u24, FirmwareVersion};
use super::motion::MotionMode;
use super::status::AxisStatus;
use super::value::{
    Counts, CountsPerRev, GuideRate, HighSpeedRatio, Move, StepPeriod, TimerFrequency,
};

/// A Synta command: the bytes it sends and the reply it understands.
pub trait Command {
    /// What a successful reply decodes to. `()` for the commands the mount acknowledges with a
    /// bare `=`, which is every action opcode.
    type Response;

    /// The opcode byte. Lower case inquires, upper case acts (SDD §5.2.2).
    const OPCODE: u8;

    /// The frame to write.
    fn encode(&self) -> Frame;

    /// Interpret the payload of a successful reply — the bytes between `=` and `\r`.
    ///
    /// Takes `&self` because two commands need their own request to interpret the answer:
    /// nothing does today, and the goto readbacks nearly did. The cost is nil and it keeps the
    /// trait usable by a command that later does.
    ///
    /// # Errors
    /// [`ProtocolError`] if the payload is the wrong width or not the expected shape.
    fn decode(&self, payload: &str) -> Result<Self::Response, ProtocolError>;

    /// Whether this command can move the mount.
    #[must_use]
    fn is_action() -> bool
    where
        Self: Sized,
    {
        Self::OPCODE.is_ascii_uppercase()
    }
}

/// The reply every action opcode gives: `=` and nothing else.
///
/// Measured — `:F1` and `:F2` both answered a bare `=` (`spikes/skywatcher-heq5/FINDINGS.md`,
/// E1) — and checked rather than ignored, because an action that answered with a payload would
/// mean this is not the command we think it is.
fn expect_ack(payload: &str, opcode: char) -> Result<(), ProtocolError> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(ProtocolError::PayloadWidth {
            payload: payload.to_owned(),
            actual: payload.chars().count(),
            expected: 0,
            opcode,
        })
    }
}

/// Declare a command with no payload and a typed reply.
macro_rules! inquiry {
    (
        $(#[$meta:meta])*
        $name:ident, $opcode:literal, $response:ty, |$payload:ident| $decode:expr
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(
            /// The axis to ask.
            pub Axis,
        );

        impl Command for $name {
            type Response = $response;
            const OPCODE: u8 = $opcode;

            fn encode(&self) -> Frame {
                Frame::build(Self::OPCODE, self.0, b"")
            }

            fn decode(&self, $payload: &str) -> Result<Self::Response, ProtocolError> {
                $decode
            }
        }
    };
}

/// Declare an action command that carries no payload and is acknowledged with a bare `=`.
macro_rules! bare_action {
    ($(#[$meta:meta])* $name:ident, $opcode:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(
            /// The axis to act on.
            pub Axis,
        );

        impl Command for $name {
            type Response = ();
            const OPCODE: u8 = $opcode;

            fn encode(&self) -> Frame {
                Frame::build(Self::OPCODE, self.0, b"")
            }

            fn decode(&self, payload: &str) -> Result<(), ProtocolError> {
                expect_ack(payload, char::from(Self::OPCODE))
            }
        }
    };
}

// -----------------------------------------------------------------------------------------
// Inquiries — the handshake and the poll
// -----------------------------------------------------------------------------------------

inquiry! {
    /// `e` — motor-controller firmware version and mount model.
    ///
    /// The one six-character reply that is **not** byte-swapped; see [`FirmwareVersion`].
    GetFirmwareVersion, b'e', FirmwareVersion,
    |payload| decode_u24_plain(payload, 'e').map(FirmwareVersion::from_raw)
}

inquiry! {
    /// `a` — counts per revolution. Read at handshake and never assumed.
    GetCountsPerRevolution, b'a', CountsPerRev,
    |payload| decode_u24(payload, 'a').map(CountsPerRev)
}

inquiry! {
    /// `b` — timer interrupt frequency in hertz. Read at handshake; the design's documented
    /// value for it was wrong by 7.1× and only the handshake saved it.
    GetTimerFrequency, b'b', TimerFrequency,
    |payload| decode_u24(payload, 'b').map(TimerFrequency)
}

inquiry! {
    /// `j` — the axis position counter. The 1 Hz poll, and the goto monitor at 2 Hz.
    GetPosition, b'j', Counts,
    |payload| decode_u24(payload, 'j').map(Counts)
}

inquiry! {
    /// `f` — axis status. Three characters, not six; see [`AxisStatus`].
    GetAxisStatus, b'f', AxisStatus,
    |payload| AxisStatus::decode(payload)
}

inquiry! {
    /// `g` — the ratio between the low and high speed classes. **Two** characters, not six.
    GetHighSpeedRatio, b'g', HighSpeedRatio,
    |payload| decode_u8_plain(payload, 'g').map(HighSpeedRatio)
}

// -----------------------------------------------------------------------------------------
// Inquiries — the three readback registers
// -----------------------------------------------------------------------------------------
//
// Undocumented in the vendor protocol and identified experimentally: writing `:H`=12,345 and
// `:M`=6,172 at a known position moved `:h` and `:m` by exactly those amounts, and `:i` returned
// the step period `:I` had just written (`spikes/skywatcher-heq5/FINDINGS.md`, E14). All three
// matches were exact. SDD §5.2.2 promotes reading them to *mandatory* before `J`.
//
// Note what they return: `:h` and `:m` are **absolute**, i.e. the position at the time of the
// write plus the increment. The writes are relative and the readbacks are not.

inquiry! {
    /// `h` — the absolute goto target `H` programmed. **Readback of [`SetGotoIncrement`].**
    GetGotoTarget, b'h', Counts,
    |payload| decode_u24(payload, 'h').map(Counts)
}

inquiry! {
    /// `m` — the absolute break point `M` programmed. **Readback of
    /// [`SetBreakPointIncrement`].**
    GetBreakPoint, b'm', Counts,
    |payload| decode_u24(payload, 'm').map(Counts)
}

inquiry! {
    /// `i` — the step period `I` programmed. **Readback of [`SetStepPeriod`].**
    GetStepPeriod, b'i', StepPeriod,
    |payload| decode_u24(payload, 'i').map(StepPeriod)
}

// -----------------------------------------------------------------------------------------
// Actions
// -----------------------------------------------------------------------------------------

bare_action! {
    /// `F` — initialise the axis.
    ///
    /// Measured to set the initialised bit and move nothing: both axes went `100` → `101` with
    /// zero counter movement (E1). Uppercase, so it is an action by the case convention even
    /// though it is the least eventful one.
    Initialise, b'F'
}

bare_action! {
    /// `K` — stop, ramped. The ordinary stop for a slew or for tracking.
    StopMotion, b'K'
}

bare_action! {
    /// `L` — stop, instant. **Emergency stop only** (SDD §5.2.4).
    ///
    /// Measured indistinguishable from `K` at low speed — 85 counts of overshoot against 84,
    /// which at 5,350 counts/s is one serial round trip, i.e. the overshoot is command latency
    /// rather than deceleration. The advantage is real only where momentum is, at high slew
    /// rates.
    InstantStop, b'L'
}

/// `G` — set the motion mode. **The highest-risk encoding in the protocol**; see
/// [`MotionMode`](super::motion::MotionMode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SetMotionMode {
    /// The axis.
    pub axis: Axis,
    /// Kind, speed class and direction.
    pub mode: MotionMode,
}

impl Command for SetMotionMode {
    type Response = ();
    const OPCODE: u8 = b'G';

    fn encode(&self) -> Frame {
        Frame::build(Self::OPCODE, self.axis, &self.mode.wire())
    }

    fn decode(&self, payload: &str) -> Result<(), ProtocolError> {
        expect_ack(payload, 'G')
    }
}

/// `I` — set the step period.
///
/// Governs slew and tracking rate. **Not goto speed**, which it was measured not to affect at
/// all; it is still part of the goto sequence and still worth verifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SetStepPeriod {
    /// The axis.
    pub axis: Axis,
    /// Timer ticks between steps.
    pub period: StepPeriod,
}

impl Command for SetStepPeriod {
    type Response = ();
    const OPCODE: u8 = b'I';

    fn encode(&self) -> Frame {
        Frame::build(Self::OPCODE, self.axis, &encode_u24(self.period.0))
    }

    fn decode(&self, payload: &str) -> Result<(), ProtocolError> {
        expect_ack(payload, 'I')
    }
}

/// `H` — set the goto target **increment**, relative to wherever the axis is now.
///
/// Carries a [`Move`] rather than a magnitude, even though only the magnitude reaches the wire.
/// The direction half goes to `G`, and holding both in one value is what makes it impossible to
/// send an increment of 1,000 with a mode that says backward. It is also what lets this type
/// state its own readback expectation without being told which way it was going — see
/// [`super::goto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SetGotoIncrement {
    /// The axis.
    pub axis: Axis,
    /// How far and which way.
    pub target: Move,
}

impl Command for SetGotoIncrement {
    type Response = ();
    const OPCODE: u8 = b'H';

    fn encode(&self) -> Frame {
        Frame::build(
            Self::OPCODE,
            self.axis,
            &encode_u24(self.target.magnitude()),
        )
    }

    fn decode(&self, payload: &str) -> Result<(), ProtocolError> {
        expect_ack(payload, 'H')
    }
}

/// `M` — set the break-point increment: where deceleration begins, measured from the start of
/// the move.
///
/// Missing entirely from SDD §5.2.2 until `spikes/skywatcher-heq5/ENCODINGS.md` found it, and
/// part of every goto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SetBreakPointIncrement {
    /// The axis.
    pub axis: Axis,
    /// How far into the move deceleration starts, and which way the move goes.
    pub brake: Move,
}

impl Command for SetBreakPointIncrement {
    type Response = ();
    const OPCODE: u8 = b'M';

    fn encode(&self) -> Frame {
        Frame::build(Self::OPCODE, self.axis, &encode_u24(self.brake.magnitude()))
    }

    fn decode(&self, payload: &str) -> Result<(), ProtocolError> {
        expect_ack(payload, 'M')
    }
}

/// `P` — set the ST4 autoguide rate. One digit; semantics unverified, range enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SetGuideRate {
    /// The axis.
    pub axis: Axis,
    /// Rate level.
    pub rate: GuideRate,
}

impl Command for SetGuideRate {
    type Response = ();
    const OPCODE: u8 = b'P';

    fn encode(&self) -> Frame {
        Frame::build(Self::OPCODE, self.axis, &[self.rate.digit()])
    }

    fn decode(&self, payload: &str) -> Result<(), ProtocolError> {
        expect_ack(payload, 'P')
    }
}

/// `J` — start the motion the preceding writes programmed.
///
/// **Constructing one is where the readback obligation is enforced.** A bounded motion must go
/// through [`Self::goto`], which takes a proof that the three registers were read back and
/// matched; the proof has no other constructor. An unbounded motion — tracking, or a manual
/// slew under its TTL — goes through [`Self::unbounded`], which needs no proof because there is
/// no target register to have got wrong. The two constructors are how a reader tells, at the
/// call site, which kind of motion is about to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StartMotion {
    axis: Axis,
}

impl StartMotion {
    /// Start an unbounded motion: tracking, or a manual slew.
    ///
    /// `G` and `I` are the only registers such a motion depends on, and neither has a target to
    /// overshoot — what bounds the motion is `K`, `L`, or the slew TTL (SDD §5.8.1). Reading
    /// them back would be cheap insurance against nothing.
    #[must_use]
    pub const fn unbounded(axis: Axis) -> Self {
        Self { axis }
    }

    /// Start a verified goto. See [`super::goto::GotoVerified`].
    #[must_use]
    pub const fn goto(proof: &super::goto::GotoVerified) -> Self {
        Self { axis: proof.axis() }
    }

    /// The axis this will start.
    #[must_use]
    pub const fn axis(self) -> Axis {
        self.axis
    }
}

impl Command for StartMotion {
    type Response = ();
    const OPCODE: u8 = b'J';

    fn encode(&self) -> Frame {
        Frame::build(Self::OPCODE, self.axis, b"")
    }

    fn decode(&self, payload: &str) -> Result<(), ProtocolError> {
        expect_ack(payload, 'J')
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::motion::{MotionDirection, MotionKind, SpeedClass};

    /// Encode a command and render its frame without the terminator, the way the vector file
    /// and a log both show it.
    fn wire<C: Command>(cmd: &C) -> String {
        cmd.encode().to_string()
    }

    #[test]
    fn the_handshake_inquiries_encode_and_decode_the_operators_captures() {
        assert_eq!(wire(&GetFirmwareVersion(Axis::Ra)), ":e1");
        assert_eq!(wire(&GetCountsPerRevolution(Axis::Ra)), ":a1");
        assert_eq!(wire(&GetTimerFrequency(Axis::Dec)), ":b2");
        assert_eq!(wire(&GetPosition(Axis::Dec)), ":j2");
        assert_eq!(wire(&GetAxisStatus(Axis::Ra)), ":f1");
        assert_eq!(wire(&GetHighSpeedRatio(Axis::Ra)), ":g1");

        assert_eq!(
            GetCountsPerRevolution(Axis::Ra).decode("00B289"),
            Ok(CountsPerRev(
                super::super::hex::U24::new(9_024_000).expect("fits")
            ))
        );
        assert_eq!(GetPosition(Axis::Ra).decode("000080"), Ok(Counts::HOME));
        assert_eq!(
            GetHighSpeedRatio(Axis::Ra).decode("10"),
            Ok(HighSpeedRatio(16))
        );
        assert_eq!(
            GetFirmwareVersion(Axis::Ra)
                .decode("020401")
                .map(|v| v.model()),
            Ok(super::super::hex::MountModel::from_code(1))
        );
    }

    #[test]
    fn each_reply_width_belongs_to_its_own_command() {
        // The failure this prevents: one decoder that assumes six characters. Point each
        // command at another's reply and every one of them refuses.
        assert!(
            GetPosition(Axis::Ra).decode("10").is_err(),
            "u24 decoder must reject 2 chars"
        );
        assert!(
            GetPosition(Axis::Ra).decode("100").is_err(),
            "u24 decoder must reject 3 chars"
        );
        assert!(GetHighSpeedRatio(Axis::Ra).decode("000080").is_err());
        assert!(GetAxisStatus(Axis::Ra).decode("000080").is_err());
        assert!(GetAxisStatus(Axis::Ra).decode("10").is_err());
    }

    #[test]
    fn an_action_is_acknowledged_with_nothing_and_a_payload_is_a_fault() {
        assert_eq!(Initialise(Axis::Ra).decode(""), Ok(()));
        assert_eq!(StartMotion::unbounded(Axis::Ra).decode(""), Ok(()));
        assert!(matches!(
            Initialise(Axis::Ra).decode("000080"),
            Err(ProtocolError::PayloadWidth {
                expected: 0,
                actual: 6,
                opcode: 'F',
                ..
            })
        ));
    }

    #[test]
    fn opcode_case_separates_inquiry_from_action_across_the_whole_table() {
        let inquiries: [u8; 9] = [
            GetFirmwareVersion::OPCODE,
            GetCountsPerRevolution::OPCODE,
            GetTimerFrequency::OPCODE,
            GetPosition::OPCODE,
            GetAxisStatus::OPCODE,
            GetHighSpeedRatio::OPCODE,
            GetGotoTarget::OPCODE,
            GetBreakPoint::OPCODE,
            GetStepPeriod::OPCODE,
        ];
        for op in inquiries {
            assert!(
                op.is_ascii_lowercase(),
                "{:?} inquires and must be lower case",
                op as char
            );
        }
        assert!(!GetPosition::is_action());
        let actions: [u8; 9] = [
            Initialise::OPCODE,
            SetMotionMode::OPCODE,
            SetStepPeriod::OPCODE,
            SetGotoIncrement::OPCODE,
            SetBreakPointIncrement::OPCODE,
            StartMotion::OPCODE,
            StopMotion::OPCODE,
            InstantStop::OPCODE,
            SetGuideRate::OPCODE,
        ];
        for op in actions {
            assert!(
                op.is_ascii_uppercase(),
                "{:?} acts and must be upper case",
                op as char
            );
        }
        assert!(StartMotion::is_action());
        assert!(InstantStop::is_action());
    }

    #[test]
    fn the_readback_inquiries_are_the_lower_case_of_the_registers_they_read() {
        // `H` ↔ `h`, `M` ↔ `m`, `I` ↔ `i`. Asserting it here means a future edit that changes
        // one opcode without the other fails a test rather than a bring-up.
        assert_eq!(
            GetGotoTarget::OPCODE,
            SetGotoIncrement::OPCODE.to_ascii_lowercase()
        );
        assert_eq!(
            GetBreakPoint::OPCODE,
            SetBreakPointIncrement::OPCODE.to_ascii_lowercase()
        );
        assert_eq!(
            GetStepPeriod::OPCODE,
            SetStepPeriod::OPCODE.to_ascii_lowercase()
        );
    }

    #[test]
    fn the_writes_encode_the_payloads_the_spike_sent() {
        // `motion.py`'s E3: `:G120`, `:I1` at 620, `:H1` at 1,000, `:M1` at 500.
        let mode = SetMotionMode {
            axis: Axis::Ra,
            mode: MotionMode::new(MotionKind::Goto, SpeedClass::Low, MotionDirection::Forward),
        };
        assert_eq!(wire(&mode), ":G120");
        let period = SetStepPeriod {
            axis: Axis::Ra,
            period: StepPeriod::SIDEREAL_AT_64935_HZ,
        };
        assert_eq!(wire(&period), ":I16C0200");
        let target = SetGotoIncrement {
            axis: Axis::Ra,
            target: Move::from_delta(1_000).expect("fits"),
        };
        assert_eq!(wire(&target), ":H1E80300");
        let brake = SetBreakPointIncrement {
            axis: Axis::Ra,
            brake: Move::from_delta(500).expect("fits"),
        };
        assert_eq!(wire(&brake), ":M1F40100");
        assert_eq!(wire(&StartMotion::unbounded(Axis::Ra)), ":J1");
    }

    #[test]
    fn a_backward_move_changes_the_mode_digit_and_not_the_increment() {
        // The single most valuable consequence of `Move` carrying both halves.
        let fwd = SetGotoIncrement {
            axis: Axis::Ra,
            target: Move::from_delta(1_000).unwrap(),
        };
        let back = SetGotoIncrement {
            axis: Axis::Ra,
            target: Move::from_delta(-1_000).unwrap(),
        };
        assert_eq!(
            wire(&fwd),
            wire(&back),
            "`H` carries a magnitude in both directions"
        );

        let mode_of = |mv: Move| SetMotionMode {
            axis: Axis::Ra,
            mode: MotionMode::goto(SpeedClass::Low, mv.direction()),
        };
        assert_eq!(wire(&mode_of(fwd.target)), ":G120");
        assert_eq!(wire(&mode_of(back.target)), ":G121");
    }

    #[test]
    fn the_guide_rate_is_one_character_of_payload() {
        let cmd = SetGuideRate {
            axis: Axis::Dec,
            rate: GuideRate::new(2).expect("in range"),
        };
        assert_eq!(wire(&cmd), ":P22");
    }

    #[test]
    fn every_frame_fits_the_protocols_widest() {
        let frames = [
            GetPosition(Axis::Ra).encode(),
            SetGotoIncrement {
                axis: Axis::Ra,
                target: Move::from_delta(16_777_215).unwrap(),
            }
            .encode(),
            SetMotionMode {
                axis: Axis::Dec,
                mode: MotionMode::ALL[7],
            }
            .encode(),
        ];
        for f in frames {
            assert!(
                f.as_bytes().len() <= super::super::frame::MAX_FRAME_LEN,
                "{f:?}"
            );
            assert_eq!(
                f.as_bytes().last(),
                Some(&b'\r'),
                "{f:?} must be terminated"
            );
            assert_eq!(
                f.as_bytes().first(),
                Some(&b':'),
                "{f:?} must start with a colon"
            );
        }
    }
}
