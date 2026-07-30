//! The durable field-to-stack transfer queue: persistent job store, sha256 checksums and
//! bounded retry with backoff. SDD §5.10; ADD ADR-05/06; PRD STK-17, ARC-11, REL-06/13, PRF-07.
//!
//! The invariant this crate is built on is that **the frame is already durable locally before the
//! agent ever sees it** (§5.3.2, REL-05). `frame.saved` is published only after the bytes are
//! fsynced and renamed into place, so the agent can fail, restart, or stay offline for a whole
//! night without endangering data. Everything else follows from that: an unreachable stack node is
//! a normal operating state, a crash mid-upload costs one retransmission, and the only irreversible
//! action in the crate — parking a frame in `failed` — is reserved for answers that are about the
//! frame itself.
//!
//! # Shape
//!
//! * [`journal`] — the SQLite queue of §5.10.1: `queued → uploading → acked`, keyed
//!   `(session_id, frame_id)`, referencing frames in place rather than spooling copies.
//! * [`upload`] — the multipart POST of §5.10.2 against the receiving contract of §5.11.1,
//!   including the `HEAD` pre-flight and the classification of every answer.
//! * [`meta`] — the derivations that turn a frame path into a `meta` part, anchored on §5.5's
//!   session layout.
//! * [`agent`] — the drain loop, the capped exponential backoff, and the `transfer.status` /
//!   `transfer.acked` / `alert` events of §4.3.
//!
//! Enqueueing is deliberately *not* here. The `frame.saved` subscription lives in the field binary
//! (`astroctl-field::transfer`) because recovering from a missed event means reconciling against
//! the frame store, and the frame store is the binary's to hold.

pub mod agent;
pub mod journal;
pub mod meta;
pub mod upload;

#[cfg(test)]
mod test_support;

pub use agent::{AgentConfig, Backoff, TransferAgent, TransferQueue};
pub use journal::{Entry, Journal, JournalError, NewEntry, Snapshot, State};
pub use meta::{frame_upload, session_id};
pub use upload::{FrameUpload, Outcome, Preflight, Refusal, RetryReason, Uploader};
