//! The field-side live-view pipeline — SDD §5.7.
//!
//! Two sources reach one output path (binary frames on `/ws/liveview`, SDD §8.3(5)):
//!
//! 1. **The camera's live-view stream** (CAM-05) — JPEG already, forwarded untouched. Nothing in
//!    this crate is on that path; re-encoding a frame the camera has already compressed would
//!    cost latency and quality for nothing.
//! 2. **The last-captured frame's preview** (CAM-06, IPP-04) — the path this crate *is*:
//!    [`decode`] the stored frame, take the window from its full-resolution samples, reduce to
//!    quarter-res, [`stretch`] with asinh, encode JPEG. [`preview::render_preview`] is the whole
//!    of it in one call.
//!
//! # Everything here is pure
//!
//! Bytes in, bytes out. No file is opened, no session is consulted, no event is published and
//! nothing is spawned. The queue with its depth-1 replace semantics, the blocking pool it runs
//! on, the cache path and the `preview_ready` announcement all live in `astroctl-field`, because
//! they are decisions about a *node* and this is a decision about an *image*. The practical
//! payoff is that every test in this crate builds its input with `vec![]`.
//!
//! # The two things most likely to be got wrong
//!
//! - **FITS rows are bottom-up.** A decoder that forgets renders every preview mirrored in
//!   declination, and it looks completely plausible. [`decode`] carries the guard test.
//! - **The stretch already exists once**, in `workers/compute_worker.py`, and the operator sees
//!   both implementations' output — often of the same exposure, minutes apart. [`stretch`]
//!   documents the correspondence line by line and asserts it.

pub mod decode;
pub mod preview;
pub mod stretch;

pub use decode::{decode, decode_any, DecodeError, DecodedFrame, SourceFormat};
pub use preview::{render_decoded, render_preview, Preview, PreviewParams};
pub use stretch::{Curve, Window};
