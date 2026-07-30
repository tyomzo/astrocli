//! The hardware abstraction layer: the async device traits every driver implements, and the
//! registry that turns a name in `field-node.yaml` into one.
//!
//! This is the extension contract (PRD EXT-01/EXT-02, HAL-01..08) and is semver-stable from
//! Phase 1 on. Everything above the HAL holds `Arc<dyn MountDevice>` and friends and never names
//! a driver type (ADD §5.6 rule 1) — **and neither does this crate**. It defines the shape a
//! driver fills and the registry a driver is handed to; `astroctl-drivers` is not among its
//! dependencies and cannot become one without a cycle. The two binaries are the only place where
//! a concrete driver and this contract meet, which is what makes the rule enforceable rather
//! than aspirational (`scripts/check-deps.sh`).
//!
//! | Module | Contents | Spec |
//! |--------|----------|------|
//! | [`mount`]    | [`MountDevice`](mount::MountDevice) | SDD §5.1 verbatim, PRD §4.1, HAL-02 |
//! | [`camera`]   | [`Camera`](camera::Camera), capture request/result, live-view frames | PRD §4.1, HAL-03 |
//! | [`guide`]    | [`GuideCamera`](guide::GuideCamera), guide frames, binning | PRD §4.1, HAL-04 |
//! | [`stream`]   | [`FrameStream`](stream::FrameStream) — latest-only frame delivery | SDD §5.3.1, §5.8.3 |
//! | [`registry`] | factories, [`DriverRegistry`](registry::DriverRegistry), probing | SDD §5.1, HAL-07/08 |
//!
//! # The contract every device trait shares
//!
//! These rules hold for all three traits and are not repeated on each method. A driver that
//! breaks one of them breaks the specific thing named here.
//!
//! 1. **Shared, never owned.** Every method takes `&self`, because callers hold `Arc<dyn …>`
//!    and several tasks use one device *concurrently by design* — the 1 Hz position poll, an
//!    API handler and the safety watchdog at the same time (SDD §7). Serializing access is the
//!    driver's job, and the shape that does it is an owning task or thread reached through a
//!    channel (SDD §5.2.4, §5.3.1) — not a lock held across an `.await`, which the
//!    `await_holding_lock` lint denies anyway.
//!
//! 2. **Never block the runtime.** No method may block a runtime worker (SDD §2). The field
//!    node runs as few as one worker thread (SDD §7), so a blocking call there is not a
//!    slowdown, it is a fraction of the whole node. A blocking C library gets its own OS thread
//!    for the device's lifetime (the gphoto2 shape, SDD §5.3.1); incidental blocking work goes
//!    to `spawn_blocking`. T-ISO-1 (SDD §9) is the behavioural test for this, and it is a
//!    standing regression guard rather than a one-off measurement.
//!
//! 3. **Cancel-safety: dropping a future never stops hardware.** Any returned future may be
//!    dropped at an await point — a client disconnects, a `select!` arm loses. Dropping means
//!    *nobody is waiting for the answer any more*; it does not mean the command was withdrawn.
//!    A dropped `goto` future leaves the mount slewing (SDD §5.1). Stopping is always an
//!    explicit call: `stop_slew`, `stop_tracking`, `abort_capture`, `emergency_stop`. A driver
//!    must therefore be in a well-defined state at every await point, and must not leave a
//!    device half-configured across one.
//!
//! 4. **`DeviceError` and nothing else.** Every fallible method returns
//!    [`DeviceError`](astroctl_core::error::DeviceError). The variant is a semantic decision,
//!    not a formatting one: SDD §4.2 maps it to an HTTP status and to whether the UI offers the
//!    operator a retry. `Timeout` and `Transport` are retryable; `Protocol` is not, because an
//!    unparseable reply repeats deterministically until the driver or the firmware changes. The
//!    strings inside the variants are read by operators — say what failed, not where in the
//!    code it failed.
//!
//! 5. **Drivers are silent.** A driver knows nothing about the event bus and publishes nothing;
//!    the facade above it polls, observes and publishes (SDD §5.6, task M1-T02). A driver that
//!    emitted events would need a bus in every one of its own tests, and would put the same
//!    state change on the wire twice.
//!
//! 6. **Capabilities are promises.** `capabilities()` is what the rest of the system offers the
//!    operator (HAL-05) — the PWA hides the PEC control on a mount reporting `has_pec: false`.
//!    Anything reported present must work; anything reported absent must answer
//!    [`DeviceError::Unsupported`](astroctl_core::error::DeviceError::Unsupported) rather than
//!    accept the call and do nothing, which from the outside is indistinguishable from working.
//!
//! 7. **`connect`/`disconnect` are idempotent.** Both are operator buttons on a UI that may be
//!    showing stale state, so "already connected" is `Ok(())`. No other method reconnects
//!    implicitly — they return `NotConnected`, and the decision to reconnect belongs to the
//!    layer that can also tell the operator (SDD §5.4).
//!
//! ADD §5.6 is authoritative for the crate layout and the allowed-dependency matrix;
//! `scripts/check-deps.sh` enforces it.

pub mod camera;
pub mod guide;
pub mod mount;
pub mod registry;
pub mod stream;

#[cfg(test)]
mod test_double;
