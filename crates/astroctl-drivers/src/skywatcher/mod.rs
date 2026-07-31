//! The Sky-Watcher (Synta) mount driver — SDD §5.2.
//!
//! | Submodule | Contents | Task |
//! |-----------|----------|------|
//! | [`codec`] | the wire protocol: framing, hex widths, typed commands, the goto program | M3-T01 |
//! | [`port`] | the cable: the `Wire` seam, the exchange loop, the write gate, autodetect | M3-T02 |
//! | [`serial`] | the task that owns the cable: two lanes, timeout, retry, heartbeat | M3-T02 |
//! | [`mock_port`] | the scriptable port double the gated tests and M3-T04 run against | M3-T02 |
//!
//! The motor controller (M3-T03) and the `MountDevice` implementation (M3-T04) join them here,
//! in that order, each one layered on the one before per SDD §5.2.1:
//!
//! ```text
//! SkywatcherMount (impl MountDevice)          — coordinates, modes, goto logic
//!     └── MotorController                     — per-axis counts, motion modes, ramping
//!           └── SyntaCodec + SerialTask       — framing, encoding, request/response, lanes
//! ```
//!
//! **The codec is the only place in the workspace that formats or parses a Synta byte.**
//! Everything above it speaks [`codec::Command`] values and typed responses. That is not a
//! stylistic preference: the mount answers a request for a nonexistent axis with plausible
//! data (`:j9` → `=000080`, `spikes/skywatcher-heq5/FINDINGS.md`), so a layer that could build
//! its own frame string could send a corrupted one and never learn of it. Making the bytes
//! unreachable from above is what turns that into a compile error.

pub mod codec;
pub mod mock_port;
pub mod port;
pub mod serial;

pub use serial::{watchdog_channel, Heartbeat, SerialLink, SerialTimings, WatchdogSink};
