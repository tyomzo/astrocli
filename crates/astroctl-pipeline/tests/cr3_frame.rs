//! The CR3 arm against a frame a real R10 wrote. M2-T05.
//!
//! **`#[ignore]`d**, like `astroctl-drivers`' hardware run and for a weaker version of the same
//! reason: this one needs no camera, only a file a camera produced, and CI has neither. Everything
//! about the arm that can be checked without one — the pattern-independence identity, the refusal
//! paths, the sniffer — is a unit test in `src/cr3.rs` and runs in all six gates.
//!
//! What only a real frame can answer is the part M2-T05 asks for in numbers: **how long the decode
//! takes and what it costs in resident memory**, because PRF-05 gives the field node 512 MB and a
//! 24 MP wavelet decode is the largest transient allocation in the whole node.
//!
//! # Running it
//!
//! ```sh
//! # --release matters more here than anywhere else in the tree: this is a measurement, and the
//! # debug build's bounds-checked inner loop over 24 million photosites is not the thing that
//! # ships. The numbers in the evidence bundle are release numbers.
//! ASTROCTL_CR3=/path/to/frame.cr3 \
//!   cargo test -p astroctl-pipeline --release --test cr3_frame -- --ignored --nocapture
//! ```
//!
//! With no `ASTROCTL_CR3` the test looks for a frame left by the driver's hardware run under
//! `/tmp/astroctl-r10-*/` and skips, loudly, if there is none. Skipping rather than failing is
//! deliberate: an absent camera is not a broken decoder, and a red test that means "no hardware
//! today" is a test people learn to ignore.

use std::path::PathBuf;
use std::time::Instant;

use astroctl_pipeline::{PreviewParams, SourceFormat};

/// Resident set size in KB, from the kernel rather than an allocator's own accounting — the
/// interesting number is what the OS thinks the process holds, which is what PRF-05 bounds.
fn resident_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("procfs");
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
        .expect("VmRSS")
}

/// The frame to measure against: `$ASTROCTL_CR3`, else the newest one the driver's hardware run
/// left in `/tmp`.
fn frame() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ASTROCTL_CR3") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "ASTROCTL_CR3 is set to {} which is not a file",
            path.display()
        );
        return Some(path);
    }

    let mut found: Vec<PathBuf> = glob_tmp_frames();
    found.sort();
    found.pop()
}

fn glob_tmp_frames() -> Vec<PathBuf> {
    let mut frames = Vec::new();
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return frames;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("astroctl-r10-") {
            continue;
        }
        let Ok(inner) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for file in inner.flatten() {
            if file.path().extension().is_some_and(|e| e == "cr3") {
                frames.push(file.path());
            }
        }
    }
    frames
}

#[test]
#[ignore = "needs a CR3 a real camera wrote; set ASTROCTL_CR3 and run with --ignored --nocapture"]
fn a_real_r10_frame_previews_within_the_budget() {
    let Some(path) = frame() else {
        println!(
            "SKIPPED: no CR3 found. Set ASTROCTL_CR3, or run the driver's hardware capture first."
        );
        return;
    };

    println!("\n=== M2-T05 · CR3 preview arm, real frame ===\n");
    println!("frame: {}", path.display());

    let bytes = std::fs::read(&path).expect("the frame is readable");
    println!("size: {:.1} MB", bytes.len() as f64 / 1e6);

    // The sniffer is the thing that decides which arm runs, so it is asserted here against a real
    // file rather than only against the twelve bytes the unit test builds.
    assert_eq!(
        SourceFormat::sniff(&bytes),
        Some(SourceFormat::Cr3),
        "a frame the R10 wrote must sniff as CR3"
    );

    let baseline_kb = resident_kb();

    let started = Instant::now();
    let decoded = astroctl_pipeline::decode(&bytes, SourceFormat::Cr3).expect("the frame decodes");
    let decode_ms = started.elapsed().as_secs_f64() * 1e3;
    let after_decode_kb = resident_kb();

    let rendered = Instant::now();
    let preview = astroctl_pipeline::render_decoded(&decoded, &PreviewParams::default());
    let render_ms = rendered.elapsed().as_secs_f64() * 1e3;
    let peak_kb = resident_kb();

    println!(
        "half-size decode: {}×{} in {decode_ms:.0} ms",
        decoded.width(),
        decoded.height()
    );
    println!(
        "quarter-res + asinh + JPEG: {}×{}, {:.0} KB in {render_ms:.0} ms",
        preview.width,
        preview.height,
        preview.jpeg.len() as f64 / 1e3
    );
    println!("total: {:.0} ms", decode_ms + render_ms);
    println!(
        "RSS: {:.0} MB baseline → {:.0} MB after decode → {:.0} MB after render (peak delta \
         {:.0} MB)",
        baseline_kb as f64 / 1024.0,
        after_decode_kb as f64 / 1024.0,
        peak_kb as f64 / 1024.0,
        (peak_kb.saturating_sub(baseline_kb)) as f64 / 1024.0
    );

    // --- what the numbers have to satisfy ---------------------------------------------------

    // The preview is a quarter of the half-size frame in each direction, i.e. an eighth of the
    // sensor — M2-T05's "half-size decode → quarter-res" read literally.
    assert_eq!(preview.width, decoded.width() / 4);
    assert_eq!(preview.height, decoded.height() / 4);

    assert_eq!(&preview.jpeg[..2], &[0xFF, 0xD8], "JPEG SOI marker");

    // A frame that decoded to a uniform value would pass every structural assertion above and be
    // useless. Lens-cap frames are *nearly* uniform by design, so this asks only that the samples
    // are not literally all one value — the honest bar for a dark frame.
    //
    // The two ways that can fail say opposite things, so they are reported separately. A frame
    // uniform *at the top of the sensor's range* is a blown exposure and a fixture problem: the
    // first R10 bulb frame this arm was pointed at was a 10 s exposure in a lit room, every
    // photosite pinned at the 14-bit clip. A frame uniform *at zero* is the decoder's fault.
    let darkest = decoded.samples().iter().copied().min().expect("samples");
    let brightest = decoded.samples().iter().copied().max().expect("samples");
    println!("sample range: {darkest}..{brightest}, black-corrected sensor units");
    assert!(
        brightest > 0,
        "every sample decoded to zero — the decoder is subtracting more than the frame holds"
    );
    assert!(
        brightest > darkest,
        "the frame decoded to the single value {darkest}. Above ~14000 that is a blown exposure, \
         not a broken decoder: re-shoot with the lens cap on or a shorter exposure."
    );

    // ...and the same claim about the JPEG the operator actually receives, which is a stronger
    // one and caught something the sample range alone did not.
    //
    // **A blown frame renders black, exactly as a dark frame does.** The first desk run produced a
    // 10 s bulb at f/1.8 with the cap off; its samples spanned 12573..14336 — a healthy-looking
    // range — and its preview was a uniform 0/255. The mechanism is `stretch::Window::from_samples`:
    // with more than 99.5 % of the frame at one value both percentiles land on it, the window
    // collapses, and the `white = black + 1.0` guard maps that value to *zero*. That guard is
    // deliberate and matches `workers/compute_worker.py` line for line, so this is not a defect to
    // fix here — but it does mean over-exposed and under-exposed are the same picture on the
    // operator's screen, which is worth knowing at 2 a.m. Recorded in the M2-T05 notes.
    //
    // For this test it means the fixture must be a *normally exposed* frame. Both failure modes
    // are flat, so the assertion catches either, and the message says which.
    let shown = astroctl_pipeline::decode(&preview.jpeg, SourceFormat::Jpeg)
        .expect("the preview this crate just wrote must be readable");
    let dimmest = shown.samples().iter().copied().min().expect("samples") / 257;
    let brightest_shown = shown.samples().iter().copied().max().expect("samples") / 257;
    println!("preview tones: {dimmest}..{brightest_shown} of 0..255");
    assert!(
        brightest_shown - dimmest > 16,
        "the preview is a flat {dimmest}/255 rectangle. The samples ran {darkest}..{brightest}, so \
         if that range is narrow relative to the sensor the frame is saturated or capped and the \
         stretch window has collapsed — re-shoot at a lower ISO or a narrower aperture."
    );

    // PRF-05 gives the whole node 512 MB. This is one stage of one pipeline, so the bar here is
    // deliberately far under that: a 24 MP frame is 48 MB as u16, the binned half is 12 MB, and a
    // decoder that needed several multiples of the frame would be the thing that pushes a Pi over.
    let growth_mb = (peak_kb.saturating_sub(baseline_kb)) as f64 / 1024.0;
    assert!(
        growth_mb < 384.0,
        "the decode grew RSS by {growth_mb:.0} MB, which does not leave room under PRF-05's 512 MB"
    );

    println!("\n=== run complete ===\n");
}

#[test]
#[ignore = "needs a CR3 a real camera wrote; set ASTROCTL_CR3 and run with --ignored --nocapture"]
fn repeated_decodes_do_not_grow_the_process() {
    // The spike measured flat RSS over 20 `decode_file` calls (M2-T01 step 8). This is the same
    // claim about *this* crate's arm, which allocates differently: it holds the binned half-frame
    // as well as rawler's plane. A soak captures every 60 s for two hours — 120 decodes — so a
    // per-decode leak of even a few MB is a wedged node by the end.
    //
    // # Warm-up is load-bearing here, and the reason is worth writing down
    //
    // On a 32-core desk machine this measurement first read as a 48 MB "leak" over 10 rounds. It
    // is not one: `rawler` decodes CRX on a thread pool, glibc gives each thread its own malloc
    // arena, and RSS therefore climbs in *steps* as the pool warms and then stops. Ten rounds is
    // short enough to catch the climb and call it a trend. The distinguishing property of a leak
    // is that it does not plateau, so this measures after the plateau — and a leak of the size
    // that would matter over a two-hour soak still fails it.
    //
    // The absolute figure is the one for the evidence bundle rather than the assertion: arena
    // count scales with cores, so a 4-core Pi holds a small fraction of what this host does.
    let Some(path) = frame() else {
        println!("SKIPPED: no CR3 found. Set ASTROCTL_CR3.");
        return;
    };
    let bytes = std::fs::read(&path).expect("the frame is readable");

    let rounds: usize = std::env::var("ASTROCTL_CR3_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    let warmup = rounds / 3;

    let mut settled_kb = 0;
    for round in 0..rounds {
        let decoded = astroctl_pipeline::decode(&bytes, SourceFormat::Cr3).expect("decode");
        let _ = astroctl_pipeline::render_decoded(&decoded, &PreviewParams::default());
        let rss = resident_kb();
        if round + 1 == warmup {
            settled_kb = rss;
        }
        println!(
            "round {:>3}: RSS {:.0} MB{}",
            round + 1,
            rss as f64 / 1024.0,
            if round < warmup { " (warm-up)" } else { "" }
        );
    }

    let peak_mb = resident_kb() as f64 / 1024.0;
    let growth_mb = (resident_kb().saturating_sub(settled_kb)) as f64 / 1024.0;
    println!(
        "after {warmup} warm-up rounds: {:.0} MB; after all {rounds}: {peak_mb:.0} MB \
         (drift {growth_mb:.1} MB)",
        settled_kb as f64 / 1024.0
    );
    assert!(
        growth_mb < 32.0,
        "RSS drifted {growth_mb:.1} MB over the {} measured decodes, which is a leak rather than \
         a warm allocator",
        rounds - warmup
    );
}
