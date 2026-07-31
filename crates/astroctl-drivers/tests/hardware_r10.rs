//! The M2-T02 acceptance run that needs a camera on the end of a cable.
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

#![cfg(feature = "libgphoto2")]

use std::sync::Arc;

use astroctl_core::config::{CameraConfig, CameraDriver, CameraTimeouts};
use astroctl_drivers::gphoto2::CanonGPhoto2CameraFactory;
use astroctl_hal::camera::Camera;
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
    let target = available
        .isos
        .iter()
        .find(|iso| **iso != before.iso)
        .expect("the body offers a second ISO to switch to")
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
