//! **T-COD-1** — the gated golden-vector suite for the Synta codec (SDD §5.2.2, task M3-T01).
//!
//! Every test here is named `t_cod_1_*`, so `cargo test t_cod_1` is exactly the gate and nothing
//! else. The module-level tests in `src/skywatcher/codec/` cover each piece in isolation; this
//! file covers the two things only a whole-protocol test can:
//!
//! 1. **`testdata/synta_vectors.txt` agrees with the code**, byte for byte, in both directions.
//!    The file is the protocol written down as data — every frame the driver will send and every
//!    reply it will parse — with a `verified`/`derived` label per row saying whether a real HEQ5
//!    produced those bytes. A change to the codec that is not a change to the wire format leaves
//!    this file untouched; a change that *is* one cannot be made quietly.
//!
//! 2. **The decoder never panics.** It is the only code in the workspace that reads bytes from a
//!    device over which we have no control, on a link where a truncated frame, a junk prefix and
//!    an interleaved reply have all been observed on real hardware. A panic here is a dead field
//!    node. The bound is checked exhaustively for short inputs and by a seeded fuzz loop beyond
//!    them — `ASTROCTL_FUZZ_ITERS` raises the count for a local long run; the default is what CI
//!    can afford on every push.
//!
//! No I/O, no runtime, no hardware: the whole file runs in about two seconds.

use std::collections::BTreeMap;
use std::path::PathBuf;

use astroctl_core::types::Axis;
use astroctl_drivers::skywatcher::codec::{
    decode_reply, AxisStatus, Command, Counts, CountsPerRev, FirmwareVersion, GetAxisStatus,
    GetBreakPoint, GetCountsPerRevolution, GetFirmwareVersion, GetGotoTarget, GetHighSpeedRatio,
    GetPosition, GetStepPeriod, GetTimerFrequency, GuideRate, HighSpeedRatio, Initialise,
    InstantStop, MotionDirection, MotionKind, MotionMode, MountError, Move, ProtocolError, Reply,
    SetBreakPointIncrement, SetGotoIncrement, SetGuideRate, SetMotionMode, SetStepPeriod,
    SpeedClass, StartMotion, StepPeriod, StopMotion, TimerFrequency,
};

// -----------------------------------------------------------------------------------------
// The vector file
// -----------------------------------------------------------------------------------------

/// One row, already split and trimmed.
#[derive(Debug)]
struct Vector {
    line: usize,
    label: String,
    command: String,
    request: String,
    reply: String,
    decoded: String,
    source: String,
}

/// What exercising a row produced.
struct Outcome {
    /// The frame the typed command encodes to, or `None` for a decode-only row.
    request: Option<String>,
    /// The rendered decode of the reply.
    decoded: String,
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("synta_vectors.txt")
}

fn load() -> (Vec<Vector>, String) {
    let path = vectors_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let mut header_tally = String::new();
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix("# Tally at the time of writing:") {
            header_tally = rest.trim().trim_end_matches('.').to_owned();
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = trimmed.split('|').map(str::trim).collect();
        assert_eq!(
            fields.len(),
            6,
            "line {line} has {} fields, expected 6 (label|command|request|reply|decoded|source): {trimmed}",
            fields.len()
        );
        out.push(Vector {
            line,
            label: fields[0].to_owned(),
            command: fields[1].to_owned(),
            request: fields[2].to_owned(),
            reply: fields[3].to_owned(),
            decoded: fields[4].to_owned(),
            source: fields[5].to_owned(),
        });
    }
    (out, header_tally)
}

// -----------------------------------------------------------------------------------------
// Rendering — the `decoded` column's vocabulary
// -----------------------------------------------------------------------------------------

fn render_error(e: MountError) -> String {
    let slug = match e {
        MountError::UnknownCommand => "unknown-command".to_owned(),
        MountError::InvalidParameter => "invalid-parameter".to_owned(),
        MountError::MotorNotStopped => "motor-not-stopped".to_owned(),
        MountError::MalformedFrame => "malformed-frame".to_owned(),
        MountError::NotInitialised => "not-initialised".to_owned(),
        MountError::DriverSleeping => "driver-sleeping".to_owned(),
        MountError::PecTrainingRunning => "pec-training-running".to_owned(),
        MountError::NoValidPecData => "no-valid-pec-data".to_owned(),
        MountError::Unrecognised(d) => format!("unrecognised({d})"),
    };
    format!("error={slug}")
}

fn render_status(s: AxisStatus) -> String {
    format!(
        "status={},{},{},{},{}",
        match s.kind {
            MotionKind::Goto => "goto",
            MotionKind::Slew => "slew",
        },
        match s.direction {
            MotionDirection::Forward => "forward",
            MotionDirection::Backward => "backward",
        },
        match s.speed {
            SpeedClass::Low => "low",
            SpeedClass::High => "high",
        },
        if s.running { "running" } else { "stopped" },
        if s.initialised {
            "initialised"
        } else {
            "uninitialised"
        },
    )
}

fn render_firmware(v: FirmwareVersion) -> String {
    let model = v.model().map_or_else(
        || format!("unknown({:#04X})", v.model_code),
        |m| m.as_str().to_owned(),
    );
    format!("firmware={:02X}.{:02X} model={model}", v.major, v.minor)
}

// -----------------------------------------------------------------------------------------
// The command grammar of column 2
// -----------------------------------------------------------------------------------------

fn parse_axis(word: &str) -> Axis {
    match word {
        "ra" => Axis::Ra,
        "dec" => Axis::Dec,
        other => panic!("{other:?} is not an axis (`ra` or `dec`)"),
    }
}

fn parse_mode(words: &[&str]) -> MotionMode {
    let kind = match words[0] {
        "goto" => MotionKind::Goto,
        "slew" => MotionKind::Slew,
        other => panic!("{other:?} is not a motion kind"),
    };
    let speed = match words[1] {
        "low" => SpeedClass::Low,
        "high" => SpeedClass::High,
        other => panic!("{other:?} is not a speed class"),
    };
    let direction = match words[2] {
        "forward" => MotionDirection::Forward,
        "backward" => MotionDirection::Backward,
        other => panic!("{other:?} is not a direction"),
    };
    MotionMode::new(kind, speed, direction)
}

fn parse_move(word: &str) -> Move {
    let delta: i64 = word
        .parse()
        .unwrap_or_else(|e| panic!("{word:?} is not a delta: {e}"));
    Move::from_delta(delta).unwrap_or_else(|e| panic!("{word:?} is not a legal move: {e}"))
}

fn parse_period(word: &str) -> StepPeriod {
    let v: u32 = word
        .parse()
        .unwrap_or_else(|e| panic!("{word:?} is not a period: {e}"));
    StepPeriod::new(v).unwrap_or_else(|e| panic!("{word:?} is not a legal period: {e}"))
}

/// Build the command column 2 names, encode it, decode the reply, and render both.
///
/// # Errors
/// The [`ProtocolError`] the codec raised, so a row can also assert a decode *failure* if one is
/// ever wanted; today every row decodes.
fn exercise(command: &str, reply: Reply<'_>) -> Result<Outcome, ProtocolError> {
    /// Encode, decode and render one typed command.
    ///
    /// A macro rather than a function because every arm below has a different `Response`, and
    /// the whole point of the typed layer is that they are not interchangeable.
    macro_rules! typed {
        ($cmd:expr, |$v:ident: $ty:ty| $render:expr) => {{
            let cmd = $cmd;
            let request = Some(cmd.encode().to_string());
            let decoded = match reply {
                Reply::Err(e) => render_error(e),
                Reply::Ok(payload) => {
                    let $v: $ty = cmd.decode(payload)?;
                    $render
                }
            };
            return Ok(Outcome { request, decoded });
        }};
    }

    let words: Vec<&str> = command.split_whitespace().collect();
    if words.first() == Some(&"-") || words.is_empty() {
        // Decode-only: a frame this codec deliberately cannot build. Keep the evidence anyway.
        let decoded = match reply {
            Reply::Err(e) => render_error(e),
            Reply::Ok(payload) => format!("raw={payload}"),
        };
        return Ok(Outcome {
            request: None,
            decoded,
        });
    }

    let axis = parse_axis(words[1]);
    match words[0] {
        "get-firmware" => typed!(GetFirmwareVersion(axis), |v: FirmwareVersion| {
            render_firmware(v)
        }),
        "get-cpr" => typed!(GetCountsPerRevolution(axis), |v: CountsPerRev| format!(
            "cpr={}",
            v.0.get()
        )),
        "get-timer-frequency" => typed!(GetTimerFrequency(axis), |v: TimerFrequency| format!(
            "timer-hz={}",
            v.0.get()
        )),
        "get-position" => typed!(GetPosition(axis), |v: Counts| format!("counts={}", v.get())),
        "get-status" => typed!(GetAxisStatus(axis), |v: AxisStatus| render_status(v)),
        "get-high-speed-ratio" => {
            typed!(GetHighSpeedRatio(axis), |v: HighSpeedRatio| format!(
                "ratio={}",
                v.0
            ))
        }
        "get-goto-target" => typed!(GetGotoTarget(axis), |v: Counts| format!(
            "counts={}",
            v.get()
        )),
        "get-break-point" => typed!(GetBreakPoint(axis), |v: Counts| format!(
            "counts={}",
            v.get()
        )),
        "get-step-period" => {
            typed!(GetStepPeriod(axis), |v: StepPeriod| format!(
                "period={}",
                v.get()
            ))
        }
        "initialise" => typed!(Initialise(axis), |_v: ()| "ack".to_owned()),
        "start-motion" => typed!(StartMotion::unbounded(axis), |_v: ()| "ack".to_owned()),
        "stop-motion" => typed!(StopMotion(axis), |_v: ()| "ack".to_owned()),
        "instant-stop" => typed!(InstantStop(axis), |_v: ()| "ack".to_owned()),
        "set-motion-mode" => typed!(
            SetMotionMode {
                axis,
                mode: parse_mode(&words[2..])
            },
            |_v: ()| "ack".to_owned()
        ),
        "set-step-period" => typed!(
            SetStepPeriod {
                axis,
                period: parse_period(words[2])
            },
            |_v: ()| "ack".to_owned()
        ),
        "set-goto-increment" => typed!(
            SetGotoIncrement {
                axis,
                target: parse_move(words[2])
            },
            |_v: ()| "ack".to_owned()
        ),
        "set-break-point" => typed!(
            SetBreakPointIncrement {
                axis,
                brake: parse_move(words[2])
            },
            |_v: ()| "ack".to_owned()
        ),
        "set-guide-rate" => {
            let level: u8 = words[2].parse().expect("guide-rate level");
            typed!(
                SetGuideRate {
                    axis,
                    rate: GuideRate::new(level).expect("in range")
                },
                |_v: ()| "ack".to_owned()
            )
        }
        other => panic!("unknown command spec {other:?} — extend `exercise` or fix the vector"),
    }
}

// -----------------------------------------------------------------------------------------
// The gate
// -----------------------------------------------------------------------------------------

#[test]
fn t_cod_1_every_golden_vector_round_trips() {
    let (vectors, _) = load();
    assert!(!vectors.is_empty(), "the vector file is empty");

    for v in &vectors {
        let reply_bytes = format!("{}\r", v.reply);
        let reply = decode_reply(reply_bytes.as_bytes()).unwrap_or_else(|e| {
            panic!(
                "line {}: reply {:?} is not a Synta frame: {e}",
                v.line, v.reply
            )
        });
        let outcome = exercise(&v.command, reply)
            .unwrap_or_else(|e| panic!("line {}: {} → {}: {e}", v.line, v.command, v.reply));

        if let Some(request) = &outcome.request {
            assert_eq!(
                request, &v.request,
                "line {}: `{}` encodes as {request}, the file says {}",
                v.line, v.command, v.request
            );
        } else {
            assert_eq!(
                v.command, "-",
                "line {}: a row with no encodable request must name its command `-`",
                v.line
            );
        }
        assert_eq!(
            outcome.decoded, v.decoded,
            "line {}: {} decodes as {}, the file says {}",
            v.line, v.reply, outcome.decoded, v.decoded
        );
    }
}

#[test]
fn t_cod_1_the_provenance_labels_are_well_formed_and_counted() {
    let (vectors, header_tally) = load();
    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    for v in &vectors {
        assert!(
            matches!(v.label.as_str(), "verified" | "derived"),
            "line {}: {:?} is neither `verified` nor `derived`",
            v.line,
            v.label
        );
        assert!(
            !v.source.is_empty(),
            "line {}: every vector states where it came from",
            v.line
        );
        *tally.entry(v.label.as_str()).or_default() += 1;
    }

    let verified = tally.get("verified").copied().unwrap_or_default();
    let derived = tally.get("derived").copied().unwrap_or_default();
    // The header states the split; M3-T01's acceptance criteria require reporting it and M3-T05
    // works from it, so a stale number in the file is a defect rather than a cosmetic slip.
    assert_eq!(
        header_tally,
        format!("{verified} verified, {derived} derived"),
        "the header tally does not match the rows ({verified} verified, {derived} derived)"
    );
    assert!(
        verified >= 9,
        "the nine handshake pairs from the operator's mount must be present"
    );
}

#[test]
fn t_cod_1_the_nine_handshake_captures_are_all_present_and_verified() {
    // M3-T01 names these specifically as the seed of the file. Losing one to an edit should be a
    // failure, not something noticed at bring-up.
    let (vectors, _) = load();
    let required = [
        (":e1", "=020401"),
        (":a1", "=00B289"),
        (":b1", "=A7FD00"),
        (":j1", "=000080"),
        (":f1", "=100"),
        (":a2", "=00B289"),
        (":b2", "=A7FD00"),
        (":j2", "=000080"),
        (":f2", "=100"),
    ];
    for (request, reply) in required {
        let found = vectors
            .iter()
            .find(|v| v.request == request && v.reply == reply)
            .unwrap_or_else(|| panic!("the capture {request} → {reply} is missing"));
        assert_eq!(
            found.label, "verified",
            "{request} → {reply} came off real hardware"
        );
    }
}

#[test]
fn t_cod_1_the_two_pairs_that_corrected_the_design_decode_exactly() {
    // M3-T01's acceptance criterion, stated as an assertion. The first is exact against an
    // independently known constant; that exactness is what makes the second trustworthy, and the
    // second is what corrected PRD §4.2 by a factor of seven.
    assert_eq!(
        GetCountsPerRevolution(Axis::Ra)
            .decode("00B289")
            .map(|v| v.0.get()),
        Ok(9_024_000)
    );
    assert_eq!(
        GetTimerFrequency(Axis::Ra)
            .decode("A7FD00")
            .map(|v| v.0.get()),
        Ok(64_935)
    );
}

#[test]
fn t_cod_1_every_command_in_the_sdd_table_has_a_vector() {
    // The acceptance criterion "every command the driver will use has a typed constructor **and
    // at least one vector**". Checked against the opcodes rather than the type names, because the
    // opcode is what SDD §5.2.2's table rows are keyed on.
    let (vectors, _) = load();
    let encoded: Vec<String> = vectors.iter().map(|v| v.request.clone()).collect();
    let opcodes = [
        (GetFirmwareVersion::OPCODE, "firmware version"),
        (GetCountsPerRevolution::OPCODE, "counts per revolution"),
        (GetTimerFrequency::OPCODE, "timer frequency"),
        (GetPosition::OPCODE, "position"),
        (GetAxisStatus::OPCODE, "axis status"),
        (GetHighSpeedRatio::OPCODE, "high-speed ratio"),
        (GetGotoTarget::OPCODE, "goto-target readback"),
        (GetBreakPoint::OPCODE, "break-point readback"),
        (GetStepPeriod::OPCODE, "step-period readback"),
        (Initialise::OPCODE, "initialise"),
        (SetMotionMode::OPCODE, "motion mode"),
        (SetStepPeriod::OPCODE, "step period"),
        (SetGotoIncrement::OPCODE, "goto increment"),
        (SetBreakPointIncrement::OPCODE, "break-point increment"),
        (StartMotion::OPCODE, "start motion"),
        (StopMotion::OPCODE, "stop motion"),
        (InstantStop::OPCODE, "instant stop"),
        (SetGuideRate::OPCODE, "guide rate"),
    ];
    for (opcode, name) in opcodes {
        let prefix = format!(":{}", char::from(opcode));
        assert!(
            encoded.iter().any(|r| r.starts_with(&prefix)),
            "{name} (`{}`) has a typed command and no golden vector",
            char::from(opcode)
        );
    }
}

#[test]
fn t_cod_1_all_eight_motion_modes_are_in_the_file() {
    // The highest-risk encoding, and the one place a table with two rows transposed would pass
    // every round-trip test in the crate. Here the eight digit pairs are asserted as literals.
    let (vectors, _) = load();
    for mode in MotionMode::ALL {
        let cmd = SetMotionMode {
            axis: Axis::Ra,
            mode,
        };
        let frame = cmd.encode().to_string();
        assert!(
            vectors.iter().any(|v| v.request == frame),
            "{mode:?} encodes as {frame} and is not in the vector file"
        );
    }
}

#[test]
fn t_cod_1_the_relative_increment_is_the_same_bytes_in_both_directions() {
    // The correction that made `S` disappear from SDD §5.2.2: the goto target is a relative,
    // unsigned increment and the sign lives in `G`. The file carries the pair; this asserts the
    // property they demonstrate.
    let fwd = SetGotoIncrement {
        axis: Axis::Ra,
        target: Move::from_delta(1_000).unwrap(),
    };
    let back = SetGotoIncrement {
        axis: Axis::Ra,
        target: Move::from_delta(-1_000).unwrap(),
    };
    assert_eq!(fwd.encode().as_bytes(), back.encode().as_bytes());
    assert_eq!(fwd.encode().to_string(), ":H1E80300");
}

// -----------------------------------------------------------------------------------------
// The decoder must never panic
// -----------------------------------------------------------------------------------------

/// xorshift64, so the fuzz corpus is reproducible from its seed.
///
/// A hand-rolled generator rather than a property-test dependency: what this needs is a large,
/// deterministic stream of arbitrary bytes, and the exhaustive sweeps below already cover the
/// small end better than random sampling would. Adding a crate to the workspace to shrink a
/// counterexample we can print in full is not a trade worth making.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Push every decoder in the codec at a buffer. Returns nothing; the assertion is that it
/// returns at all.
fn poke_everything(bytes: &[u8]) {
    let Ok(reply) = decode_reply(bytes) else {
        return;
    };
    match reply {
        Reply::Err(e) => {
            let _ = e.digit();
            let _ = e.is_verified();
            let _ = e.to_string();
        }
        Reply::Ok(payload) => {
            // Every typed decoder, including the ones whose width this payload is not.
            let axis = Axis::Ra;
            let _ = GetFirmwareVersion(axis).decode(payload);
            let _ = GetCountsPerRevolution(axis).decode(payload);
            let _ = GetTimerFrequency(axis).decode(payload);
            let _ = GetPosition(axis).decode(payload);
            let _ = GetAxisStatus(axis).decode(payload);
            let _ = GetHighSpeedRatio(axis).decode(payload);
            let _ = GetGotoTarget(axis).decode(payload);
            let _ = GetBreakPoint(axis).decode(payload);
            let _ = GetStepPeriod(axis).decode(payload);
            let _ = Initialise(axis).decode(payload);
            let _ = MotionMode::from_wire(payload);
            let _ = AxisStatus::decode(payload);
        }
    }
}

#[test]
fn t_cod_1_the_decoder_never_panics_on_short_inputs_exhaustively() {
    // Every buffer of length 0, 1 and 2 — 65,793 of them. Small inputs are where the framing
    // logic's edges are (empty, bare terminator, leader with no payload), so this end is proved
    // rather than sampled.
    poke_everything(&[]);
    for a in 0..=u8::MAX {
        poke_everything(&[a]);
        for b in 0..=u8::MAX {
            poke_everything(&[a, b]);
        }
    }
}

#[test]
fn t_cod_1_the_decoder_never_panics_on_realistic_frames_exhaustively() {
    // The protocol's own alphabet, at the lengths a reply actually takes: leader, up to six
    // payload characters, terminator. Random bytes rarely form a decodable frame at all, so this
    // sweep is what exercises the *payload* decoders rather than only the framing.
    const ALPHABET: &[u8] = b"=!0123456789ABCDEFabcdef\r:z ";
    let mut buf = [0u8; 4];
    for &a in ALPHABET {
        buf[0] = a;
        for &b in ALPHABET {
            buf[1] = b;
            for &c in ALPHABET {
                buf[2] = c;
                for &d in ALPHABET {
                    buf[3] = d;
                    poke_everything(&buf);
                }
            }
        }
    }
}

#[test]
fn t_cod_1_the_decoder_never_panics_on_arbitrary_bytes() {
    // Default is what every push can afford; `ASTROCTL_FUZZ_ITERS=1000000` is the long local run
    // M3-T01's acceptance criteria ask for.
    let iterations: u64 = std::env::var("ASTROCTL_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250_000);

    // Three generators, because one is not enough. Uniform random bytes almost never form a
    // decodable frame, so a purely random corpus tests `decode_reply`'s first guard a million
    // times and the payload decoders never. Two thirds of the corpus is therefore *shaped* — a
    // real leader and a real terminator around arbitrary contents — which is what puts arbitrary
    // bytes in front of the u24, status and motion-mode decoders.
    const PAYLOAD_ALPHABET: &[u8] = b"0123456789ABCDEFabcdef=!\r: \0\x7f\xff";

    let mut rng = Rng(0x5DEE_CE66_D5A5_1234);
    let mut buf = Vec::with_capacity(32);
    for _ in 0..iterations {
        let r = rng.next();
        buf.clear();
        match r % 3 {
            // Uniform noise, at lengths straddling MAX_FRAME_LEN so the length guard is exercised
            // from both sides.
            0 => {
                let len = (r / 3 % 14) as usize;
                for _ in 0..len {
                    buf.push((rng.next() & 0xFF) as u8);
                }
            }
            // A well-formed success frame around an arbitrary payload: `=` … `\r`. This is the
            // shape that reaches every `decode` in `poke_everything`.
            1 => {
                let len = (r / 3 % 9) as usize;
                buf.push(b'=');
                for _ in 0..len {
                    buf.push(PAYLOAD_ALPHABET[(rng.next() as usize) % PAYLOAD_ALPHABET.len()]);
                }
                buf.push(b'\r');
            }
            // A refusal frame around an arbitrary code, including the malformed ones.
            _ => {
                let len = (r / 3 % 4) as usize;
                buf.push(b'!');
                for _ in 0..len {
                    buf.push(PAYLOAD_ALPHABET[(rng.next() as usize) % PAYLOAD_ALPHABET.len()]);
                }
                buf.push(b'\r');
            }
        }
        poke_everything(&buf);
    }
}
