//! The acceptance runs that need a camera on the end of a cable — M2-T02, M2-T03 and M2-T04.
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
//!
//! # M2-T04's runs, and the one that bites
//!
//! Live view, battery/storage and the ten-minute soak are harmless. **`t_cam_1_…` is not**: it
//! resets the camera's USB device to induce a real disconnect, and on the reference body that was
//! measured to (a) leave the device enumerated with a dead session, (b) invite gvfs to auto-mount
//! and steal the claim, and (c) on a second run, take the body off the bus until it was physically
//! power-cycled. Read that test's own comment before running it, and prefer the physical cable
//! pull (M2-T05) when there is a human at the desk.

#![cfg(feature = "libgphoto2")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use astroctl_core::config::{CameraConfig, CameraDriver, CameraTimeouts};
use astroctl_core::error::DeviceError;
use astroctl_core::types::ImageFormat;
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
        live_view_fps: 5,
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
#[ignore = "needs a Canon camera on USB; FIRES THE SHUTTER; run with --ignored --nocapture"]
async fn a_raw_plus_jpeg_capture_lands_both_files_under_one_stem() {
    // **The one path the mock cannot honestly stand in for.** The second file of a RAW+JPEG pair
    // does not come back from `capture_image()` — it arrives as a separate `NewFile` event, and
    // whether the body emits it inside the driver's settle window is a fact about the camera, not
    // about the code. A review found the first version of the drain returning on the first file,
    // which would land the raw, leave the JPEG queued, and let the *next* capture download it
    // under the next frame's name. This is that fix, checked against the body that produces it.
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · RAW+JPEG ===\n");
    let before = camera.settings().await.expect("settings");
    if before.shutter.eq_ignore_ascii_case("bulb") {
        println!("dial is on Bulb, so no timed capture can run; skipping");
        println!("--- move the mode dial to M and re-run to exercise RAW+JPEG ---");
        camera.disconnect().await.expect("disconnect");
        return;
    }

    let available = camera.available_settings().await.expect("choices");
    assert!(
        available.formats.contains(&ImageFormat::RawPlusJpeg),
        "the body does not offer RAW+JPEG: {:?}",
        available.formats
    );
    camera
        .set_image_format(ImageFormat::RawPlusJpeg)
        .await
        .expect("select RAW+JPEG");
    println!("format set to RAW+JPEG (was {:?})", before.format);

    let dir = scratch_dir("rawjpeg");
    let started = Instant::now();
    let result = camera
        .capture(&CaptureRequest::new(&dir, "light_pair"))
        .await
        .expect("capture");
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

    let raw = result.raw().expect("a science file");
    let jpeg = result
        .jpeg()
        .expect("the body was set to RAW+JPEG, so the JPEG must have been collected too");
    assert_eq!(raw.path, dir.join("light_pair.cr3"));
    assert_eq!(jpeg.path, dir.join("light_pair.jpg"));
    assert_is_a_cr3(&raw.path);
    // The JPEG is named from the body's own extension, so it must not have been landed as `.cr3`
    // and handed to a raw decoder.
    let jpeg_bytes = std::fs::read(&jpeg.path).expect("the JPEG is readable");
    assert_eq!(
        &jpeg_bytes[..2],
        &[0xFF, 0xD8],
        "{} does not start with a JPEG SOI marker",
        jpeg.path.display()
    );
    println!(
        "  jpeg is a well-formed JPEG ({:.1} MB)",
        jpeg_bytes.len() as f64 / 1e6
    );

    assert_eq!(
        listing(&dir),
        vec!["light_pair.cr3".to_owned(), "light_pair.jpg".to_owned()],
        "both files, one stem, no temporary"
    );

    // The event queue must be empty afterwards, or the *next* capture inherits this frame's
    // JPEG. Proved by taking a second frame and checking it got its own files rather than three.
    let second = camera
        .capture(&CaptureRequest::new(&dir, "light_second"))
        .await
        .expect("second capture");
    println!("second capture: {} file(s)", second.files.len());
    assert_eq!(
        second.files.len(),
        2,
        "the second capture must get exactly its own pair — more means the first frame left an \
         event queued, which is the failure the drain exists to prevent"
    );

    // Leave the body as it was found.
    camera
        .set_image_format(before.format)
        .await
        .expect("restore format");
    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; FIRES THE SHUTTER 10 TIMES; run with --ignored --nocapture"]
async fn ten_consecutive_captures_are_each_complete_and_none_collides() {
    // The acceptance criterion's volume half. One capture working proves the mechanism; ten
    // consecutive ones prove the things that only go wrong the *second* time — a stale temporary
    // from the previous frame, an event left in the queue, a camera-side filename the body reuses
    // (`capt0000.cr3` every time, with `capturetarget=Internal RAM`), a claim never released.
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · ten consecutive captures ===\n");
    if camera
        .settings()
        .await
        .expect("settings")
        .shutter
        .eq_ignore_ascii_case("bulb")
    {
        println!("dial is on Bulb, so no timed capture can run; skipping");
        println!("--- move the mode dial to M and re-run ---");
        camera.disconnect().await.expect("disconnect");
        return;
    }

    const FRAMES: usize = 10;
    let dir = scratch_dir("sequence");
    let mut total_bytes = 0_u64;
    let started = Instant::now();

    for index in 0..FRAMES {
        let stem = format!("light_{index:05}");
        let frame = Instant::now();
        let result = camera
            .capture(&CaptureRequest::new(&dir, &stem))
            .await
            .unwrap_or_else(|error| panic!("capture {index} failed: {error}"));

        let raw = result
            .raw()
            .unwrap_or_else(|| panic!("capture {index} produced no science file"));
        assert_eq!(
            raw.path,
            dir.join(format!("{stem}.cr3")),
            "frame {index} landed under the wrong name — the body reuses one camera-side \
             filename, so a driver that echoed it would overwrite every frame with the next"
        );
        assert_is_a_cr3(&raw.path);
        total_bytes += result.total_bytes();
        println!(
            "  {index:>2}: {} ({:.1} MB) in {:?}",
            raw.path.file_name().expect("a name").to_string_lossy(),
            raw.size_bytes as f64 / 1e6,
            frame.elapsed()
        );
    }

    let elapsed = started.elapsed();
    println!(
        "\n{FRAMES} frames, {:.1} MB total, {:?} ({:.2} s/frame)",
        total_bytes as f64 / 1e6,
        elapsed,
        elapsed.as_secs_f64() / FRAMES as f64
    );

    // Ten frames, ten files, nothing else: no temporary survived and no frame overwrote another.
    let entries = listing(&dir);
    assert_eq!(
        entries.len(),
        FRAMES,
        "expected exactly {FRAMES} frames, found {entries:?}"
    );
    assert!(
        entries.iter().all(|name| !name.starts_with(".tmp_")),
        "a temporary survived the sequence: {entries:?}"
    );
    for index in 0..FRAMES {
        assert!(
            entries.contains(&format!("light_{index:05}.cr3")),
            "frame {index} is missing from {entries:?}"
        );
    }

    // And the camera is still fully usable afterwards — the claim was released every time.
    let battery = camera.battery().await.expect("battery after the sequence");
    println!("battery after the sequence: {}%", battery.percent);

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB with the dial on Bulb; HOLDS THE SHUTTER 30 s THEN 60 s"]
async fn the_thirty_and_sixty_second_bulb_pair() {
    // The acceptance criterion's long half, and the one that actually exercises the budget
    // arithmetic: `OpClass::Capture(d)` is `d + capture_extra_seconds`, so a 60 s exposure is
    // only *not* a wedged camera because the exposure is carried inside the operation class. A
    // fixed budget would have abandoned the thread at 30 s with the shutter open.
    // **The example config's own 30 s allowance**, deliberately. The budget is
    // `2 × exposure + capture_extra_seconds`, and the doubling is what makes the shipped default
    // sufficient for a 60 s bulb frame on a body doing long-exposure noise reduction. An earlier
    // version budgeted `exposure + allowance` and failed a perfectly good 30 s frame; running
    // this against the real default is what stops that regressing quietly.
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · 30 s + 60 s bulb pair ===\n");
    println!(
        "capture_extra_seconds = {} (the example config's own value)",
        config().timeouts.capture_extra_seconds
    );
    let dir = scratch_dir("bulbpair");
    if !bulb_available(&camera, &dir).await {
        camera.disconnect().await.expect("disconnect");
        return;
    }

    for seconds in [30_u64, 60] {
        let requested = Duration::from_secs(seconds);
        let stem = format!("light_bulb_{seconds}s");
        println!("--- {seconds} s exposure starting ---");

        let started = Instant::now();
        let result = camera
            .capture_bulb(&CaptureRequest::new(&dir, &stem), requested)
            .await
            .unwrap_or_else(|error| panic!("the {seconds} s bulb exposure failed: {error}"));
        let wall = started.elapsed();

        let raw = result.raw().expect("a science file");
        println!(
            "  {seconds} s: wall {:?}, camera reports {:?}, {} ({:.1} MB)",
            wall,
            result.exposure,
            raw.path.file_name().expect("a name").to_string_lossy(),
            raw.size_bytes as f64 / 1e6
        );

        assert_eq!(raw.path, dir.join(format!("{stem}.cr3")));
        assert_is_a_cr3(&raw.path);
        assert!(
            wall >= requested,
            "the hold was shorter than asked: {wall:?} < {requested:?}"
        );
        // The body's own figure, which M2-T01 and M2-T03 both saw run one second short of the
        // request. Two seconds of tolerance covers that without accepting an echoed request.
        let reported = result.exposure.as_secs_f64();
        assert!(
            (reported - seconds as f64).abs() <= 2.0,
            "the camera reports {reported} s for a {seconds} s hold, which is outside tolerance"
        );
        assert_eq!(result.settings.shutter, "bulb");
    }

    let entries = listing(&dir);
    println!("\nsession directory: {entries:?}");
    assert_eq!(
        entries,
        vec![
            "light_bulb_30s.cr3".to_owned(),
            "light_bulb_60s.cr3".to_owned()
        ],
        "both frames, no temporary"
    );

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

/// Whether the body currently offers a bulb mechanism, asserting the refusal when it does not.
///
/// The mirror of the timed path's dial check, and it exists for the same reason: **the physical
/// mode dial decides which of the two mechanisms is reachable, and no test can move it.** With the
/// dial on M the R10 enumerates 52 timed shutter speeds and no `bulb`, so `has_bulb` is derived
/// as false and `capture_bulb` must answer `Unsupported` — which is worth proving on hardware
/// rather than skipping past, because it is the capability report and the operation agreeing with
/// each other about a body neither of them was told about.
async fn bulb_available(camera: &Arc<dyn Camera>, dir: &Path) -> bool {
    if camera.capabilities().has_bulb {
        return true;
    }
    let error = camera
        .capture_bulb(
            &CaptureRequest::new(dir, "light_nobulb"),
            Duration::from_secs(1),
        )
        .await
        .expect_err("a body offering no `bulb` shutter cannot take a bulb exposure");
    println!("the body offers no bulb mode (mode dial is off Bulb): {error}");
    assert!(
        matches!(error, DeviceError::Unsupported),
        "a body that cannot do it is `Unsupported`, not a failure: {error:?}"
    );
    assert_eq!(
        listing(dir),
        Vec::<String>::new(),
        "a refused bulb exposure writes nothing"
    );
    println!("--- move the mode dial to Bulb and re-run to exercise the bulb path ---");
    false
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; FIRES THE SHUTTER for 10 s; run with --ignored --nocapture"]
async fn a_ten_second_bulb_exposure_lasts_ten_seconds() {
    let camera: Arc<dyn Camera> = CanonGPhoto2CameraFactory::new()
        .create(&config())
        .expect("this build has the libgphoto2 backend");
    camera.connect().await.expect("connect");

    println!("\n=== M2-T03 · 10 s bulb ===\n");
    let dir = scratch_dir("bulb");
    if !bulb_available(&camera, &dir).await {
        camera.disconnect().await.expect("disconnect");
        return;
    }

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
    let dir = scratch_dir("abort");
    if !bulb_available(&camera, &dir).await {
        camera.disconnect().await.expect("disconnect");
        return;
    }

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

// =============================================================================================
// M2-T04 — live view, battery/storage and wedge recovery
// =============================================================================================

/// The camera as its concrete type, so the run can read the REL-03 link state.
///
/// `create` hands back `Arc<dyn Camera>` and the trait has no link state — deliberately, see
/// `CanonGPhoto2CameraFactory::create_gphoto2`. These runs are the evidence for the recovery
/// protocol, so they need the one accessor the trait does not carry.
fn concrete_camera(config: &CameraConfig) -> Arc<astroctl_drivers::gphoto2::CanonGPhoto2Camera> {
    CanonGPhoto2CameraFactory::new()
        .create_gphoto2(config)
        .expect("this build has the libgphoto2 backend")
}

/// The concrete driver as a trait object, for the helpers that take one.
fn camera_as_dyn(camera: &Arc<astroctl_drivers::gphoto2::CanonGPhoto2Camera>) -> Arc<dyn Camera> {
    Arc::clone(camera) as Arc<dyn Camera>
}

/// This process's resident set size in kilobytes, from `/proc`.
///
/// The soak's memory evidence. `VmRSS` rather than an allocator counter on purpose: the thing most
/// likely to grow here is **not** Rust memory at all — libgphoto2 buffers each preview frame in C
/// (M2-T01 measured 133 KB a frame), and a leak there is invisible to anything inside the process
/// except the kernel's own accounting.
fn resident_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("procfs is mounted");
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
        .expect("VmRSS is reported")
}

/// The camera's USB device node, found the way `lsusb` finds it.
fn camera_device_node() -> Option<PathBuf> {
    for entry in std::fs::read_dir("/sys/bus/usb/devices").ok()? {
        let dir = entry.ok()?.path();
        let vendor = std::fs::read_to_string(dir.join("idVendor")).unwrap_or_default();
        // Canon Inc.
        if vendor.trim() != "04a9" {
            continue;
        }
        let bus: u16 = std::fs::read_to_string(dir.join("busnum"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let address: u16 = std::fs::read_to_string(dir.join("devnum"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        return Some(PathBuf::from(format!("/dev/bus/usb/{bus:03}/{address:03}")));
    }
    None
}

/// Pulls the cable in software: `USBDEVFS_RESET` on the camera's device node.
///
/// **This is a real device-vanished event, not a simulation of one.** The kernel tears the device
/// down and re-enumerates it, so the open `Camera` handle inside libgphoto2 refers to a device
/// that no longer exists — which is precisely the state M2-T01 produced by yanking the cable, and
/// which it measured as unrecoverable without a fresh context.
///
/// It needs write access to the node. On this workstation that is `plugdev` group membership,
/// granted by libgphoto2's own udev rules, so it runs unprivileged. `Err` where it does not, and
/// the caller skips rather than fails — a permissions difference on the runner's machine is not a
/// defect in the driver.
///
/// What it is *not* is a substitute for the physical pull: the connector, the cable and the
/// operator's elbow are all outside this, and that run stays an M2-T05 desk-session item.
fn software_cable_pull() -> Result<PathBuf, String> {
    let node = camera_device_node().ok_or("no Canon device on the USB bus")?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&node)
        .map_err(|error| format!("cannot open {} for writing: {error}", node.display()))?;

    use std::os::fd::AsFd;
    // SAFETY: `USBDEVFS_RESET` is `_IO('U', 20)` — no argument, nothing written back — issued on a
    // usbfs device node, which is the only file it is defined for.
    unsafe {
        rustix::ioctl::ioctl(
            file.as_fd(),
            rustix::ioctl::NoArg::<{ rustix::ioctl::opcode::none(b'U', 20) }>::new(),
        )
    }
    .map_err(|error| format!("USBDEVFS_RESET on {} failed: {error}", node.display()))?;
    Ok(node)
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; run with --ignored --nocapture"]
async fn live_view_streams_at_the_configured_rate_rather_than_the_bodys_maximum() {
    // The measurement that justifies the config knob. M2-T01 found the body sustaining 58.5 fps
    // at 133 KB a frame — 7.8 MB/s — against PRF-02's requirement of *at least* 5. USB-11 asks
    // for graceful degradation on a thin link, so the driver paces down and this proves it does:
    // the rate should track `camera.live_view_fps`, not the hardware's ceiling.
    let config = config();
    let camera = concrete_camera(&config);
    camera.connect().await.expect("connect");

    println!("\n=== M2-T04 · live view at the configured rate ===\n");
    println!(
        "camera.live_view_fps = {} (the shipped default)",
        config.live_view_fps
    );
    println!("M2-T01 measured the body's own ceiling at 58.5 fps, 133 KB/frame");

    let mut stream = camera.live_view_stream().await.expect("live view starts");

    // The first frame is the sensor spinning up — M2-T01 measured 390 ms for it against ~17 ms
    // for the rest — so it is taken and reported outside the rate window rather than dragging the
    // average down.
    let first = Instant::now();
    let frame = stream.next_frame().await.expect("a first frame");
    println!(
        "first frame: {:?}, {} bytes (live-view startup)",
        first.elapsed(),
        frame.jpeg.len()
    );
    assert_eq!(
        &frame.jpeg[..2],
        &[0xFF, 0xD8],
        "a live-view frame must be a JPEG"
    );

    let window = Duration::from_secs(10);
    let started = Instant::now();
    let (mut frames, mut bytes, mut worst) = (0_u32, 0_usize, Duration::ZERO);
    let mut previous = Instant::now();
    while started.elapsed() < window {
        let Some(frame) = stream.next_frame().await else {
            panic!("the stream ended mid-run after {frames} frames");
        };
        worst = worst.max(previous.elapsed());
        previous = Instant::now();
        bytes += frame.jpeg.len();
        frames += 1;
    }
    let seconds = started.elapsed().as_secs_f64();
    let fps = f64::from(frames) / seconds;

    println!(
        "{frames} frames in {seconds:.1} s = {fps:.1} fps, mean {:.0} KB/frame, worst gap {worst:?}",
        bytes as f64 / f64::from(frames) / 1024.0
    );
    println!(
        "PRF-02 needs >= 5 fps on LAN: {}",
        if fps >= 5.0 { "MET" } else { "NOT MET" }
    );

    // The requirement...
    assert!(
        fps >= f64::from(config.live_view_fps) * 0.7,
        "live view ran at {fps:.1} fps against a configured {}",
        config.live_view_fps
    );
    // ...and the *point of the knob*, which is the half a throughput-chasing driver would fail.
    // Anything near the body's 58.5 fps means the pacing is not happening at all.
    assert!(
        fps < f64::from(config.live_view_fps) * 2.0,
        "live view ran at {fps:.1} fps against a configured {} — it is not pacing down, and on a \
         VPN link that is the USB-11 failure",
        config.live_view_fps
    );

    camera.stop_live_view().await.expect("stop live view");
    assert_eq!(
        stream.next_frame().await,
        None,
        "stopping must end the stream for every subscriber"
    );
    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; FIRES THE SHUTTER; run with --ignored --nocapture"]
async fn a_capture_pauses_live_view_and_it_resumes_without_a_reconnect() {
    // SDD §5.7's expected gap, against the real 2.08 s stall rather than a mock's sleep. Three
    // things have to be true at once and only hardware can show all three: the frames stop, the
    // camera is *not* wedged by the live-view ticks that were refused while it was busy, and the
    // stream resumes by itself on the same subscription.
    let camera = concrete_camera(&config());
    camera.connect().await.expect("connect");
    let dir = scratch_dir("liveview-pause");

    println!("\n=== M2-T04 · live view across a real capture ===\n");
    let mut stream = camera.live_view_stream().await.expect("live view starts");
    let _ = stream.next_frame().await.expect("a first frame");

    // Two seconds of frames before, so the "before" rate is measured rather than assumed.
    let before_started = Instant::now();
    let mut before = 0_u32;
    while before_started.elapsed() < Duration::from_secs(2) {
        if stream.next_frame().await.is_some() {
            before += 1;
        }
    }
    println!("before the capture: {before} frames in 2 s");

    let settings = camera.settings().await.expect("settings");
    println!("shutter is `{}`", settings.shutter);

    let capture_started = Instant::now();
    let taken = camera
        .capture(&CaptureRequest::new(&dir, "light_pause"))
        .await;
    let capture_took = capture_started.elapsed();
    match &taken {
        Ok(result) => println!(
            "capture: {:?}, {} file(s)",
            capture_took,
            result.files.len()
        ),
        Err(error) => println!("capture refused after {capture_took:?}: {error}"),
    }

    // **The assertion this run exists for.** Every live-view tick during that exposure met the
    // capture gate and was refused without queueing, so none of them started a budget timer and
    // none of them could wedge the thread. A driver that let live view queue would have abandoned
    // the camera here.
    assert!(
        camera.link_state().is_connected(),
        "live view running across a capture must not wedge the camera; state is {:?}",
        camera.link_state()
    );
    let battery = camera.battery().await.expect("the camera still answers");
    println!(
        "camera still answering after the capture: battery {}%",
        battery.percent
    );

    // And the stream resumes by itself, on the same subscription, with no reconnect.
    let resumed = Instant::now();
    let frame = tokio::time::timeout(Duration::from_secs(10), stream.next_frame())
        .await
        .expect("live view must resume within ten seconds of the exposure ending")
        .expect("the stream must not have ended");
    println!(
        "live view resumed {:?} after the capture returned, {} bytes",
        resumed.elapsed(),
        frame.jpeg.len()
    );

    camera.stop_live_view().await.expect("stop live view");
    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

/// Releases a gvfs camera mount, standing in for the operator.
///
/// **The driver must never do this and does not** — `gphoto2::gvfs` is diagnostic only, because
/// tearing down another session's desktop mount is not a decision a background service gets to
/// make. This is the *test* playing the human who reads the driver's message and runs the command
/// it printed, which is the recovery path REL-03 actually has on a desktop node.
fn release_gvfs_camera_mount() -> Result<String, String> {
    // `XDG_RUNTIME_DIR` is the definition of where gvfs puts its mounts; `/run/user/<uid>` is
    // merely what it is set to everywhere we have looked. The driver's own `gvfs::gvfs_root` makes
    // the same choice, and this run deliberately does not reach into it — a test that used the
    // driver's private scanner would be checking the scanner against itself.
    let gvfs = PathBuf::from(
        std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set".to_owned())?,
    )
    .join("gvfs");
    let entry = std::fs::read_dir(&gvfs)
        .map_err(|error| format!("cannot read {}: {error}", gvfs.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("gphoto2:host="))
        .ok_or_else(|| format!("no camera mount under {}", gvfs.display()))?;
    let host = entry
        .strip_prefix("gphoto2:host=")
        .expect("just matched")
        .to_owned();

    let url = format!("gphoto2://{host}/");
    let output = std::process::Command::new("gio")
        .args(["mount", "-u", &url])
        .output()
        .map_err(|error| format!("could not run gio: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "gio mount -u \"{url}\" failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(format!("gio mount -u \"{url}\""))
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; RESETS THE USB DEVICE — MAY REQUIRE A PHYSICAL POWER CYCLE AFTERWARDS — and FIRES THE SHUTTER; run with --ignored --nocapture"]
async fn t_cam_1_a_device_that_vanishes_mid_liveview_recovers_to_a_working_capture() {
    // **T-CAM-1, as far as it can be met without hands on the cable.**
    //
    // `USBDEVFS_RESET` is the software cable pull. What M2-T04 measured when it ran this is worth
    // reading before running it again, because it is not what the design expected:
    //
    //   * The device **stayed enumerated** and the session behind it died — every transfer
    //     answering `Unspecified error`, which is neither of M2-T01's two measured strings. That
    //     is what `LinkFault::Unresponsive` was added for.
    //   * The re-enumeration triggered a **gvfs auto-mount**, reproducing the exact claim conflict
    //     that broke REL-03 in the spike. The driver named it and printed the release command; the
    //     block below runs that command, as an operator would.
    //   * The **second** reset took the body off the USB bus entirely, and it needed a physical
    //     power cycle to come back. Hence the shouting in the `#[ignore]` label. Budget for that
    //     before running this, and prefer the physical pull (M2-T05) if a human is at the desk.
    //
    // What this run cannot cover either way is the connector itself, which stays an M2-T05 item.
    let camera = concrete_camera(&config());
    camera.connect().await.expect("connect");
    let dir = scratch_dir("t-cam-1");

    println!("\n=== M2-T04 · T-CAM-1 · induced device loss and recovery ===\n");

    // --- a working session first: settings, live view -------------------------------------
    let settings = camera.settings().await.expect("settings");
    println!(
        "settings: iso={} shutter={} format={:?}",
        settings.iso, settings.shutter, settings.format
    );
    let mut stream = camera.live_view_stream().await.expect("live view starts");
    let mut before = 0_u32;
    let warmup = Instant::now();
    while warmup.elapsed() < Duration::from_secs(3) {
        if stream.next_frame().await.is_some() {
            before += 1;
        }
    }
    println!("live view running: {before} frames in 3 s");
    assert!(
        before > 0,
        "live view must be producing before the device is pulled"
    );
    assert!(camera.link_state().is_connected());

    // --- pull the cable, in software -------------------------------------------------------
    println!("\n--- RESETTING THE USB DEVICE (a cable pull without hands) ---");
    let node = match software_cable_pull() {
        Ok(node) => node,
        Err(reason) => {
            // Not a failure of the driver. Skip loudly so the pending list is accurate.
            println!("SKIPPED: {reason}");
            println!("  the recovery path is proved against the mock in the library tests;");
            println!("  re-run as a user in `plugdev`, or do the physical pull (M2-T05).");
            camera.disconnect().await.expect("disconnect");
            return;
        }
    };
    let pulled_at = Instant::now();
    println!("reset {}", node.display());

    // --- recovery ---------------------------------------------------------------------------
    // Nothing below asks the driver to reconnect. The live-view pump meets the dead session on its
    // next tick, the link reports the fault, and the recovery loop does the rest.
    //
    // **With one intervention, and it is the operator's, not the driver's.** Re-enumerating the
    // device is a hotplug event, and on a desktop that is an invitation for gvfs to auto-mount the
    // camera and hold the USB claim — the exact eighty-second failure M2-T01 measured after a
    // replug, reproduced here from the other direction. The driver deliberately never unmounts
    // anything (`gphoto2::gvfs`: "releasing another session's mount is the operator's call"), so
    // when the claim branch fires this run does what the driver's own message tells a human to do
    // and runs the `gio mount -u` it printed. That is the loop being tested: the driver names the
    // thief, the human evicts it, the driver recovers.
    let mut states: Vec<String> = Vec::new();
    let mut watch = camera.watch_link_state();
    let mut released_the_mount = false;
    let deadline = Instant::now() + Duration::from_secs(90);
    let recovered = loop {
        {
            let current = watch.borrow_and_update().clone();
            let rendered = format!("{current:?}");
            if states.last() != Some(&rendered) {
                println!(
                    "  [{:>6.2} s] {rendered}",
                    pulled_at.elapsed().as_secs_f64()
                );
                states.push(rendered);
            }
            if current.is_connected() && states.len() > 1 {
                break true;
            }
            // The claim branch, diagnosed by the driver and acted on by the "operator".
            if !released_the_mount
                && current
                    .message()
                    .is_some_and(|m| m.contains("gvfs") || m.contains("could not be claimed"))
            {
                released_the_mount = true;
                println!("\n  --- the driver says gvfs has the camera; releasing it as an operator would ---");
                match release_gvfs_camera_mount() {
                    Ok(command) => println!("  ran: {command}"),
                    Err(reason) => println!("  could not release it: {reason}"),
                }
                println!();
            }
        }
        if Instant::now() >= deadline {
            break false;
        }
        let _ = tokio::time::timeout(Duration::from_millis(500), watch.changed()).await;
    };

    println!(
        "\nrecovery took {:?}; states seen: {}",
        pulled_at.elapsed(),
        states.join(" -> ")
    );
    assert!(
        recovered,
        "T-CAM-1: the camera did not recover within 30 s. States seen: {states:?}"
    );
    // The transition the acceptance criterion names, in the order it names it.
    assert!(
        states.iter().any(|s| s.starts_with("Reconnecting")),
        "the operator must see a reconnecting state, not a silent gap: {states:?}"
    );
    assert!(
        states.last().is_some_and(|s| s == "Connected"),
        "it must end connected: {states:?}"
    );

    // --- *working capture*, which is what the criterion actually asks for --------------------
    let taken = camera
        .capture(&CaptureRequest::new(&dir, "light_recovered"))
        .await;
    match taken {
        Ok(result) => {
            let raw = result.raw().expect("a science file");
            println!(
                "capture after recovery: {} ({:.1} MB)",
                raw.path.display(),
                raw.size_bytes as f64 / 1e6
            );
            assert_is_a_cr3(&raw.path);
        }
        Err(DeviceError::Rejected(message)) if message.contains("capture_bulb") => {
            // The mode dial is on Bulb, so a timed capture has no duration to fire for. The bulb
            // path is the working-capture evidence instead.
            println!("dial is on Bulb; proving the recovered camera with a 2 s bulb frame");
            let result = camera
                .capture_bulb(
                    &CaptureRequest::new(&dir, "light_recovered"),
                    Duration::from_secs(2),
                )
                .await
                .expect("the recovered camera takes a bulb frame");
            let raw = result.raw().expect("a science file");
            println!(
                "bulb after recovery: {} ({:.1} MB)",
                raw.path.display(),
                raw.size_bytes as f64 / 1e6
            );
            assert_is_a_cr3(&raw.path);
        }
        Err(error) => panic!("the recovered camera could not take a frame: {error}"),
    }
    println!("session directory: {:?}", listing(&dir));

    // --- and live view is still live view ----------------------------------------------------
    // The subscriber from *before* the pull, never re-subscribed. A driver that dropped its sink
    // on a wedge would recover the camera perfectly and leave this stream dead — and the field
    // node's forwarding task does not re-subscribe, so the operator would be looking at a frozen
    // preview with a working camera behind it.
    match tokio::time::timeout(Duration::from_secs(15), stream.next_frame()).await {
        Ok(Some(frame)) => println!(
            "live view resumed into the original subscription: {} bytes",
            frame.jpeg.len()
        ),
        Ok(None) => panic!(
            "the live-view stream ended at the wedge; the field node will not re-subscribe and \
             the operator's preview stays dead"
        ),
        Err(_) => panic!("live view did not resume within 15 s of the camera reconnecting"),
    }

    camera.stop_live_view().await.expect("stop live view");
    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; run with --ignored --nocapture"]
async fn battery_and_storage_for_the_operator_to_check_against_the_body() {
    // The acceptance criterion is *"battery percentage matches camera body display ±5 %"*, and no
    // software can check that: the body's own display is the reference. So this prints the
    // driver's reading in a form an operator can compare at a glance, and asserts only what a
    // machine can — that the figures are plausible and the units are right.
    let camera = concrete_camera(&config());
    camera.connect().await.expect("connect");

    println!("\n=== M2-T04 · battery and storage ===\n");
    let battery = camera.battery().await.expect("battery");
    let storage = camera.storage().await.expect("storage");

    println!("+--------------------------------------------------------+");
    println!(
        "|  DRIVER READS: battery {:>3} %                             |",
        battery.percent
    );
    println!("|  COMPARE WITH THE BODY'S OWN DISPLAY (±5 % is the bar)  |");
    println!("+--------------------------------------------------------+");
    println!(
        "charging (i.e. no batterylevel key, external power): {}",
        battery.charging
    );
    println!(
        "storage: {} MB free of {} MB ({:.1} GB of {:.1} GB)",
        storage.free_mb,
        storage.total_mb,
        storage.free_mb as f64 / 1024.0,
        storage.total_mb as f64 / 1024.0
    );

    assert!(
        battery.percent <= 100,
        "a percentage over 100 is a parse error"
    );
    // gphoto2 3.4.1's `free_kb`/`capacity_kb` are misnamed and return *bytes*. M2-T03 asserted
    // this conversion and it is re-asserted here rather than trusted: getting it wrong by 1024
    // would report a 128 GB card as 128 MB and REL-12's disk thresholds would fire every night.
    assert!(
        storage.total_mb > 1_000,
        "storage total of {} MB is implausible — check the byte/kilobyte conversion in backend.rs",
        storage.total_mb
    );
    assert!(
        storage.free_mb <= storage.total_mb,
        "free ({}) exceeds total ({})",
        storage.free_mb,
        storage.total_mb
    );

    // On-demand is what the two calls above are. The 60 s cadence is the *facade's* ticker
    // (`astroctl-field/src/camera.rs`'s `poll`), and a second reading here proves only that
    // repeated polling is cheap and does not disturb the body.
    let again = Instant::now();
    let second = camera.battery().await.expect("battery again");
    println!(
        "a second on-demand read took {:?} and returned {} %",
        again.elapsed(),
        second.percent
    );

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB; RUNS FOR TEN MINUTES; run with --ignored --nocapture"]
async fn the_ten_minute_live_view_soak() {
    // The acceptance criterion: *"live view runs 10 min without fps decay or memory growth (watch
    // RSS)"*. Both halves need a real body — the mock soak in the library tests proves the
    // plumbing leaks nothing, but only libgphoto2 can leak libgphoto2, and only a real sensor can
    // slow down as it warms.
    let camera = concrete_camera(&config());
    camera.connect().await.expect("connect");

    println!("\n=== M2-T04 · ten-minute live-view soak ===\n");
    let mut stream = camera.live_view_stream().await.expect("live view starts");
    let _ = stream.next_frame().await.expect("a first frame");

    const MINUTES: u64 = 10;
    let baseline_rss = resident_kb();
    println!("minute  frames    fps    RSS (MB)   mean KB/frame");

    let mut per_minute: Vec<(f64, u64)> = Vec::new();
    for minute in 1..=MINUTES {
        let started = Instant::now();
        let (mut frames, mut bytes) = (0_u32, 0_usize);
        while started.elapsed() < Duration::from_secs(60) {
            let Some(frame) = stream.next_frame().await else {
                panic!("the stream ended during minute {minute}");
            };
            bytes += frame.jpeg.len();
            frames += 1;
        }
        let fps = f64::from(frames) / started.elapsed().as_secs_f64();
        let rss = resident_kb();
        per_minute.push((fps, rss));
        println!(
            "{minute:>6}  {frames:>6}  {fps:>5.2}  {:>9.1}   {:>13.0}",
            rss as f64 / 1024.0,
            bytes as f64 / f64::from(frames) / 1024.0
        );
    }

    let (first_fps, _) = per_minute[0];
    let (last_fps, last_rss) = per_minute[MINUTES as usize - 1];
    let growth_kb = last_rss.saturating_sub(baseline_rss);
    println!(
        "\nfps: {first_fps:.2} -> {last_fps:.2}   RSS: {:.1} MB -> {:.1} MB (+{:.1} MB)",
        baseline_rss as f64 / 1024.0,
        last_rss as f64 / 1024.0,
        growth_kb as f64 / 1024.0
    );

    // No decay: the last minute within a tenth of the first. The pacing loop is rate-limited, so
    // the expected answer is "identical"; a real decay would show as the body thermally throttling
    // or as the driver falling behind.
    assert!(
        last_fps > first_fps * 0.9,
        "live view decayed from {first_fps:.2} fps to {last_fps:.2} fps over {MINUTES} minutes"
    );
    // No growth. Ten minutes at 5 fps is three thousand frames of 133 KB passing through
    // libgphoto2 and this driver; anything that held on to even one frame in a hundred would show
    // as four megabytes here. 32 MB of headroom against PRF-05's 512 MB budget.
    assert!(
        growth_kb < 32 * 1024,
        "RSS grew by {growth_kb} KB over {MINUTES} minutes of live view, which is a leak"
    );

    camera.stop_live_view().await.expect("stop live view");
    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}

#[tokio::test]
#[ignore = "needs a Canon camera on USB with the dial on Bulb; HOLDS THE SHUTTER; answers M2-T03's open question"]
async fn an_aborted_bulb_does_not_poison_the_next_one() {
    // **M2-T03's open question, and the reason it was left open.** That task saw every bulb frame
    // fail after one aborted exposure, in a *fresh process*, so the state was on the camera rather
    // than in the driver. An occupied `Internal RAM` buffer was the obvious explanation; adding a
    // camera-side delete did not fix it, and the body then dropped off USB altogether — which
    // makes a fading battery an equally good explanation and leaves the two confounded.
    //
    // The body is healthy again, so the sequence M2-T03 asked for can be run: abort a bulb, then
    // take a 10 s bulb. Whichever way it goes is a result:
    //   * succeeds → the buffer-orphan hypothesis holds and the delete is what fixes it;
    //   * fails    → the buffer is not the cause, and the next suspect is `eosremoterelease`
    //                needing an explicit reset to `None` after an early release.
    let camera = concrete_camera(&config());
    camera.connect().await.expect("connect");
    let dir = scratch_dir("abort-then-bulb");

    println!("\n=== M2-T04 · M2-T03's open question: abort -> bulb ===\n");
    if !bulb_available(&camera_as_dyn(&camera), &dir).await {
        camera.disconnect().await.expect("disconnect");
        println!("SKIPPED: move the mode dial to Bulb and re-run.");
        return;
    }

    // --- the abort -----------------------------------------------------------------------
    println!("--- aborting a bulb exposure three seconds in ---");
    let exposing = {
        let camera = Arc::clone(&camera);
        let dir = dir.clone();
        tokio::spawn(async move {
            camera
                .capture_bulb(
                    &CaptureRequest::new(&dir, "light_aborted"),
                    Duration::from_secs(120),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    let raised = Instant::now();
    camera.abort_capture().await.expect("aborting never fails");
    let outcome = exposing.await.expect("task");
    println!(
        "abort returned in {:?}; the exposure resolved as {:?}",
        raised.elapsed(),
        outcome.as_ref().err()
    );
    assert!(
        matches!(outcome, Err(DeviceError::Aborted(_))),
        "an operator's stop is Aborted: {outcome:?}"
    );
    assert_eq!(
        listing(&dir),
        Vec::<String>::new(),
        "an aborted bulb leaves nothing on disk"
    );

    // --- the question ----------------------------------------------------------------------
    println!("\n--- LISTEN TO THE SHUTTER: 10 s bulb, immediately after the abort ---");
    let started = Instant::now();
    let after = camera
        .capture_bulb(
            &CaptureRequest::new(&dir, "light_after_abort"),
            Duration::from_secs(10),
        )
        .await;
    let wall = started.elapsed();

    match &after {
        Ok(result) => {
            let raw = result.raw().expect("a science file");
            println!(
                "\nANSWER: the bulb SUCCEEDED after an abort — wall {wall:?}, camera reports \
                 {:?}, {} ({:.1} MB)",
                result.exposure,
                raw.path.file_name().expect("a name").to_string_lossy(),
                raw.size_bytes as f64 / 1e6
            );
            println!(
                "  => the buffer-orphan hypothesis holds: discarding the aborted frame from the \
                 body is what makes the next exposure possible. M2-T03's failure was the fading \
                 battery, not a driver defect."
            );
            assert_is_a_cr3(&raw.path);
        }
        Err(error) => {
            println!("\nANSWER: the bulb FAILED after an abort — {error} (after {wall:?})");
            println!(
                "  => the buffer is NOT the cause. Next suspect, per M2-T03: `eosremoterelease` \
                 needs an explicit reset to `None` after an early release."
            );
        }
    }

    camera.disconnect().await.expect("disconnect");
    println!("=== run complete ===\n");
}
