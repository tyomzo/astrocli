//! The cable: the [`Wire`] seam, the one [`exchange`] that runs over it, the write gate that
//! decides what may be transmitted, and the autodetect that finds a port to open (M3-T02).
//!
//! # Why the seam is at the byte level and not at the exchange
//!
//! [`Wire`] has three methods — discard, write, read — and knows nothing about frames, replies,
//! timeouts or retries. Everything protocol-shaped lives in [`exchange`], *above* the seam, which
//! means the mock port and a real fd run the identical loop: the same terminator search, the same
//! overflow verdict, the same write gate, the same pre-emption. A seam drawn one level higher —
//! `async fn exchange(frame) -> Reply` on the trait — would have made the mock a *second
//! implementation* of the logic under test, and a test double that reimplements the thing it is
//! standing in for proves only that two pieces of code agree.
//!
//! # Why the real implementation owns an OS thread
//!
//! `serialport` reads and writes block, and a blocking call on a tokio worker occupies it. The
//! field node sizes its runtime at `min(2, cores-2)` workers (SDD §7), so a 16 ms exchange on a
//! worker is half the node's concurrency for 16 ms, up to 62 times a second — which is T-ISO-1's
//! failure mode exactly.
//!
//! `spawn_blocking` would work here, and that is worth saying plainly because it would *not* work
//! for the camera (SDD §5.3.1): neither of the two reasons that forced a thread there applies. A
//! file descriptor has no thread affinity, so it does not matter which pool thread touches it, and
//! these reads are bounded by [`READ_POLL`] rather than by libgphoto2's willingness to return. It
//! is rejected for a third reason instead: the port would have to live behind a mutex so that a
//! dropped `JoinHandle` could not carry it off, and "one owner, exclusively" (SDD §5.2.4) would
//! become a convention no type checks. A thread that owns the port outright and answers over a
//! channel costs the same and makes the exclusivity structural.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::Instant;

use super::codec::{Frame, MAX_FRAME_LEN};

// -----------------------------------------------------------------------------------------
// Buffers
// -----------------------------------------------------------------------------------------

/// Capacity of one read, and of the reply the exchange accumulates.
///
/// Six bytes more than [`MAX_FRAME_LEN`], and the slack is not headroom for a longer reply —
/// there is no longer reply. It is what lets an over-long one be *recognised*. A buffer sized to
/// the legal maximum cannot tell "ten bytes with the terminator still in flight" from "ten bytes
/// of garbage", and handing the second to a decoder is how a wrong number gets believed.
pub const READ_CAPACITY: usize = MAX_FRAME_LEN + 6;

/// Bytes off the wire, without an allocation.
///
/// The same type carries one read and the accumulated reply, because the two are the same shape
/// and the accumulator is never longer than one over-full read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    buf: [u8; READ_CAPACITY],
    len: u8,
}

impl Chunk {
    /// Nothing read yet.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            buf: [0; READ_CAPACITY],
            len: 0,
        }
    }

    /// Copy in up to [`READ_CAPACITY`] bytes, **truncating** beyond that.
    ///
    /// Truncation is safe here and only here: every caller either wrote a [`Frame`] (ten bytes at
    /// most, checked by the codec) or is accumulating a reply, where an overflow is reported as
    /// [`Exchange::Flood`] rather than silently shortened.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Self {
        let mut chunk = Self::empty();
        chunk.extend(bytes);
        chunk
    }

    /// The bytes, in order.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// How many bytes are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether nothing was read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append what fits; return how many bytes did not.
    fn extend(&mut self, bytes: &[u8]) -> usize {
        let room = READ_CAPACITY - self.len as usize;
        let taken = room.min(bytes.len());
        self.buf[self.len as usize..self.len as usize + taken].copy_from_slice(&bytes[..taken]);
        // `taken <= READ_CAPACITY - len`, so the sum is bounded by READ_CAPACITY (16) and fits.
        #[allow(clippy::cast_possible_truncation)]
        {
            self.len += taken as u8;
        }
        bytes.len() - taken
    }
}

impl fmt::Debug for Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Escaped, so a failing assertion shows `"=000080\r"` rather than a wrapped line.
        write!(f, "Chunk({:?})", String::from_utf8_lossy(self.as_bytes()))
    }
}

// -----------------------------------------------------------------------------------------
// The write gate
// -----------------------------------------------------------------------------------------

/// What a port is permitted to transmit.
///
/// SDD §5.2.2 makes opcode case the safety boundary — lower case inquires, upper case moves
/// things — and asks that harnesses talking to real hardware enforce it "on the raw byte stream
/// rather than by convention", so that a misaligned frame cannot align an action opcode. That is
/// what this is: the check runs on the bytes about to be written, not on [`Frame::is_action`],
/// because the whole point is to catch the frame that is not the frame anyone intended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGate {
    /// Inquiries only. `spikes/skywatcher-heq5/survey.py`'s `SAFE_BYTES`, byte for byte: `:`,
    /// `\r`, `a`–`z`, `0`–`9`, and nothing else. Autodetect probing runs in this mode, so a scan
    /// across unknown devices cannot command one of them to move.
    InquiryOnly,
    /// Anything the codec built. Motion becomes possible; this is the mode a connected driver
    /// runs in.
    Actions,
}

impl WriteGate {
    /// The first byte this gate refuses.
    ///
    /// # Errors
    /// [`Refused`], naming the byte and where it was.
    pub fn admits(self, bytes: &[u8]) -> Result<(), Refused> {
        match self {
            Self::Actions => Ok(()),
            Self::InquiryOnly => bytes
                .iter()
                .enumerate()
                .find(|(_, &b)| {
                    !(matches!(b, b':' | b'\r') || b.is_ascii_lowercase() || b.is_ascii_digit())
                })
                .map_or(Ok(()), |(offset, &byte)| Err(Refused { byte, offset })),
        }
    }
}

/// A byte the write gate would not put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    /// The offending byte.
    pub byte: u8,
    /// Where it was in the frame.
    pub offset: usize,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "byte {:?} at offset {} is not transmissible in inquiry-only mode",
            self.byte as char, self.offset
        )
    }
}

// -----------------------------------------------------------------------------------------
// The wire
// -----------------------------------------------------------------------------------------

/// One end of the cable, as an exchange uses it.
///
/// Three methods, all of them about bytes. Everything that knows what a Synta reply looks like
/// lives in [`exchange`], one level up, so that the mock and a real fd share it.
#[async_trait::async_trait]
pub trait Wire: Send {
    /// Throw away anything the port has buffered.
    ///
    /// Called before every write, which is what `survey.py` did (`tcflush(TCIFLUSH)` at the top
    /// of every exchange) and what makes an abandoned reply harmless: it is discarded by the next
    /// exchange rather than read as the answer to a question nobody asked.
    ///
    /// # Errors
    /// Whatever the port reports.
    async fn discard_input(&mut self) -> io::Result<()>;

    /// Write every byte, then flush.
    ///
    /// # Errors
    /// Whatever the port reports.
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Read whatever has arrived, waiting at most `budget`. An empty [`Chunk`] means nothing came
    /// — that is not an error, it is the ordinary case eight times per exchange.
    ///
    /// # Errors
    /// Whatever the port reports, *except* a timeout, which is an empty chunk.
    async fn read(&mut self, budget: Duration) -> io::Result<Chunk>;
}

/// Opens a [`Wire`]. Called once at start and again after the link drops.
#[async_trait::async_trait]
pub trait WireFactory: Send + Sync + fmt::Debug {
    /// Open the port.
    ///
    /// # Errors
    /// [`DeviceError::Transport`](astroctl_core::error::DeviceError::Transport) if the port
    /// cannot be opened. The message reaches an operator, so it names the path.
    async fn open(&self) -> Result<Box<dyn Wire>, astroctl_core::error::DeviceError>;

    /// What to call this link in a log or an error.
    fn describe(&self) -> String;
}

// -----------------------------------------------------------------------------------------
// The exchange
// -----------------------------------------------------------------------------------------

/// How long a single read may block before the exchange looks around.
///
/// The measured round trip is 14.7–17.2 ms over 2000 samples
/// (`spikes/skywatcher-heq5/FINDINGS.md`), so a healthy exchange costs about eight of these and
/// the polling is invisible against it. What the quantum buys is three things one long blocking
/// read cannot do: notice that the priority lane wants the cable, let the task shut down
/// promptly, and bound how long joining the port thread can take.
pub const READ_POLL: Duration = Duration::from_millis(2);

/// When an exchange must give the cable up.
pub enum Yield<'a> {
    /// Nothing takes the line from this exchange. Priority requests and autodetect probes.
    Never,
    /// Give the line up once `wanted` says the priority lane is waiting **and** the exchange has
    /// been running longer than `after`.
    ///
    /// The rule is one sentence: *an in-flight normal exchange gets one round trip, and after that
    /// the priority lane can take the cable.* That keeps SDD §5.2.4's "the in-flight normal
    /// request completes" true in the case it was written for — a healthy exchange answers in
    /// ≤17.2 ms and finishes normally, every time — while bounding what an emergency stop can wait
    /// for at one round trip rather than at a full request timeout.
    ///
    /// Measuring from the start of the exchange rather than from the moment the stop arrived is
    /// what makes both halves work at once. A grace counted from the *arrival* would have to be a
    /// whole round trip (or it would abandon healthy exchanges), and would therefore let a stop
    /// that arrived one millisecond into a stall wait a round trip *plus* that millisecond —
    /// pushing the worst case past the 20 ms budget it exists to protect.
    When {
        /// Asked after every read. Cheap — an atomic load.
        wanted: &'a (dyn Fn() -> bool + Sync),
        /// How long the exchange may run, measured from the instant its frame was written.
        after: Duration,
    },
}

/// What one trip to the wire produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exchange {
    /// Bytes up to and including the first `\r`. Hand them to
    /// [`decode_reply`](super::codec::decode_reply); they have not been interpreted here.
    Reply(Chunk),
    /// Nothing terminated arrived before the budget expired.
    Silent,
    /// [`READ_CAPACITY`] bytes with no terminator among them. Longer than any Synta reply, so
    /// this is a wedged or mis-baud-rated link rather than a reply to parse.
    Flood {
        /// How many bytes were seen, including the ones that did not fit.
        seen: usize,
    },
    /// The port itself failed — unplugged, revoked, or never opened.
    Failed(String),
    /// Abandoned so the priority lane could take the line. Not a link fault: the cable is fine
    /// and this request simply lost it.
    Yielded,
    /// The write gate would not transmit the frame. Nothing was sent.
    Refused(Refused),
}

/// One request/response exchange: flush, write, read to `\r`.
///
/// Strictly one frame at a time. Two frames in one write provably interleave the replies on real
/// hardware (`=0000=000080`, `spikes/skywatcher-heq5/FINDINGS.md`), which is why SDD §5.2.4
/// forbids pipelining and why nothing here ever has two requests outstanding.
pub async fn exchange(
    wire: &mut dyn Wire,
    frame: Frame,
    gate: WriteGate,
    budget: Duration,
    yield_to: &Yield<'_>,
) -> Exchange {
    let bytes = frame.as_bytes();
    if let Err(refused) = gate.admits(bytes) {
        return Exchange::Refused(refused);
    }
    // Before the write, not after the read: the thing being discarded is a *previous* exchange's
    // late reply, and the moment it must be gone is before this one's answer starts arriving.
    if let Err(error) = wire.discard_input().await {
        return Exchange::Failed(format!("flushing the input buffer failed: {error}"));
    }
    if let Err(error) = wire.write(bytes).await {
        return Exchange::Failed(format!("writing {frame} failed: {error}"));
    }

    // The round trip starts when the bytes leave, not when the request was queued.
    let written = Instant::now();
    let deadline = written + budget;
    let mut reply = Chunk::empty();
    let mut seen = 0usize;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Exchange::Silent;
        }
        let slice = READ_POLL.min(deadline - now);
        match wire.read(slice).await {
            Ok(chunk) => {
                seen += chunk.len();
                let overflow = reply.extend(chunk.as_bytes());
                if let Some(end) = reply
                    .as_bytes()
                    .iter()
                    .position(|&b| b == super::codec::frame::TERMINATOR)
                {
                    // Everything up to and including the terminator. Anything past it is a second
                    // frame's bytes and stays in the buffer for `decode_reply` to refuse — the
                    // codec reports that corruption rather than resynchronising past it.
                    return Exchange::Reply(Chunk::from_slice(&reply.as_bytes()[..=end]));
                }
                if overflow > 0 || reply.len() == READ_CAPACITY {
                    return Exchange::Flood { seen };
                }
            }
            Err(error) => return Exchange::Failed(format!("reading a reply failed: {error}")),
        }

        if let Yield::When { wanted, after } = yield_to {
            if Instant::now() >= written + *after && wanted() {
                return Exchange::Yielded;
            }
        }
    }
}

// -----------------------------------------------------------------------------------------
// Autodetect — the part that needs no port open (MNT-01)
// -----------------------------------------------------------------------------------------

/// Whether an entry in [`KNOWN_ADAPTERS`] has been seen on this project's own hardware.
///
/// Data rather than a comment, for the reason the codec's golden vectors are: a table where
/// "probably right" and "measured" look identical cannot be counted, and this one will grow by
/// operators reporting IDs we cannot test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Seen answering a Synta inquiry on the operator's own rig.
    Verified,
    /// Taken from the Linux kernel's driver ID tables. Plausible and untested here.
    Derived,
}

/// A USB-serial bridge chip this driver will probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adapter {
    /// USB vendor ID.
    pub vid: u16,
    /// USB product ID.
    pub pid: u16,
    /// The chip family, for the log line an operator reads.
    pub family: &'static str,
    /// Whether we have seen this exact ID work.
    pub provenance: Provenance,
}

/// The USB-serial bridges MNT-01 scans for: PL2303, FTDI and CH340.
///
/// **The table is an ordering heuristic, not a safety gate.** The real test is the probe — a port
/// is the mount if and only if it answers `:e1` with a decodable firmware version. What the table
/// buys is that a scan does not write bytes at every tty on the machine, which is why an unknown
/// ID is reported to the operator (with its VID:PID) rather than probed on a hunch.
///
/// Silicon Labs' CP2102 (`10c4:ea60`) is a common EQDIR bridge and is deliberately **not** here:
/// the task scopes the filter to the three families above, and adding a fourth we have never seen
/// answer would put a `derived` entry in the table with nothing behind it. An operator with one
/// gets a message naming their ID, which is the mechanism for adding it.
pub const KNOWN_ADAPTERS: &[Adapter] = &[
    // FTDI. The FT232R is the spike's own Pegasus Astro EQDIR stick, `0403:6001` on `/dev/ttyUSB0`
    // — every measurement in `spikes/skywatcher-heq5/FINDINGS.md` came through this one.
    Adapter {
        vid: 0x0403,
        pid: 0x6001,
        family: "FTDI FT232R",
        provenance: Provenance::Verified,
    },
    Adapter {
        vid: 0x0403,
        pid: 0x6010,
        family: "FTDI FT2232",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x0403,
        pid: 0x6011,
        family: "FTDI FT4232",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x0403,
        pid: 0x6014,
        family: "FTDI FT232H",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x0403,
        pid: 0x6015,
        family: "FTDI FT-X",
        provenance: Provenance::Derived,
    },
    // Prolific. The operator's own adapter is a PL2303; which of the family it reports is not
    // recorded, so all eight of the kernel's `pl2303` IDs are here rather than a guess at one.
    Adapter {
        vid: 0x067b,
        pid: 0x2303,
        family: "Prolific PL2303",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x067b,
        pid: 0x2304,
        family: "Prolific PL2303-TB",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x067b,
        pid: 0x23a3,
        family: "Prolific PL2303-GC",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x067b,
        pid: 0x23b3,
        family: "Prolific PL2303-GB",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x067b,
        pid: 0x23c3,
        family: "Prolific PL2303-GT",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x067b,
        pid: 0x23d3,
        family: "Prolific PL2303-GL",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x067b,
        pid: 0x23e3,
        family: "Prolific PL2303-GE",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x067b,
        pid: 0x23f3,
        family: "Prolific PL2303-GS",
        provenance: Provenance::Derived,
    },
    // WCH. The cheap adapters, and the ones most likely to turn up as a spare in the field.
    Adapter {
        vid: 0x1a86,
        pid: 0x5523,
        family: "WCH CH341",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x1a86,
        pid: 0x7522,
        family: "WCH CH340K",
        provenance: Provenance::Derived,
    },
    Adapter {
        vid: 0x1a86,
        pid: 0x7523,
        family: "WCH CH340",
        provenance: Provenance::Derived,
    },
];

/// The table entry for a VID:PID, if there is one.
#[must_use]
pub fn adapter_for(vid: u16, pid: u16) -> Option<Adapter> {
    KNOWN_ADAPTERS
        .iter()
        .copied()
        .find(|a| a.vid == vid && a.pid == pid)
}

/// A port worth probing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The path to open: the `/dev/serial/by-id/*` name when udev made one, else the device node.
    pub path: PathBuf,
    /// The device node the path resolves to. Two spellings of one port share it, which is what
    /// [`rank`] deduplicates on.
    pub node: PathBuf,
    /// The bridge chip udev reported.
    pub adapter: Adapter,
}

impl Candidate {
    /// Whether [`Self::path`] is the stable by-id name rather than the enumeration-order node.
    #[must_use]
    pub fn is_stable(&self) -> bool {
        self.path != self.node
    }
}

/// Order candidates for probing, and collapse the two spellings of one port into one entry.
///
/// Stable `/dev/serial/by-id/*` names come first, and the reason is not tidiness: `/dev/ttyUSB0`
/// is assigned in enumeration order, so on a node that also has a GPS or a focuser the mount's
/// number changes when something else is plugged in first. Probing the by-id name means the path
/// that ends up in a log is the one that will still mean the same port tomorrow.
#[must_use]
pub fn rank(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut best: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match best.iter_mut().find(|c| c.node == candidate.node) {
            // One physical port, two names: keep the stable one.
            Some(existing) if !existing.is_stable() && candidate.is_stable() => {
                *existing = candidate;
            }
            Some(_) => {}
            None => best.push(candidate),
        }
    }
    // Stable first, then by path, so two runs on one machine probe in the same order and a log
    // from the field can be compared with one from the desk.
    best.sort_by(|a, b| {
        b.is_stable()
            .cmp(&a.is_stable())
            .then_with(|| a.path.cmp(&b.path))
    });
    best
}

/// What a scan of the machine's serial ports found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// Ports whose VID:PID is in [`KNOWN_ADAPTERS`], ranked by [`rank`].
    pub candidates: Vec<Candidate>,
    /// USB serial ports that were seen and not recognised, rendered as
    /// `/dev/ttyACM0 (1234:5678 Some Product)`. These are what an operator needs in order to
    /// report an ID worth adding, so a failed autodetect prints them rather than swallowing them.
    pub unrecognised: Vec<String>,
}

// -----------------------------------------------------------------------------------------
// The real port
// -----------------------------------------------------------------------------------------
//
// Everything below needs `libudev` at build time and is therefore behind the non-default
// `serialport` feature. Note what is *not* down here: the exchange loop, the write gate, the
// terminator search, the adapter table, the ranking. Those are the parts with decisions in them
// and they are compiled, linted and tested by every gate on a machine with no mount and no udev.

/// A real serial port, and the thread that owns its file descriptor.
#[cfg(feature = "serialport")]
mod fd {
    use std::collections::HashMap;
    use std::io;
    use std::io::{Read as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use astroctl_core::error::DeviceError;
    use astroctl_core::types::Axis;
    use tokio::sync::oneshot;

    use super::super::codec::{decode_reply, Command, FirmwareVersion, GetFirmwareVersion};
    use super::{
        adapter_for, exchange, rank, Candidate, Chunk, Exchange, Scan, Wire, WireFactory,
        WriteGate, Yield, READ_CAPACITY, READ_POLL,
    };

    /// The port thread's name, so `top -H` and a tracing span agree about who holds the cable.
    pub const PORT_THREAD_NAME: &str = "astroctl-mount-serial";

    /// One blocking operation and where its answer goes.
    enum Op {
        Discard(oneshot::Sender<io::Result<()>>),
        Write(Chunk, oneshot::Sender<io::Result<()>>),
        Read(Duration, oneshot::Sender<io::Result<Chunk>>),
    }

    fn as_io(error: serialport::Error) -> io::Error {
        io::Error::other(error.to_string())
    }

    /// The port thread's whole life: own the fd, answer one operation, repeat, exit when the
    /// channel closes.
    ///
    /// There is no state here beyond the port, and that is the design rather than an accident —
    /// everything that decides *what* to send runs on the runtime, where it can be tested without
    /// a cable.
    fn run(mut port: Box<dyn serialport::SerialPort>, ops: &mpsc::Receiver<Op>) {
        while let Ok(op) = ops.recv() {
            match op {
                Op::Discard(reply) => {
                    let outcome = port.clear(serialport::ClearBuffer::Input).map_err(as_io);
                    let _ = reply.send(outcome);
                }
                Op::Write(bytes, reply) => {
                    let outcome = port.write_all(bytes.as_bytes()).and_then(|()| port.flush());
                    let _ = reply.send(outcome);
                }
                Op::Read(budget, reply) => {
                    let _ = reply.send(read_once(&mut port, budget));
                }
            }
        }
    }

    /// One bounded read. A timeout is **not** an error: it is the ordinary result about eight
    /// times per exchange, and reporting it as a failure would end every healthy exchange.
    fn read_once(
        port: &mut Box<dyn serialport::SerialPort>,
        budget: Duration,
    ) -> io::Result<Chunk> {
        port.set_timeout(budget).map_err(as_io)?;
        let mut buf = [0u8; READ_CAPACITY];
        match port.read(&mut buf) {
            Ok(read) => Ok(Chunk::from_slice(&buf[..read])),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(Chunk::empty()),
            Err(error) => Err(error),
        }
    }

    /// A [`Wire`] backed by a real file descriptor on a thread of its own.
    pub struct SerialWire {
        /// The only sender. Dropping it is what tells the thread to stop.
        ops: Option<mpsc::Sender<Op>>,
        handle: Option<thread::JoinHandle<()>>,
        path: PathBuf,
    }

    impl std::fmt::Debug for SerialWire {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SerialWire")
                .field("path", &self.path)
                .field("open", &self.ops.is_some())
                .finish()
        }
    }

    impl SerialWire {
        /// Open `path` at `baud`, 8N1, no flow control, and give it to a thread.
        ///
        /// The line settings are the spike's, which is the only configuration ever measured
        /// against this mount: `survey.py` set `CS8 | CREAD | CLOCAL` with every input, output and
        /// local flag cleared — raw 8N1, no modem control, no flow control — at 9600 baud.
        ///
        /// # Errors
        /// Whatever opening the device node reports.
        pub fn open(path: &Path, baud: u32) -> io::Result<Self> {
            let port = serialport::new(path.to_string_lossy().into_owned(), baud)
                .data_bits(serialport::DataBits::Eight)
                .parity(serialport::Parity::None)
                .stop_bits(serialport::StopBits::One)
                .flow_control(serialport::FlowControl::None)
                // Every read is bounded by the exchange loop's quantum; this is only the default
                // for one that somehow is not.
                .timeout(READ_POLL)
                .open()
                .map_err(as_io)?;
            let (ops, receiver) = mpsc::channel();
            let handle = thread::Builder::new()
                .name(PORT_THREAD_NAME.to_owned())
                .spawn(move || run(port, &receiver))?;
            Ok(Self {
                ops: Some(ops),
                handle: Some(handle),
                path: path.to_owned(),
            })
        }

        /// Hand one operation to the thread and await its answer.
        async fn ask<T: Send + 'static>(
            &mut self,
            build: impl FnOnce(oneshot::Sender<io::Result<T>>) -> Op,
        ) -> io::Result<T> {
            let Some(ops) = self.ops.as_ref() else {
                return Err(io::Error::other("the serial port is closed"));
            };
            let (sender, reply) = oneshot::channel();
            ops.send(build(sender))
                .map_err(|_| io::Error::other("the serial port thread has stopped"))?;
            reply
                .await
                .map_err(|_| io::Error::other("the serial port thread dropped a reply"))?
        }
    }

    impl Drop for SerialWire {
        /// Close the port and wait for the thread to notice.
        ///
        /// The join is bounded, unlike the camera's (SDD §5.3.1, which abandons its thread rather
        /// than joining it): this thread is either blocked on `recv`, which fails the instant the
        /// sender above is dropped, or inside one read of at most [`READ_POLL`]. Being able to
        /// wait for the fd to actually be released — so a reconnect can reopen it — is the third
        /// thing the read quantum buys.
        fn drop(&mut self) {
            self.ops = None;
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[async_trait::async_trait]
    impl Wire for SerialWire {
        async fn discard_input(&mut self) -> io::Result<()> {
            self.ask(Op::Discard).await
        }

        async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            // Copied rather than borrowed: the bytes cross a channel to another thread, and a
            // `Frame` is ten bytes, so there is nothing to save by making that a lifetime problem.
            let owned = Chunk::from_slice(bytes);
            self.ask(move |reply| Op::Write(owned, reply)).await
        }

        async fn read(&mut self, budget: Duration) -> io::Result<Chunk> {
            self.ask(move |reply| Op::Read(budget, reply)).await
        }
    }

    /// Opens one fixed device node — what `mount.port` names, or what autodetect chose.
    #[derive(Debug, Clone)]
    pub struct SerialPortFactory {
        path: PathBuf,
        baud: u32,
    }

    impl SerialPortFactory {
        /// A factory for `path` at `baud` (`mount.port`, `mount.baud`).
        #[must_use]
        pub fn new(path: impl Into<PathBuf>, baud: u32) -> Self {
            Self {
                path: path.into(),
                baud,
            }
        }
    }

    #[async_trait::async_trait]
    impl WireFactory for SerialPortFactory {
        async fn open(&self) -> Result<Box<dyn Wire>, DeviceError> {
            let (path, baud) = (self.path.clone(), self.baud);
            // Opening a tty is a syscall and a termios round trip. Short — but blocking, and the
            // field node's runtime has two workers to lose (SDD §7).
            let opened = tokio::task::spawn_blocking(move || SerialWire::open(&path, baud))
                .await
                .map_err(|error| {
                    DeviceError::Transport(format!("the port open task failed: {error}"))
                })?;
            opened
                .map(|wire| Box::new(wire) as Box<dyn Wire>)
                .map_err(|error| {
                    DeviceError::Transport(format!(
                        "could not open {}: {error}",
                        self.path.display()
                    ))
                })
        }

        fn describe(&self) -> String {
            self.path.display().to_string()
        }
    }

    /// Every USB serial port on the machine, split into what is worth probing and what is only
    /// worth reporting. Blocking; call it from `spawn_blocking`.
    ///
    /// # Errors
    /// [`DeviceError::Transport`] if udev enumeration fails.
    pub fn scan() -> Result<Scan, DeviceError> {
        let ports = serialport::available_ports().map_err(|error| {
            DeviceError::Transport(format!("enumerating serial ports failed: {error}"))
        })?;
        let by_id = by_id_index();
        let mut found = Scan::default();
        for port in ports {
            let serialport::SerialPortType::UsbPort(info) = port.port_type else {
                // Not a USB port: a Pi's own `/dev/ttyAMA0`, a virtual console, a pty. The mount
                // is on a USB adapter by definition (PRD §4.2 — an EQDIR stick), and an operator
                // with a genuine RS-232 card can name the node in `mount.port`.
                continue;
            };
            let node = PathBuf::from(&port.port_name);
            if let Some(adapter) = adapter_for(info.vid, info.pid) {
                let path = by_id.get(&node).cloned().unwrap_or_else(|| node.clone());
                found.candidates.push(Candidate {
                    path,
                    node,
                    adapter,
                });
            } else {
                found.unrecognised.push(format!(
                    "{} ({:04x}:{:04x}{})",
                    node.display(),
                    info.vid,
                    info.pid,
                    info.product
                        .as_deref()
                        .map_or_else(String::new, |product| format!(" {product}"))
                ));
            }
        }
        found.candidates = rank(found.candidates);
        found.unrecognised.sort();
        Ok(found)
    }

    /// Map each device node to its `/dev/serial/by-id/*` name, where udev made one.
    ///
    /// Best effort by construction. A container with no `/dev/serial`, or a kernel whose udev
    /// rules did not fire, yields no stable names and the scan falls back to the `/dev/ttyUSB*`
    /// nodes — worse paths, not a failure.
    fn by_id_index() -> HashMap<PathBuf, PathBuf> {
        let Ok(entries) = std::fs::read_dir("/dev/serial/by-id") else {
            return HashMap::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let link = entry.path();
                let node = std::fs::canonicalize(&link).ok()?;
                Some((node, link))
            })
            .collect()
    }

    /// Find the mount (MNT-01): scan, rank, then probe each candidate with the version inquiry.
    ///
    /// **The probe is the test; the VID:PID table only decides what is worth opening.** It runs
    /// behind [`WriteGate::InquiryOnly`], which is `survey.py`'s byte-level rule applied to the
    /// one situation where it matters most — a port whose identity is exactly what is not yet
    /// known. Whatever is at the other end of an unrecognised cable, this cannot tell it to move.
    ///
    /// # Errors
    /// [`DeviceError::Transport`] naming every port that was probed and what it answered, plus
    /// every USB serial port that was skipped and its VID:PID. Both halves are there because the
    /// operator reading it is the one who can either fix the cabling or report an ID to add.
    pub async fn autodetect(baud: u32, probe_budget: Duration) -> Result<PathBuf, DeviceError> {
        let found = tokio::task::spawn_blocking(scan).await.map_err(|error| {
            DeviceError::Transport(format!("the port scan task failed: {error}"))
        })??;

        let mut attempts = Vec::new();
        for candidate in &found.candidates {
            match probe(&candidate.path, baud, probe_budget).await {
                Ok(version) => {
                    tracing::info!(
                        port = %candidate.path.display(),
                        adapter = candidate.adapter.family,
                        firmware = ?version,
                        "mount found by autodetect"
                    );
                    return Ok(candidate.path.clone());
                }
                Err(reason) => attempts.push(format!(
                    "{} [{}]: {reason}",
                    candidate.path.display(),
                    candidate.adapter.family
                )),
            }
        }

        let probed = if attempts.is_empty() {
            "no port carried a recognised USB-serial bridge".to_owned()
        } else {
            format!("probed {}", attempts.join("; "))
        };
        let skipped = if found.unrecognised.is_empty() {
            String::new()
        } else {
            format!(
                "; unrecognised USB serial ports (set `mount.port` to use one): {}",
                found.unrecognised.join(", ")
            )
        };
        Err(DeviceError::Transport(format!(
            "no Sky-Watcher mount found by autodetect: {probed}{skipped}"
        )))
    }

    /// One candidate, one inquiry. Errors are prose because they end up in one list an operator
    /// reads, not in a match arm.
    async fn probe(path: &Path, baud: u32, budget: Duration) -> Result<FirmwareVersion, String> {
        let owned = path.to_owned();
        let mut wire = tokio::task::spawn_blocking(move || SerialWire::open(&owned, baud))
            .await
            .map_err(|error| format!("the open task failed: {error}"))?
            .map_err(|error| format!("could not open it: {error}"))?;

        let inquiry = GetFirmwareVersion(Axis::Ra);
        match exchange(
            &mut wire,
            inquiry.encode(),
            WriteGate::InquiryOnly,
            budget,
            &Yield::Never,
        )
        .await
        {
            Exchange::Reply(chunk) => {
                let reply = decode_reply(chunk.as_bytes()).map_err(|error| {
                    format!("answered something that is not a Synta reply: {error}")
                })?;
                let payload = reply
                    .payload()
                    .map_err(|error| format!("refused the version inquiry: {error}"))?;
                inquiry.decode(payload).map_err(|error| {
                    format!("answered {payload:?}, which is not a version: {error}")
                })
            }
            Exchange::Silent => Err("said nothing".to_owned()),
            Exchange::Flood { seen } => Err(format!(
                "streamed {seen} bytes with no terminator — wrong baud rate?"
            )),
            Exchange::Failed(why) => Err(why),
            // Neither is reachable: a probe passes `Yield::Never`, and the frame is `:e1`.
            Exchange::Yielded | Exchange::Refused(_) => {
                Err("the probe was refused before transmission".to_owned())
            }
        }
    }
}

#[cfg(feature = "serialport")]
pub use fd::{autodetect, scan, SerialPortFactory, SerialWire, PORT_THREAD_NAME};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skywatcher::codec::{
        Command, GetAxisStatus, GetFirmwareVersion, GetGotoTarget, GetPosition, Initialise,
        InstantStop, StartMotion, StopMotion,
    };
    use astroctl_core::types::Axis;

    fn candidate(path: &str, node: &str) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            node: PathBuf::from(node),
            adapter: KNOWN_ADAPTERS[0],
        }
    }

    #[test]
    fn the_inquiry_gate_is_survey_pys_safe_byte_set() {
        // `SAFE_BYTES = set(b":\r" + b"abc...z" + b"0123456789")`, and nothing else.
        for byte in 0u8..=255 {
            let expected =
                matches!(byte, b':' | b'\r') || byte.is_ascii_lowercase() || byte.is_ascii_digit();
            assert_eq!(
                WriteGate::InquiryOnly.admits(&[byte]).is_ok(),
                expected,
                "byte {byte:#04x} ({:?})",
                byte as char
            );
            assert!(WriteGate::Actions.admits(&[byte]).is_ok());
        }
    }

    #[test]
    fn the_gate_refuses_the_raw_stream_not_the_frames_opinion_of_itself() {
        // The point of a byte-level gate: it does not ask the frame whether it is an action, it
        // looks at what is about to leave. Checked against real commands rather than synthesised
        // frames, because the codec is the only thing that can build one.
        let actions = [
            InstantStop(Axis::Ra).encode(),
            StopMotion(Axis::Ra).encode(),
            Initialise(Axis::Dec).encode(),
            StartMotion::unbounded(Axis::Ra).encode(),
        ];
        for frame in actions {
            let refused = WriteGate::InquiryOnly
                .admits(frame.as_bytes())
                .expect_err("an action opcode must not pass the inquiry-only gate");
            assert!(refused.byte.is_ascii_uppercase());
            assert_eq!(refused.offset, 1, "the opcode is the second byte");
            assert!(
                frame.is_action(),
                "and the frame agrees, for what it is worth"
            );
        }
        // ...and every inquiry passes, including the ones the probe uses.
        for frame in [
            GetFirmwareVersion(Axis::Ra).encode(),
            GetPosition(Axis::Dec).encode(),
            GetAxisStatus(Axis::Ra).encode(),
            GetGotoTarget(Axis::Dec).encode(),
        ] {
            assert!(WriteGate::InquiryOnly.admits(frame.as_bytes()).is_ok());
            assert!(!frame.is_action());
        }
    }

    #[test]
    fn a_chunk_truncates_rather_than_growing_and_says_how_much_it_dropped() {
        let mut chunk = Chunk::empty();
        assert!(chunk.is_empty());
        assert_eq!(chunk.extend(b"=000080\r"), 0);
        assert_eq!(chunk.as_bytes(), b"=000080\r");
        assert_eq!(chunk.len(), 8);
        // Sixteen capacity, eight used: eight fit, the rest is reported.
        assert_eq!(chunk.extend(&[b'x'; 12]), 4);
        assert_eq!(chunk.len(), READ_CAPACITY);
        assert_eq!(Chunk::from_slice(&[b'x'; 40]).len(), READ_CAPACITY);
    }

    #[test]
    fn ranking_prefers_the_by_id_name_and_collapses_the_two_spellings() {
        let ranked = rank(vec![
            candidate("/dev/ttyUSB1", "/dev/ttyUSB1"),
            candidate("/dev/ttyUSB0", "/dev/ttyUSB0"),
            candidate("/dev/serial/by-id/usb-Prolific-if00-port0", "/dev/ttyUSB1"),
        ]);
        assert_eq!(ranked.len(), 2, "one entry per physical port");
        assert_eq!(
            ranked[0].path,
            PathBuf::from("/dev/serial/by-id/usb-Prolific-if00-port0")
        );
        assert!(ranked[0].is_stable());
        assert_eq!(ranked[1].path, PathBuf::from("/dev/ttyUSB0"));
        assert!(!ranked[1].is_stable());
    }

    #[test]
    fn the_adapter_table_names_the_three_families_and_marks_what_was_measured() {
        // The spike's own stick, and the only entry that has ever answered a Synta inquiry.
        let ftdi = adapter_for(0x0403, 0x6001).expect("the EQDIR stick's own ID");
        assert_eq!(ftdi.provenance, Provenance::Verified);
        assert_eq!(
            KNOWN_ADAPTERS
                .iter()
                .filter(|a| a.provenance == Provenance::Verified)
                .count(),
            1,
            "exactly one ID has been seen working; the rest are read from kernel tables"
        );
        // MNT-01's three families are all present...
        for (vid, pid) in [(0x0403, 0x6001), (0x067b, 0x2303), (0x1a86, 0x7523)] {
            assert!(adapter_for(vid, pid).is_some(), "{vid:04x}:{pid:04x}");
        }
        // ...and nothing else is, including the CP2102 the doc comment explains away.
        assert_eq!(adapter_for(0x10c4, 0xea60), None);
        assert_eq!(adapter_for(0x0000, 0x0000), None);
    }
}
