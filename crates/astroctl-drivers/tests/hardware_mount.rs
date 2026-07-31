//! **T-HIL-1** — the Sky-Watcher driver against real hardware (M3-T02's half).
//!
//! Every test here needs a physical USB-serial adapter, so the whole file is behind the
//! non-default `serialport` feature *and* `#[ignore]`. `cargo test --workspace` never sees it;
//! that is the point.
//!
//! # Running it
//!
//! ```sh
//! sudo apt install libudev-dev            # the build dependency, see the workspace manifest
//! sudo usermod -aG dialout "$USER"        # then log out and back in
//! cargo test -p astroctl-drivers --features serialport --test hardware_mount -- --ignored --nocapture
//! ```
//!
//! # What is safe to run, and when
//!
//! **Everything in this file is read-only and cannot command motion.** The enumeration test opens
//! nothing at all. The probe opens a port and transmits exactly one frame, `:e1`, behind
//! [`WriteGate::InquiryOnly`] — which refuses every uppercase byte on the raw stream, so a
//! misaligned frame cannot align an action opcode. That is `spikes/skywatcher-heq5/survey.py`'s
//! own rule, applied by the driver rather than by a harness.
//!
//! So the probe is safe with the mount powered and the clutches engaged. Motion belongs to
//! T-HIL-1 steps 3–6 and to M3-T03/M3-T04, not here.
//!
//! # What has actually been run — 2026-07-31
//!
//! `t_hil_1_autodetect_recognises_the_adapter_that_is_plugged_in` — **passed**, against the
//! operator's own Pegasus Astro EQDIR Stick, on a machine with no `dialout` membership. Verbatim:
//!
//! ```text
//! unrecognised USB serial ports: []
//! candidate /dev/serial/by-id/usb-Pegasus_Astro_EQDIR_Stick_PAAD1FQW-if00-port0
//!        -> /dev/ttyUSB0 [FTDI FT232R 0403:6001, Verified] stable=true
//! ```
//!
//! So the discovery half of MNT-01 is evidenced rather than argued: the VID:PID matched the one
//! entry in `KNOWN_ADAPTERS` that was measured rather than read from a kernel table, udev's
//! symlink resolved to the right node, and the stable name won the ranking.
//!
//! `t_hil_1_the_mount_answers_the_version_inquiry` — **not run**, and its failure is itself worth
//! recording because it is the first thing bring-up will hit:
//!
//! ```text
//! no Sky-Watcher mount found by autodetect: probed
//!   /dev/serial/by-id/usb-Pegasus_Astro_EQDIR_Stick_PAAD1FQW-if00-port0 [FTDI FT232R]:
//!   could not open it: Permission denied
//! ```
//!
//! `/dev/ttyUSB0` is `root:dialout` mode `660`, and adding a user to `dialout` needs root — so
//! **the field node's deployment has to grant this and it is not a runtime problem the driver can
//! solve.** The message names the port, the adapter and the cause, which is what makes it a
//! two-minute fix rather than a puzzle. Whether a mount answers is still open, and needs the
//! group membership *and* a powered mount.

#![cfg(feature = "serialport")]

use std::time::Duration;

use astroctl_drivers::skywatcher::port::{scan, Provenance};

#[test]
#[ignore = "needs a USB-serial adapter plugged in"]
fn t_hil_1_autodetect_recognises_the_adapter_that_is_plugged_in() {
    let found = scan().expect("udev enumeration");

    println!("unrecognised USB serial ports: {:?}", found.unrecognised);
    for candidate in &found.candidates {
        println!(
            "candidate {} -> {} [{} {:04x}:{:04x}, {:?}] stable={}",
            candidate.path.display(),
            candidate.node.display(),
            candidate.adapter.family,
            candidate.adapter.vid,
            candidate.adapter.pid,
            candidate.adapter.provenance,
            candidate.is_stable()
        );
    }

    assert!(
        !found.candidates.is_empty(),
        "no recognised USB-serial bridge found. If an adapter is plugged in, its VID:PID is not \
         in KNOWN_ADAPTERS — the list above names what was seen, and that is the report needed \
         to add it."
    );

    // The stable name is what a log should carry: `/dev/ttyUSB0` is assigned in enumeration
    // order, so it moves when something else is plugged in first.
    let first = &found.candidates[0];
    assert!(
        first.is_stable(),
        "the first candidate is {}, not a /dev/serial/by-id name — udev made no stable symlink, \
         which is worth knowing before the field",
        first.path.display()
    );
    assert!(first.path.starts_with("/dev/serial/by-id/"));

    // One entry per physical port: the by-id name and the node are the same cable.
    let nodes: std::collections::HashSet<_> = found.candidates.iter().map(|c| &c.node).collect();
    assert_eq!(
        nodes.len(),
        found.candidates.len(),
        "a port was listed twice"
    );
}

#[tokio::test]
#[ignore = "needs a powered mount on the other end of the cable"]
async fn t_hil_1_the_mount_answers_the_version_inquiry() {
    // The whole of MNT-01, end to end, through the driver's own code: scan, rank, open, transmit
    // one lowercase frame, decode. Nothing here can move the mount.
    let port = astroctl_drivers::skywatcher::port::autodetect(9600, Duration::from_millis(500))
        .await
        .expect("autodetect found a mount");
    println!("autodetect chose {}", port.display());

    // The spike's own capture was `:e1` -> `=020401`, decoding to an HEQ5. If this ever reports a
    // different model, the `e` reply's field order (SDD §5.2.2's documented trap) is the first
    // thing to re-check, not the last.
    let found = scan().expect("udev enumeration");
    let chosen = found
        .candidates
        .iter()
        .find(|candidate| candidate.path == port)
        .expect("the chosen port was one of the candidates");
    println!(
        "it is a {} ({:?})",
        chosen.adapter.family, chosen.adapter.provenance
    );
    assert_eq!(
        chosen.adapter.provenance,
        Provenance::Verified,
        "a mount answered through an adapter whose ID was only `derived` — promote it in \
         KNOWN_ADAPTERS, because it has now been seen working"
    );
}
