//! A scriptable [`CamOps`] for testing everything above the C library.
//!
//! Test-only (`#[cfg(test)]`), and that is a deliberate limit rather than an oversight: the
//! *simulator* is this crate's shipped stand-in for a camera and it implements the whole `Camera`
//! trait honestly, sky and all. What this mock is for is narrower and lower — proving that a
//! command reached the camera thread, that a budget expired, that a wedge dropped the channel.
//! Shipping it would offer a second, worse simulator to anyone who found it.
//!
//! It records **the OS thread every call ran on**, which is what turns "all blocking gphoto2
//! calls are on the camera thread" from a claim in a design document into an assertion.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_core::types::{BatteryStatus, DeviceInfo, ImageFormat, StorageInfo};

use super::ops::{
    format_from_token, format_token, AbortSignal, CamOps, CamOpsFactory, CfgKey, RawCapture,
    RawChoices, RawFileRef, RawIdentity, RawSettings,
};

/// One call, and where it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallRecord {
    /// Which `CamOps` method.
    pub(crate) op: &'static str,
    /// The OS thread name it ran on — `<unnamed>` if the thread has none, which would itself be
    /// a failure, since the camera thread is always named.
    pub(crate) thread: String,
}

/// The R10's format vocabulary, as the body spells it (M2-T01 read 23 choices; these are the
/// three that matter to `ImageFormat`).
const FORMAT_CHOICES: &[&str] = &["Large Fine JPEG", "RAW", "RAW + Large Fine JPEG"];

/// Shared, scriptable state. The test holds one end, the camera thread's `CamOps` the other.
#[derive(Debug)]
pub(crate) struct MockState {
    /// Every call in the order it happened.
    calls: Mutex<Vec<CallRecord>>,
    /// How long each call blocks the camera thread. The knob that blows a timeout.
    block_for: Mutex<Duration>,
    /// If set, `open` fails with a transport error carrying this text.
    open_failure: Mutex<Option<String>>,
    /// What the body reports, and what a write changes.
    settings: Mutex<RawSettings>,
    /// What the body offers.
    choices: Mutex<RawChoices>,
    /// How many `CamOps` objects have been built. A reconnect after a wedge must build a *fresh*
    /// one — M2-T01 proved the stale handle never recovers — so this counter is the evidence.
    builds: AtomicUsize,
    /// How many have been dropped. Proves the context is released on the camera thread.
    drops: AtomicUsize,
    /// The thread each `CamOps` was dropped on.
    drop_threads: Mutex<Vec<String>>,

    // --- capture, bulb and download ----------------------------------------------------------
    /// What the body hands back from a trigger.
    capture_files: Mutex<Vec<RawFileRef>>,
    /// What the body claims the exposure was, if anything.
    reported_exposure: Mutex<Option<f64>>,
    /// If set, `capture` fails with this rejection.
    capture_failure: Mutex<Option<String>>,
    /// If set, `capture_bulb` refuses before opening the shutter — a body whose mode dial forbids
    /// it, which is the R10's own behaviour with the dial away from Bulb.
    bulb_failure: Mutex<Option<String>>,
    /// If set, `download` fails with this transport error.
    download_failure: Mutex<Option<String>>,
    /// How many downloads succeed before `download_failure` starts applying.
    download_failure_after: AtomicUsize,
    /// Extra time `capture` blocks the thread for, on top of any global block.
    capture_block: Mutex<Duration>,
    /// Extra time `download` blocks the thread for. The knob that blows `download_seconds`.
    download_block: Mutex<Duration>,
    /// Every path `download` was asked to write to, in order.
    download_destinations: Mutex<Vec<PathBuf>>,
    /// Whether every `download` so far found its destination absent — the unlink-first check.
    download_path_was_clear: AtomicBool,
    /// How many bytes each downloaded file claims to be.
    download_size: Mutex<u64>,
    /// How many times the shutter has been opened and closed, and whether it is open now.
    shutter_open: AtomicBool,
    /// Every open/close in order, so a test can prove the shutter was released on the abort path.
    shutter_log: Mutex<Vec<&'static str>>,
    /// Raised by `abort()`, so a test can see the safety-net release happen.
    aborts: AtomicUsize,

    // --- live view -----------------------------------------------------------------------------
    /// How many preview frames have been pulled. The fps evidence, without a camera.
    previews: AtomicUsize,
    /// How many times the body was told to leave live view.
    stop_previews: AtomicUsize,
    /// If set, `preview` fails with this transport error — a camera that left mid-stream.
    preview_failure: Mutex<Option<String>>,
    /// How many bytes each preview frame claims to be. 133 KB on the reference body.
    preview_size: Mutex<usize>,
    /// What the body's abilities say about a preview operation.
    has_live_view: AtomicBool,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            block_for: Mutex::new(Duration::ZERO),
            open_failure: Mutex::new(None),
            settings: Mutex::new(RawSettings {
                iso: "1600".to_owned(),
                shutter: "30".to_owned(),
                aperture: Some("5.6".to_owned()),
                format: "RAW + Large Fine JPEG".to_owned(),
            }),
            choices: Mutex::new(RawChoices {
                isos: ["100", "800", "1600", "3200"]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                shutters: ["1/4000", "30", "bulb"]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
                apertures: ["1.4", "5.6"].iter().map(|s| (*s).to_owned()).collect(),
                formats: FORMAT_CHOICES.iter().map(|s| (*s).to_owned()).collect(),
            }),
            builds: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            drop_threads: Mutex::new(Vec::new()),
            // The R10 with `capturetarget=Internal RAM` reuses this one name (M2-T01).
            capture_files: Mutex::new(vec![RawFileRef {
                folder: "/".to_owned(),
                name: "capt0000.cr3".to_owned(),
            }]),
            reported_exposure: Mutex::new(None),
            capture_failure: Mutex::new(None),
            bulb_failure: Mutex::new(None),
            download_failure: Mutex::new(None),
            download_failure_after: AtomicUsize::new(0),
            capture_block: Mutex::new(Duration::ZERO),
            download_block: Mutex::new(Duration::ZERO),
            download_destinations: Mutex::new(Vec::new()),
            download_path_was_clear: AtomicBool::new(true),
            // Not 32 MB: the tests write this many real bytes, and the size that matters to them
            // is that it is reported faithfully, not that it is large.
            download_size: Mutex::new(2048),
            shutter_open: AtomicBool::new(false),
            shutter_log: Mutex::new(Vec::new()),
            aborts: AtomicUsize::new(0),
            previews: AtomicUsize::new(0),
            stop_previews: AtomicUsize::new(0),
            preview_failure: Mutex::new(None),
            // Small, not the measured 133 KB: these tests count frames and assert bytes are
            // carried through unmodified, and allocating 133 KB several hundred times over a soak
            // test would measure the allocator rather than the driver.
            preview_size: Mutex::new(64),
            has_live_view: AtomicBool::new(true),
        }
    }
}

impl MockState {
    /// A shared state and the factory that builds ops against it.
    pub(crate) fn new() -> (Arc<Self>, Arc<MockFactory>) {
        let state = Arc::new(Self::default());
        let factory = Arc::new(MockFactory {
            state: Arc::clone(&state),
        });
        (state, factory)
    }

    /// Makes every subsequent call block the camera thread for `duration`.
    pub(crate) fn block_calls_for(&self, duration: Duration) {
        *self.block_for.lock().expect("mock state") = duration;
    }

    /// Makes `open` fail with a transport error carrying `message`.
    pub(crate) fn fail_open_with(&self, message: &str) {
        *self.open_failure.lock().expect("mock state") = Some(message.to_owned());
    }

    /// Stops `open` failing.
    pub(crate) fn succeed_open(&self) {
        *self.open_failure.lock().expect("mock state") = None;
    }

    /// Removes every aperture choice — a fully manual lens.
    pub(crate) fn remove_aperture_control(&self) {
        self.choices.lock().expect("mock state").apertures.clear();
    }

    /// Adds a shutter token, standing in for the operator turning the mode dial.
    pub(crate) fn offer_shutter(&self, token: &str) {
        self.choices
            .lock()
            .expect("mock state")
            .shutters
            .push(token.to_owned());
    }

    /// Every call so far.
    pub(crate) fn calls(&self) -> Vec<CallRecord> {
        self.calls.lock().expect("mock state").clone()
    }

    /// The settings the body currently reports — the read-back half of a round trip.
    pub(crate) fn settings(&self) -> RawSettings {
        self.settings.lock().expect("mock state").clone()
    }

    /// How many `CamOps` objects have been built.
    pub(crate) fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }

    /// How many have been dropped, and on which threads.
    pub(crate) fn drop_threads(&self) -> Vec<String> {
        self.drop_threads.lock().expect("mock state").clone()
    }

    // --- capture, bulb and download scripting -------------------------------------------------

    /// Makes the body hand back these files from the next trigger.
    ///
    /// Two of them is the RAW+JPEG case, where the second file reaches the driver as a separate
    /// event rather than from the trigger call itself.
    pub(crate) fn capture_yields(&self, names: &[&str]) {
        *self.capture_files.lock().expect("mock state") = names
            .iter()
            .map(|name| RawFileRef {
                folder: "/".to_owned(),
                name: (*name).to_owned(),
            })
            .collect();
    }

    /// Makes the body volunteer an exposure figure, as the R10 does after a bulb hold.
    pub(crate) fn reports_exposure(&self, seconds: f64) {
        *self.reported_exposure.lock().expect("mock state") = Some(seconds);
    }

    /// Makes `capture` refuse — no card, autofocus failure, a mode dial that forbids it.
    pub(crate) fn fail_capture_with(&self, message: &str) {
        *self.capture_failure.lock().expect("mock state") = Some(message.to_owned());
    }

    /// Makes `capture_bulb` refuse before the shutter opens.
    pub(crate) fn fail_bulb_with(&self, message: &str) {
        *self.bulb_failure.lock().expect("mock state") = Some(message.to_owned());
    }

    /// Makes `download` fail with a transport error.
    pub(crate) fn fail_download_with(&self, message: &str) {
        *self.download_failure.lock().expect("mock state") = Some(message.to_owned());
    }

    /// Lets `after` downloads succeed, then fails every one after that.
    ///
    /// The RAW+JPEG half-failure: the raw lands and the JPEG does not, which is the case where
    /// a driver that only cleaned up its temporaries would leave a complete-looking frame with
    /// half its files.
    pub(crate) fn fail_download_after(&self, after: usize, message: &str) {
        self.download_failure_after.store(after, Ordering::SeqCst);
        *self.download_failure.lock().expect("mock state") = Some(message.to_owned());
    }

    /// Removes `bulb` from the body's shutter list — a body with no bulb mode.
    pub(crate) fn remove_bulb_shutter(&self) {
        self.choices
            .lock()
            .expect("mock state")
            .shutters
            .retain(|token| !token.eq_ignore_ascii_case("bulb"));
    }

    /// Makes the download block the camera thread for `duration` — a slow card, or the wire.
    pub(crate) fn download_takes(&self, duration: Duration) {
        *self.download_block.lock().expect("mock state") = duration;
    }

    /// Makes the trigger block the camera thread for `duration`.
    ///
    /// This is the wedge-shaped silence: a `capture` that never returns is a thread inside a C
    /// call that cannot be interrupted, which is the state the whole wedge protocol exists for.
    pub(crate) fn capture_takes(&self, duration: Duration) {
        *self.capture_block.lock().expect("mock state") = duration;
    }

    /// Every path `download` was asked to write to.
    pub(crate) fn download_destinations(&self) -> Vec<PathBuf> {
        self.download_destinations
            .lock()
            .expect("mock state")
            .clone()
    }

    /// Whether every download found its destination absent — i.e. the unlink happened first.
    pub(crate) fn download_saw_a_clear_path(&self) -> bool {
        self.download_path_was_clear.load(Ordering::SeqCst)
    }

    /// The shutter's open/close history, e.g. `["open", "close"]`.
    pub(crate) fn shutter_log(&self) -> Vec<&'static str> {
        self.shutter_log.lock().expect("mock state").clone()
    }

    /// Whether the shutter is open right now. Must be false after every capture, aborted or not.
    pub(crate) fn shutter_is_open(&self) -> bool {
        self.shutter_open.load(Ordering::SeqCst)
    }

    // --- live view scripting -----------------------------------------------------------------

    /// How many preview frames the body has been asked for.
    pub(crate) fn previews(&self) -> usize {
        self.previews.load(Ordering::SeqCst)
    }

    /// How many times the body was told to leave live view.
    pub(crate) fn stop_previews(&self) -> usize {
        self.stop_previews.load(Ordering::SeqCst)
    }

    /// Makes `preview` fail — the camera left mid-stream, which is how the spike's cable pull
    /// presented itself.
    pub(crate) fn fail_preview_with(&self, message: &str) {
        *self.preview_failure.lock().expect("mock state") = Some(message.to_owned());
    }

    /// Stops `preview` failing — the camera came back.
    pub(crate) fn succeed_preview(&self) {
        *self.preview_failure.lock().expect("mock state") = None;
    }

    /// Makes the body report no preview operation in its abilities — a camera with no live view.
    pub(crate) fn remove_live_view(&self) {
        self.has_live_view.store(false, Ordering::SeqCst);
    }

    /// Records a call and applies the scripted block.
    fn enter(&self, op: &'static str) {
        self.calls.lock().expect("mock state").push(CallRecord {
            op,
            thread: current_thread_name(),
        });
        let block = *self.block_for.lock().expect("mock state");
        if !block.is_zero() {
            // A real blocking sleep, because that is what a libgphoto2 call is: the point of the
            // wedge protocol is that this thread cannot be interrupted, and an `await` here
            // would be a thread that could be.
            std::thread::sleep(block);
        }
    }
}

impl MockState {
    /// What a trigger hands back.
    fn raw_capture(&self) -> RawCapture {
        RawCapture {
            files: self.capture_files.lock().expect("mock state").clone(),
            exposure_seconds: *self.reported_exposure.lock().expect("mock state"),
        }
    }
}

/// The current OS thread's name.
fn current_thread_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_owned()
}

/// Builds [`MockOps`] against a shared [`MockState`].
#[derive(Debug)]
pub(crate) struct MockFactory {
    state: Arc<MockState>,
}

impl CamOpsFactory for MockFactory {
    fn build(&self) -> Result<Box<dyn CamOps>, DeviceError> {
        self.state.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockOps {
            state: Arc::clone(&self.state),
        }))
    }
}

/// A `CamOps` that answers from [`MockState`].
#[derive(Debug)]
pub(crate) struct MockOps {
    state: Arc<MockState>,
}

impl Drop for MockOps {
    fn drop(&mut self) {
        self.state.drops.fetch_add(1, Ordering::SeqCst);
        self.state
            .drop_threads
            .lock()
            .expect("mock state")
            .push(current_thread_name());
    }
}

impl CamOps for MockOps {
    fn open(&mut self) -> Result<RawIdentity, DeviceError> {
        self.state.enter("open");
        if let Some(message) = self.state.open_failure.lock().expect("mock state").clone() {
            return Err(DeviceError::Transport(message));
        }
        Ok(RawIdentity {
            info: DeviceInfo {
                name: "Canon EOS R10".to_owned(),
                model: "Canon EOS R10".to_owned(),
                firmware: Some("1.4.0".to_owned()),
                serial: Some("0123456789".to_owned()),
                protocol: "PTP/USB (libgphoto2)".to_owned(),
            },
            settings: self.state.settings.lock().expect("mock state").clone(),
            choices: self.state.choices.lock().expect("mock state").clone(),
            has_live_view: self.state.has_live_view.load(Ordering::SeqCst),
        })
    }

    fn close(&mut self) -> Result<(), DeviceError> {
        self.state.enter("close");
        Ok(())
    }

    fn read_settings(&mut self) -> Result<RawSettings, DeviceError> {
        self.state.enter("read_settings");
        Ok(self.state.settings.lock().expect("mock state").clone())
    }

    fn read_choices(&mut self) -> Result<RawChoices, DeviceError> {
        self.state.enter("read_choices");
        Ok(self.state.choices.lock().expect("mock state").clone())
    }

    fn write_setting(&mut self, key: CfgKey, value: &str) -> Result<(), DeviceError> {
        self.state.enter("write_setting");
        let mut settings = self.state.settings.lock().expect("mock state");
        match key {
            CfgKey::Iso => settings.iso = value.to_owned(),
            CfgKey::Shutter => settings.shutter = value.to_owned(),
            CfgKey::Aperture => settings.aperture = Some(value.to_owned()),
            CfgKey::ImageFormat => {
                // The body stores the token it was given, and only a token it knows. Rejecting
                // an unknown one here is what a real camera does, and it is what makes a driver
                // that invented a token fail visibly rather than quietly shoot the wrong format.
                if format_from_token(value).is_none() {
                    return Err(DeviceError::Rejected(format!("unknown format `{value}`")));
                }
                settings.format = value.to_owned();
            }
        }
        Ok(())
    }

    fn battery(&mut self) -> Result<BatteryStatus, DeviceError> {
        self.state.enter("battery");
        Ok(BatteryStatus {
            percent: 100,
            charging: false,
        })
    }

    fn storage(&mut self) -> Result<StorageInfo, DeviceError> {
        self.state.enter("storage");
        // The card M2-T01 measured: 127.8 GB total, 69.5 GB free.
        Ok(StorageInfo {
            free_mb: 69_500,
            total_mb: 127_800,
        })
    }

    fn capture(&mut self) -> Result<RawCapture, DeviceError> {
        self.state.enter("capture");
        // A real trigger blocks the thread for the exposure *and*, on this body, the transfer.
        let block = *self.state.capture_block.lock().expect("mock state");
        if !block.is_zero() {
            std::thread::sleep(block);
        }
        if let Some(message) = self
            .state
            .capture_failure
            .lock()
            .expect("mock state")
            .clone()
        {
            return Err(DeviceError::Rejected(message));
        }
        Ok(self.state.raw_capture())
    }

    fn capture_bulb(
        &mut self,
        duration: Duration,
        abort: &AbortSignal,
        since: u64,
        _file_wait: Duration,
    ) -> Result<RawCapture, DeviceError> {
        self.state.enter("capture_bulb");
        if let Some(message) = self.state.bulb_failure.lock().expect("mock state").clone() {
            // Refused before the shutter opens, so there is nothing to release.
            return Err(DeviceError::Rejected(message));
        }

        self.state.shutter_open.store(true, Ordering::SeqCst);
        self.state
            .shutter_log
            .lock()
            .expect("mock state")
            .push("open");

        let aborted = abort.hold(duration, since);

        // Released on every path, exactly as the real backend must.
        self.state.shutter_open.store(false, Ordering::SeqCst);
        self.state
            .shutter_log
            .lock()
            .expect("mock state")
            .push("close");

        if aborted {
            return Err(DeviceError::Aborted(
                "the capture was aborted by the operator".to_owned(),
            ));
        }
        Ok(self.state.raw_capture())
    }

    fn download(&mut self, file: &RawFileRef, destination: &Path) -> Result<u64, DeviceError> {
        self.state.enter("download");

        // The trap, modelled: libgphoto2's `download_to` returns `File exists` rather than
        // truncating (M2-T01 finding 1). A driver that stopped unlinking first would pass every
        // test that used a fresh directory and fail on the first retry after a crash — so the
        // mock refuses too, and records the fact for a test to assert directly.
        if destination.exists() {
            self.state
                .download_path_was_clear
                .store(false, Ordering::SeqCst);
            return Err(DeviceError::Transport(format!(
                "File exists: {}",
                destination.display()
            )));
        }
        self.state
            .download_destinations
            .lock()
            .expect("mock state")
            .push(destination.to_path_buf());

        let block = *self.state.download_block.lock().expect("mock state");
        if !block.is_zero() {
            std::thread::sleep(block);
        }
        if let Some(message) = self
            .state
            .download_failure
            .lock()
            .expect("mock state")
            .clone()
        {
            // `fail_download_after(n, …)` lets the first `n` through, which is how the RAW+JPEG
            // half-failure is scripted.
            let remaining = self.state.download_failure_after.load(Ordering::SeqCst);
            if remaining == 0 {
                return Err(DeviceError::Transport(message));
            }
            self.state
                .download_failure_after
                .store(remaining - 1, Ordering::SeqCst);
        }

        // Real bytes, because the caller fsyncs and renames them and a test that asserts a frame
        // exists should be asserting about a file that does.
        let size = *self.state.download_size.lock().expect("mock state");
        let bytes = vec![0xA5_u8; usize::try_from(size).unwrap_or(0)];
        std::fs::write(destination, &bytes)
            .map_err(|error| DeviceError::Transport(format!("mock download: {error}")))?;
        let _ = file;
        Ok(size)
    }

    fn abort(&mut self) -> Result<(), DeviceError> {
        self.state.enter("abort");
        self.state.aborts.fetch_add(1, Ordering::SeqCst);
        // The safety-net release: if the shutter is somehow still open, close it.
        if self.state.shutter_open.swap(false, Ordering::SeqCst) {
            self.state
                .shutter_log
                .lock()
                .expect("mock state")
                .push("close");
        }
        Ok(())
    }

    fn preview(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.state.enter("preview");
        if let Some(message) = self
            .state
            .preview_failure
            .lock()
            .expect("mock state")
            .clone()
        {
            return Err(DeviceError::Transport(message));
        }
        let n = self.state.previews.fetch_add(1, Ordering::SeqCst);
        let size = *self.state.preview_size.lock().expect("mock state");

        // Real JPEG-shaped bytes with the frame number written into them, so a test can prove
        // that what arrived at the sink is the frame the body produced rather than a repeat of an
        // earlier one — which is the failure a `watch` channel makes easy to miss, since a stalled
        // producer and a working one both leave *a* frame in the slot.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpeg.extend_from_slice(&(n as u32).to_be_bytes());
        jpeg.resize(size.max(8), 0x5A);
        Ok(jpeg)
    }

    fn stop_preview(&mut self) -> Result<(), DeviceError> {
        self.state.enter("stop_preview");
        self.state.stop_previews.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// The frame number a mock preview carries, for a test asserting frames are fresh.
pub(crate) fn mock_preview_sequence(jpeg: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = jpeg.get(4..8)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// The token the mock's body uses for a format, for tests that assert a round trip.
pub(crate) fn mock_format_token(format: ImageFormat) -> Option<String> {
    let choices: Vec<String> = FORMAT_CHOICES.iter().map(|s| (*s).to_owned()).collect();
    format_token(format, &choices).map(str::to_owned)
}
