//! The goto sequence, and the readback obligation expressed as a type.
//!
//! # The sequence
//!
//! ```text
//! G  motion mode           kind, speed class, direction
//! I  step period           rate control — and see below
//! H  goto target INCREMENT relative, unsigned; the sign is in G
//! M  break-point increment where deceleration begins
//! --- read back h, m, i and check them ---
//! J  start
//! ```
//!
//! `I` is in the sequence and does **not** control goto speed: a 10× change left the velocity
//! profile identical on real hardware (`spikes/skywatcher-heq5/FINDINGS.md`, E3). It is sent for
//! protocol completeness and read back because a step period that failed to land is evidence the
//! whole write sequence went astray — which is the only thing this codec uses it for.
//!
//! # Why the readback is structural rather than advisory
//!
//! SDD §5.2.2 makes pre-motion verification mandatory, and the reason is specific: **the mount
//! does not validate the axis digit**. `:j9` answers `=000080` for an axis that does not exist,
//! so a corrupted frame does not produce an error, it produces a plausible wrong answer. There
//! is no reply that tells the driver a goto was programmed onto the wrong axis, or with a
//! byte-swapped increment, or not at all. The only thing that does is asking the mount what it
//! thinks it was told.
//!
//! E14 established that asking works: after writing `:H`=12,345 and `:M`=6,172 at position
//! 8,654,669, the undocumented inquiries `:h` and `:m` returned exactly position + increment,
//! and `:i` returned exactly the step period written. Three round trips, ≈48 ms at the measured
//! 16 ms each.
//!
//! An obligation in a document is an obligation someone skips at 2 a.m. So [`StartMotion::goto`]
//! takes a [`GotoVerified`], and [`GotoVerified`] has exactly one constructor:
//! [`GotoProgram::verify`], which fails unless all three registers match. Writing a goto and
//! starting it without checking is not a discipline failure here; it does not compile.
//!
//! [`StartMotion::unbounded`] remains available with no proof, and that is honest rather than a
//! hole: tracking and manual slew have no target register, so there is nothing to read back.
//! What the two constructors buy is that the call site says which kind of motion it is.

use astroctl_core::types::Axis;

use super::command::{
    Command, GetBreakPoint, GetGotoTarget, GetStepPeriod, SetBreakPointIncrement, SetGotoIncrement,
    SetMotionMode, SetStepPeriod,
};
use super::error::EncodeError;
use super::frame::Frame;
use super::motion::{MotionMode, SpeedClass};
use super::value::{Counts, Move, StepPeriod};

/// Which register a readback disagreed about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Register {
    /// `H` written, `h` read: the absolute goto target.
    GotoTarget,
    /// `M` written, `m` read: the absolute break point.
    BreakPoint,
    /// `I` written, `i` read: the step period.
    StepPeriod,
}

impl Register {
    /// The write opcode and the inquiry that reads it back, for a log line.
    #[must_use]
    pub const fn opcodes(self) -> (char, char) {
        match self {
            Self::GotoTarget => ('H', 'h'),
            Self::BreakPoint => ('M', 'm'),
            Self::StepPeriod => ('I', 'i'),
        }
    }
}

impl core::fmt::Display for Register {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (set, get) = self.opcodes();
        let name = match self {
            Self::GotoTarget => "goto target",
            Self::BreakPoint => "break point",
            Self::StepPeriod => "step period",
        };
        write!(f, "{name} (`{set}`/`{get}`)")
    }
}

/// A register the mount was told to set, which then disagreed about what it was told.
///
/// **Nothing has moved when this is produced.** It is raised between the writes and `J`, which
/// is the entire point of the check; a driver surfaces it as a protocol error and abandons the
/// goto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{register} reads {actual} after being set to {expected}")]
pub struct ReadbackMismatch {
    /// Which register.
    pub register: Register,
    /// What the write should have produced.
    pub expected: u32,
    /// What the mount reported.
    pub actual: u32,
}

/// A register write whose value the mount can be asked to repeat back.
///
/// The associated types are the pairing: `Readback` is the inquiry, and its `Response` is
/// constrained to be the same type this write's expectation is computed in, so a mismatched
/// pair — `H` checked against `:i`, say — is a type error rather than a comparison that always
/// fails or, worse, always passes.
pub trait ReadbackVerified: Command {
    /// The inquiry that reads this register.
    type Readback: Command<Response = Self::Value>;
    /// The type both sides are compared in.
    type Value: Copy + PartialEq + core::fmt::Debug + Into<u32>;
    /// Which register this is, for diagnostics.
    const REGISTER: Register;

    /// The inquiry to send after this write.
    fn readback(&self) -> Self::Readback;

    /// What the readback must return.
    ///
    /// `at` is the axis counter at the moment of the write. `H` and `M` are **relative** and
    /// their readbacks are **absolute**, so the expectation depends on where the axis was; `I`
    /// ignores it. Read `:j` immediately before programming, with the axis stopped.
    fn expected(&self, at: Counts) -> Self::Value;

    /// Compare a readback against the expectation.
    ///
    /// # Errors
    /// [`ReadbackMismatch`] naming the register and both values.
    fn check(&self, at: Counts, observed: Self::Value) -> Result<(), ReadbackMismatch>
    where
        Self: Sized,
    {
        let expected = self.expected(at);
        if expected == observed {
            Ok(())
        } else {
            Err(ReadbackMismatch {
                register: Self::REGISTER,
                expected: expected.into(),
                actual: observed.into(),
            })
        }
    }
}

impl ReadbackVerified for SetGotoIncrement {
    type Readback = GetGotoTarget;
    type Value = Counts;
    const REGISTER: Register = Register::GotoTarget;

    fn readback(&self) -> GetGotoTarget {
        GetGotoTarget(self.axis)
    }

    fn expected(&self, at: Counts) -> Counts {
        at.after(self.target)
    }
}

impl ReadbackVerified for SetBreakPointIncrement {
    type Readback = GetBreakPoint;
    type Value = Counts;
    const REGISTER: Register = Register::BreakPoint;

    fn readback(&self) -> GetBreakPoint {
        GetBreakPoint(self.axis)
    }

    fn expected(&self, at: Counts) -> Counts {
        at.after(self.brake)
    }
}

impl ReadbackVerified for SetStepPeriod {
    type Readback = GetStepPeriod;
    type Value = StepPeriod;
    const REGISTER: Register = Register::StepPeriod;

    fn readback(&self) -> GetStepPeriod {
        GetStepPeriod(self.axis)
    }

    fn expected(&self, _at: Counts) -> StepPeriod {
        // The only one of the three that is absolute on both sides.
        self.period
    }
}

/// One programmed goto: the four writes, the three checks, and the permission to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GotoProgram {
    axis: Axis,
    from: Counts,
    mode: MotionMode,
    period: StepPeriod,
    target: Move,
    brake: Move,
}

impl GotoProgram {
    /// Program a goto.
    ///
    /// `from` is the axis counter read immediately before, with the axis stopped: the readback
    /// expectations are computed against it and a stale value invalidates them. `target` is the
    /// signed delta; `brake_counts` is how far into the move deceleration begins.
    ///
    /// # Errors
    /// [`EncodeError::BreakPointOutsideMove`] if the break point is not inside the move.
    pub fn new(
        axis: Axis,
        from: Counts,
        target: Move,
        brake_counts: u32,
        speed: SpeedClass,
        period: StepPeriod,
    ) -> Result<Self, EncodeError> {
        // The brake is derived from the target rather than supplied alongside it, so it cannot
        // point the other way — the failure mode being a mount that decelerates away from where
        // it is going.
        let brake = target.shortened_to(brake_counts)?;
        Ok(Self {
            axis,
            from,
            mode: MotionMode::goto(speed, target.direction()),
            period,
            target,
            brake,
        })
    }

    /// The break point the bring-up experiments used: half the move, never less than one count.
    ///
    /// `motion.py` sent this for every goto it ran, and E13 measured **zero** counts of error at
    /// 1,000, 10,000 and 100,000 counts in both directions. It is a policy rather than a
    /// protocol fact — M3-T03 owns the choice — but it is the policy with evidence behind it.
    #[must_use]
    pub const fn midpoint_break(target: Move) -> u32 {
        let half = target.magnitude().get() / 2;
        if half == 0 {
            1
        } else {
            half
        }
    }

    /// The axis.
    #[must_use]
    pub const fn axis(&self) -> Axis {
        self.axis
    }

    /// Where the axis was when this was programmed.
    #[must_use]
    pub const fn from(&self) -> Counts {
        self.from
    }

    /// Where the axis should end up.
    #[must_use]
    pub const fn destination(&self) -> Counts {
        self.from.after(self.target)
    }

    /// `G`, the first write.
    #[must_use]
    pub const fn motion_mode(&self) -> SetMotionMode {
        SetMotionMode {
            axis: self.axis,
            mode: self.mode,
        }
    }

    /// `I`, the second write.
    #[must_use]
    pub const fn step_period(&self) -> SetStepPeriod {
        SetStepPeriod {
            axis: self.axis,
            period: self.period,
        }
    }

    /// `H`, the third write.
    #[must_use]
    pub const fn goto_increment(&self) -> SetGotoIncrement {
        SetGotoIncrement {
            axis: self.axis,
            target: self.target,
        }
    }

    /// `M`, the fourth write.
    #[must_use]
    pub const fn break_point(&self) -> SetBreakPointIncrement {
        SetBreakPointIncrement {
            axis: self.axis,
            brake: self.brake,
        }
    }

    /// The four write frames in protocol order, for a serial task that would rather loop than
    /// name four types. Each is acknowledged with a bare `=`.
    #[must_use]
    pub fn writes(&self) -> [Frame; 4] {
        [
            self.motion_mode().encode(),
            self.step_period().encode(),
            self.goto_increment().encode(),
            self.break_point().encode(),
        ]
    }

    /// The three readback inquiries, in the order [`GotoObservation`]'s fields expect them.
    #[must_use]
    pub const fn readbacks(&self) -> (GetGotoTarget, GetBreakPoint, GetStepPeriod) {
        (
            GetGotoTarget(self.axis),
            GetBreakPoint(self.axis),
            GetStepPeriod(self.axis),
        )
    }

    /// What the three readbacks must return if the writes landed.
    ///
    /// Exposed so a driver can log the expectation next to the observation, and so a mock port
    /// in M3-T02 can be scripted to satisfy a real program rather than a hand-written string.
    #[must_use]
    pub fn expectation(&self) -> GotoObservation {
        GotoObservation {
            target: self.goto_increment().expected(self.from),
            brake: self.break_point().expected(self.from),
            period: self.step_period().expected(self.from),
        }
    }

    /// Check the three readbacks and, on success, hand back the only thing that permits `J`.
    ///
    /// # Errors
    /// The first [`ReadbackMismatch`] found, checking target, then break point, then step
    /// period. First rather than all three: the driver's response to any of them is the same —
    /// abandon the goto — and the first is the one that says most about what went wrong.
    pub fn verify(&self, observed: GotoObservation) -> Result<GotoVerified, ReadbackMismatch> {
        self.goto_increment().check(self.from, observed.target)?;
        self.break_point().check(self.from, observed.brake)?;
        self.step_period().check(self.from, observed.period)?;
        Ok(GotoVerified { axis: self.axis })
    }
}

/// What the three readbacks actually returned.
///
/// Named fields rather than three positional arguments because two of them are [`Counts`] and
/// would swap silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GotoObservation {
    /// `:h` — the absolute goto target.
    pub target: Counts,
    /// `:m` — the absolute break point.
    pub brake: Counts,
    /// `:i` — the step period.
    pub period: StepPeriod,
}

/// Proof that a goto's three registers were read back and matched.
///
/// The only constructor is [`GotoProgram::verify`]. It carries nothing but the axis, and that is
/// the point: its value is entirely in the fact that it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GotoVerified {
    axis: Axis,
}

impl GotoVerified {
    /// The axis whose goto was verified.
    #[must_use]
    pub const fn axis(&self) -> Axis {
        self.axis
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::command::StartMotion;

    /// E14's numbers, which are the only hardware evidence for the readback registers.
    fn e14_program() -> GotoProgram {
        GotoProgram::new(
            Axis::Ra,
            Counts::new(8_654_669).expect("fits"),
            Move::from_delta(12_345).expect("fits"),
            6_172,
            SpeedClass::Low,
            StepPeriod::SIDEREAL_AT_64935_HZ,
        )
        .expect("6,172 is inside a 12,345-count move")
    }

    #[test]
    fn the_program_reproduces_the_frames_the_spike_sent() {
        let p = e14_program();
        let rendered: Vec<String> = p.writes().iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            // `:G1` GOTO/low/forward, `:I1` 620, `:H1` 12,345, `:M1` 6,172 — byte-swapped.
            vec![":G120", ":I16C0200", ":H1393000", ":M11C1800"]
        );
    }

    #[test]
    fn the_expectation_is_the_absolute_pair_e14_measured() {
        // Position 8,654,669: `:h` read 8,667,014 and `:m` read 8,660,841. Both are position
        // plus the increment, exactly.
        let p = e14_program();
        let expect = p.expectation();
        assert_eq!(expect.target.get(), 8_667_014);
        assert_eq!(expect.brake.get(), 8_660_841);
        assert_eq!(expect.period, StepPeriod::SIDEREAL_AT_64935_HZ);
        assert_eq!(p.destination().get(), 8_667_014);
    }

    #[test]
    fn a_matching_readback_yields_the_permission_to_start() {
        let p = e14_program();
        let proof = p
            .verify(p.expectation())
            .expect("the expectation matches itself");
        assert_eq!(proof.axis(), Axis::Ra);
        assert_eq!(StartMotion::goto(&proof).encode().to_string(), ":J1");
        assert_eq!(StartMotion::goto(&proof).axis(), Axis::Ra);
    }

    #[test]
    fn each_register_can_fail_on_its_own_and_says_which() {
        let p = e14_program();
        let good = p.expectation();

        let wrong_target = GotoObservation {
            target: Counts::new(8_667_015).unwrap(),
            ..good
        };
        assert_eq!(
            p.verify(wrong_target),
            Err(ReadbackMismatch {
                register: Register::GotoTarget,
                expected: 8_667_014,
                actual: 8_667_015,
            })
        );

        let wrong_brake = GotoObservation {
            brake: Counts::HOME,
            ..good
        };
        assert_eq!(
            p.verify(wrong_brake).unwrap_err().register,
            Register::BreakPoint
        );

        let wrong_period = GotoObservation {
            period: StepPeriod::new(6_200).unwrap(),
            ..good
        };
        assert_eq!(
            p.verify(wrong_period).unwrap_err().register,
            Register::StepPeriod
        );
    }

    #[test]
    fn the_byte_swap_fault_the_readback_exists_to_catch_is_caught() {
        // The exact failure SDD §5.2.2 names: an increment that reached the mount with its bytes
        // in the wrong order. 12,345 = 0x003039; transposed as 0x393000 it is 3,748,352.
        let p = e14_program();
        let landed = 8_654_669 + 0x0039_3000;
        let mangled = GotoObservation {
            target: Counts::new(landed).unwrap(),
            ..p.expectation()
        };
        let err = p
            .verify(mangled)
            .expect_err("a byte-swapped increment must not pass");
        assert_eq!(err.register, Register::GotoTarget);
        assert_eq!(err.expected, 8_667_014);
        assert_eq!(err.actual, landed);
        assert_eq!(
            err.to_string(),
            format!("goto target (`H`/`h`) reads {landed} after being set to 8667014")
        );
    }

    #[test]
    fn a_backward_goto_expects_the_counter_to_fall() {
        let from = Counts::new(8_654_669).unwrap();
        let p = GotoProgram::new(
            Axis::Dec,
            from,
            Move::from_delta(-12_345).unwrap(),
            6_172,
            SpeedClass::High,
            StepPeriod::SIDEREAL_AT_64935_HZ,
        )
        .expect("valid");
        // `H` carries the same magnitude either way; `G` is what changed.
        assert_eq!(p.goto_increment().encode().to_string(), ":H2393000");
        assert_eq!(
            p.motion_mode().encode().to_string(),
            ":G201",
            "GOTO, high, backward"
        );
        assert_eq!(p.expectation().target.get(), 8_654_669 - 12_345);
        assert_eq!(p.expectation().brake.get(), 8_654_669 - 6_172);
    }

    #[test]
    fn the_break_point_cannot_point_away_from_the_move() {
        let from = Counts::HOME;
        assert!(matches!(
            GotoProgram::new(
                Axis::Ra,
                from,
                Move::from_delta(1_000).unwrap(),
                2_000,
                SpeedClass::Low,
                StepPeriod::SIDEREAL_AT_64935_HZ,
            ),
            Err(EncodeError::BreakPointOutsideMove {
                brake: 2_000,
                total: 1_000
            })
        ));
    }

    #[test]
    fn the_readback_inquiries_target_the_same_axis_as_the_writes() {
        // A goto programmed on DEC that verified against RA's registers would pass while the
        // wrong axis moved. The pairing is per-program, not per-driver.
        let p = GotoProgram::new(
            Axis::Dec,
            Counts::HOME,
            Move::from_delta(1_000).unwrap(),
            500,
            SpeedClass::Low,
            StepPeriod::SIDEREAL_AT_64935_HZ,
        )
        .expect("valid");
        let (h, m, i) = p.readbacks();
        assert_eq!(h.encode().to_string(), ":h2");
        assert_eq!(m.encode().to_string(), ":m2");
        assert_eq!(i.encode().to_string(), ":i2");
    }

    #[test]
    fn the_midpoint_break_is_the_one_the_experiments_measured_zero_error_with() {
        assert_eq!(
            GotoProgram::midpoint_break(Move::from_delta(1_000).unwrap()),
            500
        );
        assert_eq!(
            GotoProgram::midpoint_break(Move::from_delta(-100_000).unwrap()),
            50_000
        );
        // A one-count move still needs a break point inside it.
        assert_eq!(GotoProgram::midpoint_break(Move::from_delta(1).unwrap()), 1);
        let mv = Move::from_delta(1).unwrap();
        assert!(mv.shortened_to(GotoProgram::midpoint_break(mv)).is_ok());
    }
}
