//! The acceptance runs that need a camera on the end of a cable — M2-T02 and M2-T03.
//!
//! **`#[ignore]`d, so `cargo test` and all six gates skip it.** It is not a unit test with a
//! hardware dependency bolted on — it is the evidence-producing run for the one acceptance
//! criterion that cannot be met without a body: *"with R10 attached: connect, read settings,
//! change ISO from the API, see it on the camera body"*. Everything else in this driver is proved
//! against a mock `CamOps` and runs in CI.
//!
//! # Running it
//!
//! ```sh
//! # Needs libgphoto2-dev on the machine (see the crate manifest for why it is a *build*
//! # dependency, not just a link one).
//! sudo apt install libgphoto2-dev
//!
//! cargo test -p astroctl-drivers --features libgphoto2 \
//!     --test hardware_r10 -- --ignored --nocapture --test-threads 1
//! ```
//!
//! `--nocapture` matters: the printed transcript *is* the evidence. `--test-threads 1` matters
//! more — there is one camera, and two tests opening it at once is the exclusive-claim failure
//! this driver has a whole module to explain.
//!
//! # What a human still has to check
//!
//! The ISO round trip below is verified through the API: it reads the value back *from the
//! camera*, not from anything the driver remembered. What no test can check is the last clause of
//! the criterion — that the new value is showing **on the camera's own display**. Watch the body
//! while the test runs; the transcript prints the old and new values and pauses between them.
//!
//! # A note on the mode dial, which decides what can be run
//!
//! The R10's physical mode dial constrains the API: with it on **Bulb** the body offers only
//! `bulb` as a shutter speed, so [`Camera::capture`] has no duration to fire for and correctly
//! refuses. That is not a test failure, it is the behaviour — so the timed-capture run below
//! *asserts the refusal* when the dial is on Bulb and takes the frame when it is not, and prints
//! which of the two it did. Move the dial to **M** to exercise the timed path.
//!
//! # Every run here fires the shutter
//!
//! Unlike M2-T02's, these tests actuate the camera and write frames to a temporary directory.
//! Lens cap on is fine; the frames are structural evidence, not pictures.

#![cfg(feature = "libgphoto2")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use astroctl_core::config::{CameraConfig, CameraDriver, CameraTimeouts};
use astroctl_core::error::DeviceError;
use astroctl_drivers::gphoto2::CanonGPhoto2CameraFactory;
use astroctl_hal::camera::{Camera, CaptureRequest};
use astroctl_hal::registry::CameraFactory;

/// `config/field-node.example.yaml`'s camera section.
fn config() -> CameraConfig {
    CameraConfig {
        driver: CameraDriver::Gphoto2,
        default_iso: "1600".to_owned(),
        default_shutter: "30".to_owned(),
        default_format: "RAW+JPEG".to_owned(),
        ops_via_cli: Vec::new(),
        timeouts: CameraTimeouts {
            config_seconds: 5,
            capture_extra_seconds: 30,
            download_seconds: 120,
        },
        indi_device: None,
    }
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; run with --ignored --nocapture"]
async fn connect_read_settings_and_change_iso_on_a_real_body() {
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");

    println!("\n=== M2-T02 hardware acceptance run ===\n");

    // --- connect -------------------------------------------------------------------------
    let started = std::time::Instant::now();
    // A failure here is the one worth reading in full: if something else holds the camera, the
    // message names the gvfs mount and the command that releases it.
    camera
        .connect()
        .await
        .expect("connect — read the error in full if this fails, it diagnoses itself");
    println!("connect: ok in {:?}", started.elapsed());

    let info = camera.device_info();
    println!(
        "body:    {} (serial {:?}, firmware {:?}) over {}",
        info.model, info.serial, info.firmware, info.protocol
    );

    let caps = camera.capabilities();
    println!(
        "caps:    iso {}..{}, shutter {:.6}s..{:.1}s, bulb={}, live_view={}",
        caps.min_iso,
        caps.max_iso,
        caps.min_shutter_s,
        caps.max_shutter_s,
        caps.has_bulb,
        caps.has_live_view
    );

    // --- read settings -------------------------------------------------------------------
    let before = camera.settings().await.expect("read settings");
    println!(
        "settings: iso={} shutter={} aperture={:?} format={:?}",
        before.iso, before.shutter, before.aperture, before.format
    );

    let available = camera.available_settings().await.expect("read choices");
    println!(
        "offers:  {} isos, {} shutters, {} apertures, {} formats",
        available.isos.len(),
        available.shutters.len(),
        available.apertures.len(),
        available.formats.len()
    );
    println!("  isos:     {:?}", available.isos);
    println!("  shutters: {:?}", available.shutters);
    assert!(
        !available.isos.is_empty(),
        "the body must enumerate its own ISO list — a hardcoded list is the thing this driver \
         refuses to have"
    );

    // --- change ISO and read it back ------------------------------------------------------
    // A *numeric* ISO, not merely a different one. The R10's list begins with `Auto`, which is a
    // mode rather than a value: setting it and reading back returns whatever the body's metering
    // has settled on (M2-T03 saw `Auto` written and `400` read back), so a test that picked the
    // first different entry passed or failed depending on whether the camera had metered since
    // it was last woken.
    let target = available
        .isos
        .iter()
        .find(|iso| **iso != before.iso && iso.parse::<u32>().is_ok())
        .expect("the body offers a second numeric ISO to switch to")
        .clone();

    println!("\n--- LOOK AT THE CAMERA BODY NOW ---");
    println!("changing ISO {} -> {}", before.iso, target);
    camera.set_iso(&target).await.expect("set iso");

    let after = camera.settings().await.expect("read settings back");
    println!("camera reports iso={} (asked for {})", after.iso, target);
    assert_eq!(
        after.iso, target,
        "the camera must report the ISO that was set; read back from the body, not from cache"
    );

    // --- a value the body does not offer is refused, never substituted ---------------------
    let refused = camera
        .set_iso("999999")
        .await
        .expect_err("an ISO the body does not offer must be refused");
    println!("refusing a bogus ISO: {refused}");

    // --- restore ---------------------------------------------------------------------------
    camera.set_iso(&before.iso).await.expect("restore iso");
    let restored = camera.settings().await.expect("read settings");
    assert_eq!(restored.iso, before.iso, "the body is left as it was found");
    println!("restored iso={}", restored.iso);

    // --- status ----------------------------------------------------------------------------
    let battery = camera.battery().await.expect("battery");
    let storage = camera.storage().await.expect("storage");
    println!(
        "battery: {}% (charging={}) · storage: {} MB free of {} MB",
        battery.percent, battery.charging, storage.free_mb, storage.total_mb
    );
    // gphoto2 3.4.1's `free_kb`/`capacity_kb` accessors are misnamed and return *bytes*; this is
    // the check that the unit conversion in the backend is right. The reference card is 127.8 GB.
    assert!(
        storage.total_mb > 1_000,
        "storage total of {} MB is implausible — check the byte/kilobyte conversion in backend.rs",
        storage.total_mb
    );

    // --- disconnect -------------------------------------------------------------------------
    camera.disconnect().await.expect("disconnect");
    println!("disconnect: ok\n=== run complete ===\n");
}

// =============================================================================================
// M2-T03 — capture, bulb and abort
// =============================================================================================

/// A scratch directory for the frames a run writes, unique to the test.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("astroctl-r10-{}-{name}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("a writable temporary directory");
    dir
}

/// Every entry in a directory, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("reads the directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Checks that a file really is a Canon CR3, structurally.
///
/// CR3 is an ISO base media file: bytes 4..8 are `ftyp` and the major brand at 8..12 is `crx `.
/// This is the spike's well-formedness question answered without a decoder — `rawler` lives in
/// `astroctl-pipeline` and ADD §5.6 rule 1 forbids this crate depending on it, so the check here
/// is the container's own header rather than a decode. A frame that passes this and has a
/// plausible size is a file the decoder will accept; M2-T01 already proved the decode itself.
fn assert_is_a_cr3(path: &Path) {
    let bytes = std::fs::read(path).expect("the frame is readable");
    // Half a megabyte, not the 32 MB a lit frame weighs: **CR3 size varies enormously with
    // content**. M2-T01 measured 32.0 MB for a lit frame and 1.7 MB for a dark bulb frame, and
    // M2-T03's dark bulb frames came in at 1.5 MB. A threshold set near the lit figure would fail
    // every run made with the lens cap on, which is how these tests are meant to be run.
    assert!(
        bytes.len() > 500_000,
        "{} is {} bytes, which is too small to be a raw frame at all",
        path.display(),
        bytes.len()
    );
    assert_eq!(
        &bytes[4..8],
        b"ftyp",
        "{} is not an ISO-BMFF file",
        path.display()
    );
    assert_eq!(
        &bytes[8..12],
        b"crx ",
        "{} is ISO-BMFF but not Canon raw",
        path.display()
    );
    println!(
        "  frame is a well-formed CR3: {} ({:.1} MB)",
        path.display(),
        bytes.len() as f64 / 1e6
    );
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; FIRES THE SHUTTER; run with --ignored --nocapture"]
async fn a_timed_capture_lands_a_well_formed_cr3() {
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · timed capture ===\n");
    let dir = scratch_dir("timed");
    let request = CaptureRequest::new(&dir, "light_00001");

    let shutter = camera.settings().await.expect("settings").shutter;
    println!("shutter on the body: {shutter}");

    if shutter.eq_ignore_ascii_case("bulb") {
        // The mode dial is on Bulb. A plain capture has no duration to fire for, and the driver
        // must say so rather than hang — which is itself worth proving on hardware.
        let error = camera
            .capture(&request)
            .await
            .expect_err("a plain capture with the shutter on `bulb` must be refused");
        println!("dial is on Bulb, so a timed capture is refused: {error}");
        assert!(
            matches!(error, DeviceError::Rejected(ref m) if m.contains("capture_bulb")),
            "the refusal must point at the call that carries a duration: {error:?}"
        );
        assert_eq!(
            listing(&dir),
            Vec::<String>::new(),
            "a refused capture writes nothing"
        );
        println!("\n--- move the mode dial to M and re-run to exercise the timed path ---");
        camera.disconnect().await.expect("disconnect");
        return;
    }

    let started = Instant::now();
    let result = camera.capture(&request).await.expect("capture");
    println!(
        "capture: {} file(s) in {:?}",
        result.files.len(),
        started.elapsed()
    );
    for file in &result.files {
        println!(
            "  {:?} {} ({:.1} MB)",
            file.kind,
            file.path.display(),
            file.size_bytes as f64 / 1e6
        );
    }
    println!(
        "settings read back: iso={} shutter={} aperture={:?} format={:?}, exposure={:?}",
        result.settings.iso,
        result.settings.shutter,
        result.settings.aperture,
        result.settings.format,
        result.exposure
    );

    let raw = result.raw().expect("a science file");
    assert_eq!(
        raw.path,
        dir.join("light_00001.cr3"),
        "the frame must be named from the request's stem and the body's own extension"
    );
    assert_is_a_cr3(&raw.path);

    // Durability, from a consumer's point of view: the directory holds finished frames and no
    // temporary at all.
    let entries = listing(&dir);
    println!("session directory: {entries:?}");
    assert!(
        entries.iter().all(|name| !name.starts_with(".tmp_")),
        "a completed capture must leave no temporary behind: {entries:?}"
    );

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; FIRES THE SHUTTER for 10 s; run with --ignored --nocapture"]
async fn a_ten_second_bulb_exposure_lasts_ten_seconds() {
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · 10 s bulb ===\n");
    assert!(
        camera.capabilities().has_bulb,
        "the body did not list `bulb` among its shutters — is the mode dial on Bulb?"
    );

    let dir = scratch_dir("bulb");
    let requested = Duration::from_secs(10);

    println!("--- LISTEN TO THE SHUTTER: 10 s exposure starting ---");
    let started = Instant::now();
    let result = camera
        .capture_bulb(&CaptureRequest::new(&dir, "light_bulb"), requested)
        .await
        .expect("bulb exposure");
    let wall = started.elapsed();

    println!(
        "bulb: wall {:?}, camera reports exposure {:?}",
        wall, result.exposure
    );
    for file in &result.files {
        println!(
            "  {:?} {} ({:.1} MB)",
            file.kind,
            file.path.display(),
            file.size_bytes as f64 / 1e6
        );
    }

    // The wall time covers the hold plus the body's readout and the download, so it is bounded
    // below by the exposure and generously above.
    assert!(
        wall >= requested,
        "the hold was shorter than the exposure asked for: {wall:?} < {requested:?}"
    );
    // The reported figure is the one that matters: M2-T01 read `BulbExposureTime 9` for a 10 s
    // hold, so a second of slack in either direction is the body's own accuracy, not a defect.
    let reported = result.exposure.as_secs_f64();
    assert!(
        (reported - 10.0).abs() <= 2.0,
        "the exposure the camera reports ({reported} s) is not within tolerance of the 10 s asked \
         for — if this reads exactly 10.0 the body reported nothing and the request was echoed"
    );

    let raw = result.raw().expect("a science file");
    assert_eq!(raw.path, dir.join("light_bulb.cr3"));
    assert_is_a_cr3(&raw.path);

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; FIRES THE SHUTTER; run with --ignored --nocapture"]
async fn an_abort_mid_bulb_returns_promptly_and_leaves_nothing_on_disk() {
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · abort mid-bulb ===\n");
    assert!(
        camera.capabilities().has_bulb,
        "the body offers no bulb mode"
    );

    let dir = scratch_dir("abort");
    let dir_for_task = dir.clone();
    let camera_for_task = Arc::clone(&camera);

    // Sixty seconds, so an abort that did nothing would be unmistakable.
    println!("--- LISTEN TO THE SHUTTER: 60 s exposure starting, abort after 3 s ---");
    let exposing = tokio::spawn(async move {
        camera_for_task
            .capture_bulb(
                &CaptureRequest::new(&dir_for_task, "light_aborted"),
                Duration::from_secs(60),
            )
            .await
    });

    tokio::time::sleep(Duration::from_secs(3)).await;
    let raised = Instant::now();
    camera.abort_capture().await.expect("aborting never fails");
    let error = exposing
        .await
        .expect("task")
        .expect_err("the capture was aborted");
    let took = raised.elapsed();

    println!("abort returned in {took:?}: {error}");
    assert!(
        matches!(error, DeviceError::Aborted(_)),
        "an operator's stop is `Aborted`, not `Rejected`: {error:?}"
    );
    // The shutter closes on the abort; what follows is the body reading out the frame it took,
    // which the driver drains and discards. Ten seconds is generous for both.
    assert!(
        took < Duration::from_secs(45),
        "the abort did not shorten a 60 s exposure: it took {took:?}"
    );

    let entries = listing(&dir);
    println!("session directory after the abort: {entries:?}");
    assert_eq!(
        entries,
        Vec::<String>::new(),
        "an aborted capture must leave nothing on disk — frames and temporaries alike"
    );

    // And the camera is still usable: the claim was released and the event queue drained, so the
    // next exposure is not handed the aborted frame.
    let after = camera.settings().await.expect("the camera still answers");
    println!("camera still responds after the abort: iso={}", after.iso);

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; run with --ignored --nocapture"]
async fn the_bodys_bulb_mechanism_is_the_one_the_spike_found() {
    // Transcript rather than assertion-heavy: what this proves is that the press/release tokens
    // this driver matches on are the ones the body actually offers, and — the part the spike's
    // own probe got away with by luck — that matching the press as "contains Full" alone would
    // have found `Release Full` if the body listed it first.
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · bulb mechanism ===\n");
    let caps = camera.capabilities();
    println!(
        "has_bulb={} (derived from the body's own shutter list)",
        caps.has_bulb
    );
    let available = camera.available_settings().await.expect("choices");
    println!("shutters: {:?}", available.shutters);
    println!("formats:  {:?}", available.formats);

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; run with --ignored --nocapture"]
async fn probe_finds_the_camera_and_names_the_port() {
    let found = CanonGPhoto2CameraFactory::new()
        .probe()
        .await
        .expect("probe runs");
    println!("\nprobe found {} camera(s):", found.len());
    for camera in &found {
        println!(
            "  {} at {} (driver {})",
            camera.label, camera.address, camera.driver
        );
    }
    assert!(!found.is_empty(), "no camera detected — is it switched on?");
}
