//! The Python worker IPC protocol (ADR-13): versioned JSON messages over stdio, plus the
//! supervision state machine. Stacking-server-internal; never crosses the network, never
//! carries pixel data — frames are passed by filesystem path (SDD §5.12).
//!
//! Two layers, split so they can be depended on separately:
//!
//! | Module         | Feature      | What it is |
//! |----------------|--------------|------------|
//! | [`protocol`]   | always       | message types and the line codec — serde and nothing else |
//! | [`supervisor`] | `supervisor` | spawn, handshake, ping, restart, and the job queue |
//!
//! The split is ADD §5.6 rule 6: the field binary may carry the protocol (rule 5 has both
//! binaries sharing this crate) but must not carry worker process management. With
//! `default-features = false` it links neither `tokio::process` nor a line of spawn logic, so
//! that rule is checkable by building rather than by reading.
//!
//! # Wiring the supervisor (astroctl-stack)
//!
//! ```no_run
//! # use astroctl_core::bus::EventBus;
//! # use astroctl_core::config::StackConfig;
//! # async fn wire(config: &StackConfig, bus: &EventBus) {
//! let workers = astroctl_ipc::supervisor::spawn(&config.workers, bus);
//! # }
//! ```
//!
//! [`supervisor::spawn`] starts no process: SDD §5.12.3 supervises workers as on-demand
//! children, and the first [`supervisor::WorkerHandle::submit`] is what brings one up. Dropping
//! the last handle shuts the worker down.
//!
//! # Both sides of the protocol
//!
//! `workers/astroctl_ipc.py` is the Python mirror of [`protocol`]. The two are held together by
//! `crates/astroctl-ipc/testdata/golden-messages.json`, which both implementations round-trip
//! and which the tests assert they agree on, byte for byte (T-IPC-1). Changing a message shape
//! in one language and not the other fails a test rather than a night of stacking.

pub mod protocol;

#[cfg(feature = "supervisor")]
pub mod supervisor;

pub use protocol::{
    FromWorker, JobId, JobKind, Nonce, ProtocolError, ToWorker, WorkerCaps, WorkerError,
    MAX_LINE_BYTES, PROTO_VERSION,
};
