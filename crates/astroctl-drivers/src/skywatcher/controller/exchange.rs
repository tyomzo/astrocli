//! The one thing this task needs from the serial task, and the sequences built on it.
//!
//! # Why a trait here rather than a dependency on M3-T02
//!
//! M3-T01's [`Command`] already fixes the shape: one method, generic over the command, returning
//! that command's own response type. Its module documentation writes the signature out —
//!
//! ```ignore
//! async fn send<C: Command>(&self, cmd: C) -> Result<C::Response, DeviceError>;
//! ```
//!
//! — and that is exactly [`Exchange::send`] with the error type left open. So M3-T04's glue is an
//! impl with one forwarding line, and this task can be written, tested and reviewed before the
//! serial task exists. Nothing here names a port, a runtime or `serialport`; the crate still
//! compiles with `--no-default-features -F skywatcher` against `thiserror` alone.
//!
//! The error type is associated rather than fixed to `DeviceError` for one reason: a test double
//! wants to return its own error, and a seam that forced every implementor through the HAL's
//! error enum would make the smallest possible mock the largest possible dependency.
//!
//! # What is worth sequencing here at all
//!
//! Two things, and only two, because everything else in this module is a value the caller sends
//! at its own pace:
//!
//! * [`handshake`] — five inquiries whose *results feed each other's interpretation*, and which
//!   must be read before any motion. It sends **only lowercase opcodes**; the spike's own
//!   read-only harnesses "refuse to transmit ANY uppercase byte" and
//!   [`the_handshake_never_transmits_an_action_opcode`](tests::the_handshake_never_transmits_an_action_opcode)
//!   holds this one to the same rule.
//! * [`run_goto`] — the four writes, the three mandatory readbacks, the comparison, and `J`. This
//!   is where M3-T01's proof-carrying design becomes an executed obligation rather than an
//!   expressible one: [`GotoVerified`](crate::skywatcher::codec::GotoVerified) has exactly one
//!   constructor, `GotoProgram::verify`, and this function is the only caller of it in the driver.
//!
//! Everything else — starting tracking, stopping, polling — is a single command, and a helper
//! that sent one command would only hide which one.

use core::fmt::{Debug, Display};
use core::future::Future;

use astroctl_core::types::Axis;

use crate::skywatcher::codec::{
    Command, GetCountsPerRevolution, GetFirmwareVersion, GetHighSpeedRatio, GetPosition,
    GetTimerFrequency, GotoObservation, GotoProgram, ReadbackMismatch, StartMotion,
};

use super::{AxisParams, MotorController};

/// One request/response exchange with the mount.
///
/// The whole seam. An implementor is the serial task (M3-T02) or a test double; nothing else in
/// this module knows the difference.
pub trait Exchange {
    /// What a failed exchange reports — a timeout, a refusal, a malformed reply.
    type Error;

    /// Send one command and decode its reply.
    ///
    /// `Send` on the returned future because the field node runs this on a multi-threaded
    /// runtime and a sequence that could not cross a task boundary would have to be driven from
    /// the task that owns the port — which is the one place it cannot be.
    fn send<C>(&self, command: C) -> impl Future<Output = Result<C::Response, Self::Error>> + Send
    where
        C: Command + Send;
}

/// What a sequence over an [`Exchange`] can report.
///
/// Two shapes, because there are two kinds of failure and the operator's response to them
/// differs: the link failed (retry, then degrade — SDD §5.2.4), or the mount disagreed about what
/// it was told (abandon the motion, and something is wrong with the wire or the frame).
#[derive(Debug, thiserror::Error)]
pub enum SequenceError<E: Debug + Display> {
    /// The exchange itself failed.
    #[error("{0}")]
    Link(E),

    /// A register read back something other than what was written. **Nothing has moved.**
    #[error(transparent)]
    Readback(#[from] ReadbackMismatch),
}

impl<E: Debug + Display> SequenceError<E> {
    /// The link error, if that is what this was.
    #[must_use]
    pub const fn link(&self) -> Option<&E> {
        match self {
            Self::Link(error) => Some(error),
            Self::Readback(_) => None,
        }
    }
}

/// Read one axis's parameters (SDD §5.2.2's handshake).
///
/// Five inquiries, no actions: firmware, counts per revolution, timer frequency, high-speed
/// ratio, and the position that a caller needs before it can program anything relative. `F` is
/// deliberately *not* sent — it is an action opcode, and whether to initialise an axis is a
/// decision for the layer that knows whether the mount has been power-cycled. The command is
/// [`MotorController::initialise`] when that layer wants it.
///
/// # Errors
/// [`SequenceError::Link`] on the first exchange that fails.
pub async fn handshake<E>(io: &E, axis: Axis) -> Result<AxisParams, SequenceError<E::Error>>
where
    E: Exchange + Sync,
    E::Error: Debug + Display,
{
    let firmware = io
        .send(GetFirmwareVersion(axis))
        .await
        .map_err(SequenceError::Link)?;
    let counts_per_revolution = io
        .send(GetCountsPerRevolution(axis))
        .await
        .map_err(SequenceError::Link)?;
    let timer_frequency = io
        .send(GetTimerFrequency(axis))
        .await
        .map_err(SequenceError::Link)?;
    let high_speed_ratio = io
        .send(GetHighSpeedRatio(axis))
        .await
        .map_err(SequenceError::Link)?;

    Ok(AxisParams {
        axis,
        firmware,
        counts_per_revolution,
        timer_frequency,
        high_speed_ratio,
    })
}

/// Read an axis counter — the 1 Hz poll, and the value a goto must be programmed against.
///
/// # Errors
/// [`SequenceError::Link`] if the exchange fails.
pub async fn read_position<E>(
    io: &E,
    controller: &MotorController,
) -> Result<crate::skywatcher::codec::Counts, SequenceError<E::Error>>
where
    E: Exchange + Sync,
    E::Error: Debug + Display,
{
    io.send(GetPosition(controller.axis()))
        .await
        .map_err(SequenceError::Link)
}

/// Program a goto, verify it, and start it.
///
/// ```text
/// G  motion mode          I  step period        H  goto increment      M  break point
/// --- read back h, m, i and compare ---
/// J  start
/// ```
///
/// **The readbacks are why this function exists.** The mount does not validate the axis digit —
/// `:j9` answers `=000080` for an axis that is not there — so a corrupted frame produces a
/// plausible wrong answer rather than an error, and the only thing that catches it is asking the
/// mount what it thinks it was told. SDD §5.2.2 makes the check mandatory; M3-T01 made it a type;
/// this is where it is performed. On mismatch nothing has moved and `J` is never sent.
///
/// Three extra round trips, ≈48 ms at the measured 16 ms each, against a goto sequence that is
/// ~128 ms of dead air before the mount stirs.
///
/// # Errors
/// [`SequenceError::Link`] if any exchange fails, or [`SequenceError::Readback`] naming the first
/// register that disagreed.
pub async fn run_goto<E>(io: &E, program: &GotoProgram) -> Result<(), SequenceError<E::Error>>
where
    E: Exchange + Sync,
    E::Error: Debug + Display,
{
    io.send(program.motion_mode())
        .await
        .map_err(SequenceError::Link)?;
    io.send(program.step_period())
        .await
        .map_err(SequenceError::Link)?;
    io.send(program.goto_increment())
        .await
        .map_err(SequenceError::Link)?;
    io.send(program.break_point())
        .await
        .map_err(SequenceError::Link)?;

    let (target, brake, period) = program.readbacks();
    let observed = GotoObservation {
        target: io.send(target).await.map_err(SequenceError::Link)?,
        brake: io.send(brake).await.map_err(SequenceError::Link)?,
        period: io.send(period).await.map_err(SequenceError::Link)?,
    };

    // The only call to `verify` in the driver, and therefore the only route to a `GotoVerified`.
    let proof = program.verify(observed)?;
    io.send(StartMotion::goto(&proof))
        .await
        .map_err(SequenceError::Link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::{
        Counts, MotionDirection, Move, ProtocolError, SpeedClass, StepPeriod,
    };
    use crate::skywatcher::controller::fixture;
    use std::sync::Mutex;

    /// A mount that answers the way the operator's HEQ5 answered, and records what it was asked.
    ///
    /// It replies with *payload strings* and lets each command decode its own, which is what makes
    /// it a test of the sequences rather than of a hand-written expectation: a command pointed at
    /// the wrong payload fails here exactly as it would on the wire.
    ///
    /// The goto registers are modelled rather than scripted — `H` and `M` are relative writes
    /// whose readbacks are absolute, and a fixture that returned a fixed string would not catch a
    /// sequence that programmed the right numbers against the wrong position.
    #[derive(Debug)]
    struct ScriptedMount {
        state: Mutex<State>,
    }

    #[derive(Debug)]
    struct State {
        sent: Vec<String>,
        position: u32,
        goto_target: u32,
        break_point: u32,
        step_period: u32,
        /// Corrupt the `:h` readback by this many counts — the byte-swap fault, simulated.
        corrupt_target_by: i64,
        started: bool,
    }

    impl ScriptedMount {
        fn new() -> Self {
            Self {
                state: Mutex::new(State {
                    sent: Vec::new(),
                    position: 8_654_669,
                    goto_target: 0,
                    break_point: 0,
                    step_period: 0,
                    corrupt_target_by: 0,
                    started: false,
                }),
            }
        }

        fn corrupting_the_goto_target(self, by: i64) -> Self {
            self.state.lock().expect("uncontended").corrupt_target_by = by;
            self
        }

        fn sent(&self) -> Vec<String> {
            self.state.lock().expect("uncontended").sent.clone()
        }

        fn started(&self) -> bool {
            self.state.lock().expect("uncontended").started
        }

        /// The payload the mount would answer with, given a request frame.
        fn reply(&self, frame: &str) -> String {
            use crate::skywatcher::codec::{encode_u24, U24};
            let mut state = self.state.lock().expect("uncontended");
            state.sent.push(frame.to_owned());

            let hex = |value: u32| {
                String::from_utf8(
                    encode_u24(U24::new(value & 0x00FF_FFFF).expect("masked")).to_vec(),
                )
                .expect("hex is ASCII")
            };
            // `:X<axis><payload>` — the opcode is the character after the colon.
            let opcode = frame.as_bytes().get(1).copied().unwrap_or(b'?');
            let payload = frame.get(3..).unwrap_or_default().to_owned();
            let written = |payload: &str| {
                // The writes are byte-swapped hex, so decode them the way the mount does.
                crate::skywatcher::codec::decode_u24(payload, '?')
                    .map(U24::get)
                    .unwrap_or(0)
            };

            match opcode {
                b'e' => "020401".to_owned(),
                b'a' => hex(fixture::CPR),
                b'b' => hex(fixture::TIMER_HZ),
                b'g' => "10".to_owned(),
                b'j' => hex(state.position),
                b'f' => "101".to_owned(),
                b'h' => {
                    let target = state.goto_target;
                    let corrupt = state.corrupt_target_by;
                    hex((i64::from(target) + corrupt) as u32)
                }
                b'm' => hex(state.break_point),
                b'i' => hex(state.step_period),
                b'I' => {
                    state.step_period = written(&payload);
                    String::new()
                }
                b'H' => {
                    // Relative write, absolute readback — the asymmetry E14 measured.
                    state.goto_target = state.position.wrapping_add(written(&payload));
                    String::new()
                }
                b'M' => {
                    state.break_point = state.position.wrapping_add(written(&payload));
                    String::new()
                }
                b'J' => {
                    state.started = true;
                    String::new()
                }
                _ => String::new(),
            }
        }
    }

    impl Exchange for ScriptedMount {
        type Error = ProtocolError;

        async fn send<C>(&self, command: C) -> Result<C::Response, ProtocolError>
        where
            C: Command + Send,
        {
            let frame = command.encode().to_string();
            let payload = self.reply(&frame);
            command.decode(&payload)
        }
    }

    fn program(controller: &super::MotorController) -> GotoProgram {
        controller
            .goto(
                Counts::new(8_654_669).expect("fits"),
                Move::from_delta(12_345).expect("fits"),
                SpeedClass::Low,
            )
            .expect("a non-zero move")
    }

    #[tokio::test]
    async fn the_handshake_reads_the_parameters_the_operators_mount_reported() {
        let mount = ScriptedMount::new();
        let params = handshake(&mount, Axis::Ra)
            .await
            .expect("the mount answered");
        assert_eq!(params.counts_per_revolution.0.get(), fixture::CPR);
        assert_eq!(params.timer_frequency.0.get(), fixture::TIMER_HZ);
        assert_eq!(params.high_speed_ratio.0, fixture::HIGH_SPEED_RATIO);
        assert_eq!(params.firmware.model_code, 0x01, "an HEQ5");

        // ...and it produces a controller whose sidereal period is the measured 620.
        let controller = MotorController::new(params).expect("valid parameters");
        let sequence = controller
            .track(
                astroctl_core::types::TrackingMode::Sidereal,
                MotionDirection::Forward,
            )
            .expect("in range");
        assert_eq!(sequence.rate().period(), StepPeriod::SIDEREAL_AT_64935_HZ);
    }

    #[tokio::test]
    async fn the_handshake_never_transmits_an_action_opcode() {
        // The rule the spike's read-only harnesses enforced on themselves, enforced here on a
        // sequence that runs before anything has been proved about the mount.
        let mount = ScriptedMount::new();
        handshake(&mount, Axis::Dec).await.expect("answered");
        for frame in mount.sent() {
            let opcode = frame.as_bytes()[1];
            assert!(
                opcode.is_ascii_lowercase(),
                "the handshake sent `{frame}`, which can move the mount"
            );
            assert!(frame.starts_with(":") && frame.contains('2'));
        }
        assert!(!mount.started());
    }

    #[tokio::test]
    async fn a_goto_reads_back_all_three_registers_before_it_starts() {
        let controller = fixture::controller(Axis::Ra);
        let mount = ScriptedMount::new();
        run_goto(&mount, &program(&controller))
            .await
            .expect("the readbacks match");

        let sent = mount.sent();
        let opcodes: Vec<char> = sent
            .iter()
            .map(|frame| char::from(frame.as_bytes()[1]))
            .collect();
        assert_eq!(
            opcodes,
            vec!['G', 'I', 'H', 'M', 'h', 'm', 'i', 'J'],
            "the sequence SDD §5.2.2 mandates, in order"
        );
        assert!(mount.started());
    }

    #[tokio::test]
    async fn a_readback_that_disagrees_stops_the_goto_before_the_motors_are_commanded() {
        // The exact failure SDD §5.2.2 names: an increment that reached the mount with its bytes
        // in the wrong order. 12,345 = 0x003039; transposed it is 0x393000 = 3,748,352, so the
        // readback is off by the difference.
        let controller = fixture::controller(Axis::Ra);
        let mangled = 0x0039_3000 - 12_345;
        let mount = ScriptedMount::new().corrupting_the_goto_target(i64::from(mangled));

        let error = run_goto(&mount, &program(&controller))
            .await
            .expect_err("a byte-swapped increment must not start a motion");
        assert!(matches!(error, SequenceError::Readback(_)));
        assert!(error.link().is_none());
        assert!(
            !mount.started(),
            "`J` must not be sent after a failed readback"
        );
        assert!(
            !mount.sent().iter().any(|frame| frame.starts_with(":J")),
            "the frames were {:?}",
            mount.sent()
        );
    }

    #[tokio::test]
    async fn a_link_failure_is_reported_as_a_link_failure_and_not_as_a_disagreement() {
        // The two error shapes lead to different operator responses — retry versus abandon — so
        // conflating them would send a node into a retry loop against a mount that is answering
        // consistently and wrongly.
        struct DeadLink;

        impl Exchange for DeadLink {
            type Error = ProtocolError;

            async fn send<C>(&self, _command: C) -> Result<C::Response, ProtocolError>
            where
                C: Command + Send,
            {
                Err(ProtocolError::Empty)
            }
        }

        let controller = fixture::controller(Axis::Ra);
        let error = run_goto(&DeadLink, &program(&controller))
            .await
            .expect_err("a dead link cannot program a goto");
        assert!(matches!(error, SequenceError::Link(_)));
        assert!(error.link().is_some());

        assert!(handshake(&DeadLink, Axis::Ra).await.is_err());
    }

    #[tokio::test]
    async fn the_position_read_targets_the_controllers_own_axis() {
        let mount = ScriptedMount::new();
        let controller = fixture::controller(Axis::Dec);
        let at = read_position(&mount, &controller)
            .await
            .expect("the mount answered");
        assert_eq!(at.get(), 8_654_669);
        assert_eq!(mount.sent(), vec![":j2"]);
    }
}
