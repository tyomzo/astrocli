//! The live-view pacing loop — CAM-05, SDD §5.7 source 1.
//!
//! # The loop is here and not on the camera thread
//!
//! SDD §5.3.1 sketches `LiveViewStart`/`LiveViewStop` with the camera thread pushing JPEGs into a
//! watch channel by itself. It cannot, and the reason is the same single queue the rest of the
//! driver is built around: **a thread inside its own preview loop is a thread not reading its
//! command channel.** Capture, settings and abort would all queue behind live view instead of
//! interleaving with it, and a `stop_live_view` could not arrive at all — the one command that
//! needs to reach a thread that is busy previewing.
//!
//! So the pacing lives here, above the channel, and each frame is one ordinary
//! [`CamCmd::Preview`] round trip. The thread stays a queue; live view is just a client of it.
//!
//! # Why it asks through the *gate*, and what that buys
//!
//! Every tick goes through [`CameraLink::request_unless_capturing`], which M2-T03 added and which
//! this loop is the second and most important caller of. That one choice is what makes the
//! capture pause and the wedge detector coexist:
//!
//! * **During a capture** the gate answers `Busy` *before sending*, under the sender lock. No
//!   command is queued, no budget timer starts, and therefore **a paused live view can never
//!   wedge the camera**. The loop skips the tick. That is SDD §5.7's "expected gap" implemented
//!   as an absence of work rather than as a special case: there is nothing to treat as normal,
//!   because nothing happened.
//! * **When the camera has genuinely stopped answering** there is no capture in flight, so the
//!   preview goes through, occupies the thread, and blows its budget — which is exactly a wedge,
//!   and the recovery loop starts. The detector still fires.
//!
//! Getting this backwards is not hypothetical: a status poll on the ungated path during a bulb
//! exposure is precisely the defect M2-T03 fixed, and live view at 5 fps would have reintroduced
//! it three hundred times a minute. The distinction the two cases need — SDD §5.7's "idle because
//! busy" versus "idle because wedged" — is [`CaptureClaim`](super::thread::CaptureClaim), and it
//! is consulted under the same lock that sends, so the two cannot race.
//!
//! # The sink outlives the link, deliberately
//!
//! Dropping a [`FrameSink`] ends every subscriber's stream permanently, and the field node's
//! forwarding task does not re-subscribe when that happens — it logs "the camera's live-view
//! stream ended" and exits (`astroctl-field/src/liveview.rs`). So a driver that dropped its sink
//! on a wedge would recover the camera perfectly and still leave the operator with a dead
//! preview until they went and pressed *start* again.
//!
//! This loop therefore survives its link. A tick that meets `NotConnected` while the recovery
//! loop is rebuilding is a skipped frame, not an exit; when the camera comes back the frames
//! resume into the *same* sink and the same subscribers. The loop ends for two reasons only:
//! `stop_live_view`, and the driver being dropped.
//!
//! # Pacing down, not up
//!
//! M2-T01 measured 58.5 fps and 133 KB per frame on the R10 — 7.8 MB/s, for a preview whose
//! requirement is 5 fps (PRF-02). USB-11 asks for the opposite of throughput: degrade gracefully
//! on a thin link. So the period comes from `camera.live_view_fps` and the loop sleeps out the
//! remainder of it; the frame cost is subtracted, so a slow frame does not compound into drift.

use std::sync::Arc;
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_hal::camera::LiveViewFrame;
use astroctl_hal::stream::{FrameSink, FrameStream};
use chrono::Utc;

use super::thread::{CamCmd, CameraLink, OpClass};

/// A running live-view session.
///
/// Held by the driver for as long as live view is on. Dropping it stops the loop — see
/// [`Drop`](Self::drop) — which is what makes an aborted `disconnect` safe.
#[derive(Debug)]
pub(crate) struct LiveView {
    /// The producing half. Subscribers are minted from it, so
    /// [`FrameSink::consumers`] counts *watchers* and not the driver itself.
    sink: Arc<FrameSink<LiveViewFrame>>,
    /// The pacing task.
    task: tokio::task::JoinHandle<()>,
}

impl LiveView {
    /// Starts the loop.
    ///
    /// `link` is looked up per tick rather than captured, because the link this loop started with
    /// is *not* the link it will be using after a recovery. Capturing an `Arc<CameraLink>` here
    /// would pin the loop to a dead thread for the rest of the night.
    pub(crate) fn start(links: Arc<dyn LinkSource>, fps: u32) -> Self {
        let (sink, _stream) = FrameStream::channel();
        let sink = Arc::new(sink);
        let task = tokio::spawn(pump(Arc::clone(&sink), links, period(fps)));
        Self { sink, task }
    }

    /// A new cursor on the running stream.
    ///
    /// Two calls are two cursors on one stream, never two sensor loops — the trait requires that
    /// and the camera has one imaging path anyway.
    pub(crate) fn subscribe(&self) -> FrameStream<LiveViewFrame> {
        self.sink.subscribe()
    }
}

impl Drop for LiveView {
    fn drop(&mut self) {
        // Aborting is enough: the task holds only the sink and a link handle, and the sink dying
        // with it is precisely how a driver says "live view stopped" (`FrameStream`'s own docs).
        // Telling the *body* to leave live view is a separate, queued command — see
        // `stop_live_view` — because it needs the camera and this does not.
        self.task.abort();
    }
}

/// Where the pacing loop gets the link it should use *right now*.
///
/// A trait rather than an `Arc<CameraLink>` because of recovery: the link is replaced wholesale
/// when a camera comes back, and a loop holding the old one would go on talking to a thread that
/// has been abandoned. Asking per tick is what lets live view survive a reconnect without the
/// driver having to restart it — which matters because nothing above the driver would.
pub(crate) trait LinkSource: std::fmt::Debug + Send + Sync + 'static {
    /// The live link, or `None` while there is none.
    fn link(&self) -> Option<Arc<CameraLink>>;
}

/// The frame period for a rate.
///
/// Guarded against zero, which config validation already forbids (`live_view_fps` is bounded
/// 1..=60) but which a hand-built `CameraConfig` in a test could still produce — and a period of
/// zero is a loop that pulls USB frames as fast as the bus will go, i.e. exactly the thing this
/// module exists to prevent.
fn period(fps: u32) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.max(1)))
}

/// Pulls frames at `period` and publishes them, for as long as it is allowed to run.
///
/// Never returns on its own. Every failure is a skipped frame: the camera is busy, or is being
/// rebuilt, or is momentarily unhappy, and in all three cases the right thing is to try again
/// next tick rather than to end a stream that consumers cannot restart.
async fn pump(sink: Arc<FrameSink<LiveViewFrame>>, links: Arc<dyn LinkSource>, period: Duration) {
    // Logged once per state rather than per tick: at 5 fps a camera that has been unplugged for a
    // minute is three hundred identical lines, and a log nobody can read is a log nobody reads.
    let mut last_complaint: Option<String> = None;

    loop {
        let started = tokio::time::Instant::now();

        match tick(&sink, &*links).await {
            Ok(()) => last_complaint = None,
            Err(reason) => {
                if last_complaint.as_deref() != Some(reason.as_str()) {
                    tracing::debug!(%reason, "live view is not producing frames");
                    last_complaint = Some(reason);
                }
            }
        }

        // The frame's own cost comes out of the period, so a 390 ms first frame (LV startup,
        // measured) does not push every later frame late by 390 ms as well.
        let spent = started.elapsed();
        tokio::time::sleep(period.saturating_sub(spent)).await;
    }
}

/// One frame, or the reason there was not one.
///
/// The reason is a `String` rather than a `DeviceError` because nothing acts on it — it is a log
/// line and only a log line. Every one of these is expected at some point in a normal night.
async fn tick(sink: &FrameSink<LiveViewFrame>, links: &dyn LinkSource) -> Result<(), String> {
    // Nobody is watching. A frame costs a USB round trip and 133 KB of transfer, and the M1-T09
    // acceptance criterion names the orphaned-work failure directly: a preview generated for no
    // consumer is a transfer per frame for the rest of the night. `FrameStream::consumers` exists
    // for exactly this, and it works here only because the driver holds the sink and never a
    // stream of its own.
    if sink.consumers() == 0 {
        return Err("nobody is watching".to_owned());
    }

    let Some(link) = links.link() else {
        return Err("the camera link is down".to_owned());
    };

    // **The gate, and the whole reason this module is safe.** During a capture this returns
    // `Busy` without queueing and without starting a budget timer, so the pause costs a frame and
    // cannot wedge the camera. See the module docs.
    let jpeg = link
        .request_unless_capturing(OpClass::Config, CamCmd::Preview)
        .await
        .map_err(|error| match error {
            // Named rather than folded into the general case because it is the *expected* one and
            // reads differently in a log: the camera is working and doing what was asked.
            DeviceError::Busy(_) => "the camera is exposing".to_owned(),
            other => other.to_string(),
        })?;

    // Timestamped on arrival rather than on publication. `captured_at` is what lets the PWA label
    // a stale preview (SDD §8.3(8)), so it must be when the camera produced the frame, as nearly
    // as this side of the wire can know it.
    sink.publish(LiveViewFrame::new(jpeg, Utc::now()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::period;

    #[test]
    fn the_frame_period_comes_from_the_configured_rate() {
        // PRF-02's floor and the shipped default.
        assert_eq!(period(5), Duration::from_millis(200));
        assert_eq!(period(1), Duration::from_secs(1));
        // The measured hardware ceiling, which is also the config validator's upper bound.
        assert!(period(60) < Duration::from_millis(20));
    }

    #[test]
    fn a_rate_of_zero_does_not_become_a_busy_loop() {
        // Config validation forbids it, but a `CameraConfig` built by hand in a test does not go
        // through validation — and the failure mode is a loop pulling USB frames as fast as the
        // bus allows, which is the one thing this module exists to prevent.
        assert_eq!(period(0), Duration::from_secs(1));
    }
}
