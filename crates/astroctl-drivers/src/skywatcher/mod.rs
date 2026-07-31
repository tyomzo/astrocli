//! The Sky-Watcher (Synta) mount driver — SDD §5.2.
//!
//! | Submodule | Contents | Task |
//! |-----------|----------|------|
//! | [`codec`] | the wire protocol: framing, hex widths, typed commands, the goto program | M3-T01 |
//! | [`math`] | counters ↔ coordinates: the mech↔sky decomposition, pier side, goto targeting | M3-T03 |
//! | [`controller`] | per-axis motion: handshake parameters, rates and step periods, goto, status | M3-T03 |
//!
//! The serial task (M3-T02), the motor controller (M3-T03) and the `MountDevice`
//! implementation (M3-T04) join it here, in that order, each one layered on the one before
//! per SDD §5.2.1:
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
pub mod controller;
pub mod math;
