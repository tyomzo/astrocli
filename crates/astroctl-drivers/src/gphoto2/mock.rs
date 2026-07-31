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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astroctl_core::error::DeviceError;
use astroctl_core::types::{BatteryStatus, DeviceInfo, ImageFormat, StorageInfo};

use super::ops::{
    format_from_token, format_token, CamOps, CamOpsFactory, CfgKey, RawChoices, RawIdentity,
    RawSettings,
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
            has_live_view: true,
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
}

/// The token the mock's body uses for a format, for tests that assert a round trip.
pub(crate) fn mock_format_token(format: ImageFormat) -> Option<String> {
    let choices: Vec<String> = FORMAT_CHOICES.iter().map(|s| (*s).to_owned()).collect();
    format_token(format, &choices).map(str::to_owned)
}
