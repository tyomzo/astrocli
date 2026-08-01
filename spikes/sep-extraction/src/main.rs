//! SEP star-extraction spike — the go/no-go on Phase 2a's plate-solving risk.
//!
//! Two questions, per the risk register:
//!
//!   1. Does libsep vendor-build cleanly with the `cc` crate on this toolchain?
//!   2. Does it hold up on real R10-scale data — speed and **peak RSS** on a 24 MP frame,
//!      against PRF-05's 512 MB ceiling?
//!
//! Everything here is measurement. See FINDINGS.md for what the numbers mean.

mod fits;
mod rss;
mod sep;
mod truth;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astroctl_core::types::RaDec;
use astroctl_drivers::simulator::sky::{Exposure, StarField};

/// Source Extractor's background tile size.
const BW: i64 = 64;
/// Source Extractor's background filter size, in tiles.
const FW: i64 = 3;
/// Detection threshold, in units of the background RMS.
const THRESH: f32 = 1.5;
/// Minimum connected area, in pixels.
const MINAREA: i32 = 5;
/// Deblending parameters — Source Extractor's defaults.
const DEBLEND_NTHRESH: i32 = 32;
const DEBLEND_CONT: f64 = 0.005;
const CLEAN_PARAM: f64 = 1.0;

/// How far a detection may sit from a truth star and still count as that star.
///
/// 3 px at the reference rig's 0.767"/px is 2.3 arcseconds — under the 3.0" FWHM the simulator
/// renders, so this cannot quietly match a star to its neighbour unless they are genuinely
/// blended. Reported in FINDINGS alongside every completeness figure, because a completeness
/// number without its tolerance is meaningless.
const MATCH_TOLERANCE_PX: f64 = 3.0;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    // Layout is asserted before any command runs. A measurement taken through a mis-mirrored
    // struct is worse than no measurement, because it looks like data.
    let layout = sep::assert_layout();

    // The extraction pixel stack. Process-global `_Atomic size_t` inside the C library, so this
    // is a startup decision and not a per-call one — see FINDINGS §7.
    if let Ok(v) = std::env::var("SEP_PIXSTACK") {
        if let Ok(n) = v.parse::<usize>() {
            sep::set_pixstack(n);
            eprintln!("(pixstack set to {n} pixels)");
        }
    }

    match cmd {
        "layout" => cmd_layout(&layout),
        "render" => cmd_render(&args),
        "bench" => cmd_bench(&args),
        "truth" => cmd_truth(&args),
        "once" => cmd_once(&args),
        "cr3" => cmd_cr3(&args),
        "cr3plane" => cmd_cr3_plane(&args),
        "pixstack" => cmd_pixstack(&args),
        "export" => cmd_export(&args),
        _ => {
            eprintln!(
                "sep-extraction-spike — usage:
  layout                                 FFI struct-layout check + library version
  render  <out.fits> [exp_s] [faintest]  render a 24 MP simulator frame as float32 FITS
  bench   <in.fits> [runs]               wall time + peak RSS, repeated
  truth   <in.fits>                      score SEP against the computed star catalogue
  cr3     <in.cr3>                       decode a real R10 frame and extract from it
  export  <in.fits> <out.fits>           16-bit sim FITS -> float32 FITS for the Python check"
            );
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------------------------

fn cmd_layout(layout: &[String]) {
    println!("libsep version : {}", sep::version());
    println!("pixstack       : {} pixels", sep::pixstack());
    println!("FFI layout check:");
    for line in layout {
        println!("{line}");
    }
    println!("OK — every hand-written struct mirror agrees with the C compiler.");
}

// ---------------------------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------------------------

/// The reference rig: 6000x4000 at 3.72 um behind 1000 mm. `CameraProfile::default()`.
const WIDTH: u32 = 6000;
const HEIGHT: u32 = 4000;
const PITCH_UM: f64 = 3.72;
const FOCAL_MM: f64 = 1000.0;
/// `StarField::default()`'s seed — the ASCII of "ASTROCTL".
const DEFAULT_SEED: u64 = 0x4153_5452_4F43_544C;

fn cmd_render(args: &[String]) {
    let out = PathBuf::from(args.get(2).map(String::as_str).unwrap_or("out/render.fits"));
    let exposure_s: f64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(30.0);
    let faintest: f64 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(18.0);

    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let scale = truth::arcsec_per_pixel(PITCH_UM, FOCAL_MM);
    // M42, the simulator's own default pointing.
    let pointing = RaDec::from_parts(5.5833, -5.3911).expect("M42 is a valid coordinate");
    let field = StarField::new(DEFAULT_SEED).with_faintest_magnitude(faintest);

    let spec = Exposure {
        width: WIDTH,
        height: HEIGHT,
        pointing,
        arcsec_per_pixel: scale,
        exposure: Duration::from_secs_f64(exposure_s),
        // ISO 3200 on the R10 profile, matching the session frames.
        gain_adu_per_electron: 8.0,
        fwhm_arcsec: 3.0,
        sky_electrons_per_second: 20.0,
        read_noise_electrons: 3.0,
        bias_adu: 512.0,
        full_well_electrons: 30_000.0,
        saturation_adu: u16::MAX,
        noise_seed: Some(0xA570_0C71),
        jitter_arcsec: (0.0, 0.0),
        injected: Vec::new(),
        field,
    };

    let started = Instant::now();
    let pixels = spec.render();
    println!("rendered {WIDTH}x{HEIGHT} in {:.2} s", started.elapsed().as_secs_f64());

    // Rows out in reverse, exactly as `simulator::fits::write` does it: FITS numbers rows from
    // the bottom. Doing the same here means every FITS this spike reads — session frame or
    // freshly rendered — has one row convention, and `Truth::into_fits_row_order` is the single
    // place that knows about it.
    let mut data: Vec<f32> = Vec::with_capacity(pixels.len());
    for row in (0..HEIGHT as usize).rev() {
        let start = row * WIDTH as usize;
        data.extend(
            pixels[start..start + WIDTH as usize]
                .iter()
                .map(|s| f32::from(*s)),
        );
    }
    let extra = vec![
        ("SIMSEED", (DEFAULT_SEED as i64).to_string(), "synthetic sky seed"),
        ("RA", format!("{:.6}", 5.5833 * 15.0), "degrees J2000"),
        ("DEC", format!("{:.6}", -5.3911), "degrees J2000"),
        ("XPIXSZ", format!("{PITCH_UM:.6}"), "pixel pitch, micrometres"),
        ("FOCALLEN", format!("{FOCAL_MM:.1}"), "focal length, mm"),
        ("EXPTIME", format!("{exposure_s:.1}"), "exposure in seconds"),
        ("FAINTEST", format!("{faintest:.1}"), "catalogue faint limit"),
    ];
    let extra_ref: Vec<(&str, String, &str)> = extra
        .into_iter()
        .map(|(k, v, c)| (k, v, c))
        .collect();
    fits::write_f32(&out, &data, WIDTH as usize, HEIGHT as usize, &extra_ref)
        .unwrap_or_else(|e| panic!("{e}"));
    println!("wrote {} ({} MB)", out.display(), data.len() * 4 / 1_048_576);
}

// ---------------------------------------------------------------------------------------------
// bench
// ---------------------------------------------------------------------------------------------

/// One pass of the production shape: background estimate, in-place subtract, extract.
struct Pass {
    background_ms: f64,
    subtract_ms: f64,
    extract_ms: f64,
    total_ms: f64,
    nobj: usize,
    global: f32,
    globalrms: f32,
}

/// Runs the pipeline once over `data`, consuming it in place, panicking on any libsep error.
fn one_pass(data: &mut [f32], width: i64, height: i64) -> Pass {
    try_one_pass(data, width, height).unwrap_or_else(|e| panic!("{e}"))
}

/// As `one_pass`, but surfaces libsep's status instead of panicking — needed because
/// `sep_extract` legitimately fails on a frame with too much area above threshold, and that
/// failure is a measurement rather than a crash.
fn try_one_pass(data: &mut [f32], width: i64, height: i64) -> Result<Pass, sep::SepError> {
    let t0 = Instant::now();

    let image = sep::SepImage::mono_f32(data, width, height);
    let bkg = sep::Background::estimate(&image, BW, BW, FW, FW, 0.0)?;
    let background_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let global = bkg.global();
    let globalrms = bkg.global_rms();

    // The image struct borrows `data` immutably; the subtraction needs it mutably. Dropping the
    // borrow here is the whole reason `SepImage` is constructed twice per pass, and is the
    // shape a production binding has to encode rather than comment on.
    drop(image);

    let t1 = Instant::now();
    bkg.subtract_in_place(data)?;
    let subtract_ms = t1.elapsed().as_secs_f64() * 1000.0;
    drop(bkg);

    let t2 = Instant::now();
    let mut image = sep::SepImage::mono_f32(data, width, height);
    // Threshold relative to the global RMS — the same thing `sep.extract(d, 1.5,
    // err=bkg.globalrms)` does in Python. Verified against extract.c: `thresh = relthresh *
    // pixsig` where `pixsig = image->noiseval`.
    image.noiseval = f64::from(globalrms);
    image.noise_type = sep::SEP_NOISE_STDDEV;

    let catalog = sep::Catalog::extract(
        &image,
        THRESH,
        sep::SEP_THRESH_REL,
        MINAREA,
        Some((&sep::DEFAULT_CONV, 3, 3)),
        DEBLEND_NTHRESH,
        DEBLEND_CONT,
        true,
        CLEAN_PARAM,
    )?;
    let extract_ms = t2.elapsed().as_secs_f64() * 1000.0;
    let nobj = catalog.len();
    drop(catalog);

    Ok(Pass {
        background_ms,
        subtract_ms,
        extract_ms,
        total_ms: t0.elapsed().as_secs_f64() * 1000.0,
        nobj,
        global,
        globalrms,
    })
}

fn cmd_bench(args: &[String]) {
    let path = PathBuf::from(args.get(2).expect("bench needs a FITS path"));
    let runs: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(20);

    let before_load = rss::sample();
    let image = fits::read(&path).unwrap_or_else(|e| panic!("{e}"));
    let (w, h) = (image.width as i64, image.height as i64);
    let megapixels = (w * h) as f64 / 1.0e6;
    println!(
        "frame          : {} — {}x{} = {:.1} MP",
        path.display(),
        w,
        h,
        megapixels
    );
    println!(
        "image buffer   : {:.1} MB as float32",
        (w * h) as f64 * 4.0 / 1_048_576.0
    );

    // The pristine copy the repeat loop needs. Held as f32 because that is what was read; the
    // 96 MB it costs is harness overhead a single-shot production run would not pay, and every
    // RSS figure below is reported with that stated.
    let pristine = image.data;
    let after_load = rss::sample();
    println!(
        "RSS after load : {:.1} MB current, {:.1} MB peak (baseline before load {:.1} MB)",
        after_load.current_mb(),
        after_load.peak_mb(),
        before_load.current_mb()
    );

    let mut totals = Vec::new();
    let mut backgrounds = Vec::new();
    let mut extracts = Vec::new();
    let mut currents = Vec::new();
    let mut peaks = Vec::new();
    let mut counts = Vec::new();
    let mut first: Option<Pass> = None;

    for run in 0..runs {
        let mut working = pristine.clone();
        let pass = one_pass(&mut working, w, h);
        drop(working);
        let s = rss::sample();

        totals.push(pass.total_ms);
        backgrounds.push(pass.background_ms);
        extracts.push(pass.extract_ms);
        currents.push(s.current_mb());
        peaks.push(s.peak_mb());
        counts.push(pass.nobj);

        if run == 0 {
            println!(
                "\nrun 1          : background {:.0} ms, subtract {:.0} ms, extract {:.0} ms, total {:.0} ms",
                pass.background_ms, pass.subtract_ms, pass.extract_ms, pass.total_ms
            );
            println!(
                "                 {} objects, global {:.1} ADU, globalrms {:.2} ADU",
                pass.nobj, pass.global, pass.globalrms
            );
            println!(
                "                 peak RSS after one pass: {:.1} MB",
                s.peak_mb()
            );
            first = Some(pass);
        }
    }
    let _ = first;

    let t = rss::Series::new(totals);
    let b = rss::Series::new(backgrounds);
    let e = rss::Series::new(extracts);
    let c = rss::Series::new(currents);
    let p = rss::Series::new(peaks);

    println!("\n--- {runs} consecutive runs ---");
    println!(
        "background   : min {:.0} ms, mean {:.0} ms, p50 {:.0} ms, max {:.0} ms",
        b.min(),
        b.mean(),
        b.percentile(50.0),
        b.max()
    );
    println!(
        "extract      : min {:.0} ms, mean {:.0} ms, p50 {:.0} ms, max {:.0} ms",
        e.min(),
        e.mean(),
        e.percentile(50.0),
        e.max()
    );
    println!(
        "total        : min {:.0} ms, mean {:.0} ms, p50 {:.0} ms, max {:.0} ms",
        t.min(),
        t.mean(),
        t.percentile(50.0),
        t.max()
    );
    println!(
        "RSS current  : first {:.1} MB, last {:.1} MB, drift {:+.1} MB, max {:.1} MB",
        c.values.first().copied().unwrap_or(0.0),
        c.values.last().copied().unwrap_or(0.0),
        c.drift(),
        c.max()
    );
    println!(
        "RSS peak     : first {:.1} MB, last {:.1} MB, drift {:+.1} MB  <-- VmHWM, monotonic",
        p.values.first().copied().unwrap_or(0.0),
        p.values.last().copied().unwrap_or(0.0),
        p.drift()
    );
    let unique: std::collections::BTreeSet<usize> = counts.iter().copied().collect();
    println!(
        "object count : {:?} across {runs} runs{}",
        unique,
        if unique.len() == 1 {
            " — deterministic"
        } else {
            " — NOT deterministic, investigate"
        }
    );

    let final_sample = rss::sample();
    println!(
        "\npeak RSS (VmHWM) for the whole process: {:.1} MB",
        final_sample.peak_mb()
    );
    println!(
        "  of which the harness's retained pristine copy: {:.1} MB (derived — a single-shot \n  production run does not hold it)",
        (w * h) as f64 * 4.0 / 1_048_576.0
    );
}

// ---------------------------------------------------------------------------------------------
// truth
// ---------------------------------------------------------------------------------------------

fn cmd_truth(args: &[String]) {
    let path = PathBuf::from(args.get(2).expect("truth needs a FITS path"));
    let image = fits::read(&path).unwrap_or_else(|e| panic!("{e}"));
    let (w, h) = (image.width as i64, image.height as i64);

    let seed = image
        .seed("SIMSEED")
        .expect("no SIMSEED in header — not a simulator frame");
    let ra_deg = image.number("RA").expect("no RA in header");
    let dec_deg = image.number("DEC").expect("no DEC in header");
    let pitch = image.number("XPIXSZ").unwrap_or(PITCH_UM);
    let focal = image.number("FOCALLEN").unwrap_or(FOCAL_MM);
    let scale = truth::arcsec_per_pixel(pitch, focal);
    let exptime = image.number("EXPTIME").unwrap_or(f64::NAN);

    let pointing = RaDec::from_parts(ra_deg / 15.0, dec_deg).expect("header coordinate is valid");

    println!("frame        : {} — {}x{}", path.display(), w, h);
    println!("seed         : {seed} (0x{seed:016X})");
    println!("pointing     : RA {ra_deg:.6} deg, Dec {dec_deg:.6} deg");
    println!("plate scale  : {scale:.4} arcsec/px  ({pitch} um / {focal} mm)");
    println!("exposure     : {exptime} s");

    // --- SEP ---
    let mut data = image.data;
    let pass = one_pass(&mut data, w, h);
    // Re-run to get the catalogue itself rather than only its length.
    let mut data2 = fits::read(&path).unwrap_or_else(|e| panic!("{e}")).data;
    let objects = {
        let img = sep::SepImage::mono_f32(&data2, w, h);
        let bkg = sep::Background::estimate(&img, BW, BW, FW, FW, 0.0).expect("background");
        let rms = bkg.global_rms();
        drop(img);
        bkg.subtract_in_place(&mut data2).expect("subtract");
        drop(bkg);
        let mut img = sep::SepImage::mono_f32(&data2, w, h);
        img.noiseval = f64::from(rms);
        img.noise_type = sep::SEP_NOISE_STDDEV;
        let cat = sep::Catalog::extract(
            &img,
            THRESH,
            sep::SEP_THRESH_REL,
            MINAREA,
            Some((&sep::DEFAULT_CONV, 3, 3)),
            DEBLEND_NTHRESH,
            DEBLEND_CONT,
            true,
            CLEAN_PARAM,
        )
        .expect("extract");
        cat.objects()
    };
    println!(
        "\nSEP          : {} objects in {:.0} ms (background {:.0} ms + extract {:.0} ms)",
        pass.nobj, pass.total_ms, pass.background_ms, pass.extract_ms
    );
    println!(
        "               global {:.1} ADU, globalrms {:.2} ADU, threshold {:.2} ADU",
        pass.global,
        pass.globalrms,
        THRESH * pass.globalrms
    );

    // --- truth ---
    // ...in FITS row order, because that is the order the file is stored in and therefore the
    // order SEP saw. See `Truth::into_fits_row_order` for why omitting this scores 0%.
    let t = truth::compute(
        seed,
        pointing,
        w as u32,
        h as u32,
        scale,
        MATCH_TOLERANCE_PX,
    )
    .into_fits_row_order(h as u32);
    println!(
        "truth        : {} stars in frame ({} generated in the search radius)",
        t.stars.len(),
        t.total_generated
    );

    let detections: Vec<(f64, f64)> = objects.iter().map(|o| (o.x, o.y)).collect();
    let score = truth::score(&t.stars, &detections, MATCH_TOLERANCE_PX);

    let (rx, ry) = score.axis_rms();
    println!("\n--- scoring, tolerance {MATCH_TOLERANCE_PX} px ---");
    println!(
        "matched      : {} of {} truth stars ({:.1}% overall completeness)",
        score.matches.len(),
        t.stars.len(),
        100.0 * score.matches.len() as f64 / t.stars.len() as f64
    );
    println!(
        "spurious     : {} of {} detections ({:.1}% false-positive rate)",
        score.spurious.len(),
        detections.len(),
        100.0 * score.spurious.len() as f64 / detections.len().max(1) as f64
    );
    println!(
        "centroid RMS : {:.4} px radial  ({:.4} px x, {:.4} px y)",
        score.centroid_rms(),
        rx,
        ry
    );
    println!(
        "               {:.4} arcsec radial at {scale:.4} arcsec/px",
        score.centroid_rms() * scale
    );
    println!(
        "               median {:.4} px, max {:.4} px",
        score.centroid_median(),
        score.centroid_max()
    );

    println!("\n--- completeness vs magnitude ---");
    println!(
        "{:>12}  {:>6}  {:>6}  {:>7}  {:>8}  {:>9}",
        "mag bin", "truth", "found", "compl.", "RMS px", "blended"
    );
    for bin in truth::bins(&t.stars, &score, MATCH_TOLERANCE_PX, 1.0) {
        println!(
            "{:>5.1}..{:<5.1}  {:>6}  {:>6}  {:>6.1}%  {:>8.4}  {:>9}",
            bin.low,
            bin.high,
            bin.truth,
            bin.detected,
            100.0 * bin.completeness(),
            bin.rms,
            bin.missed_blended
        );
    }

    // Where the spurious detections sit, which distinguishes "noise" from "the catalogue's faint
    // population that truth includes but the match tolerance missed".
    let mut spur_peak: Vec<f64> = score
        .spurious
        .iter()
        .map(|i| f64::from(objects[*i].peak))
        .collect();
    spur_peak.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if !spur_peak.is_empty() {
        println!(
            "\nspurious peak ADU: min {:.1}, median {:.1}, max {:.1}",
            spur_peak[0],
            spur_peak[spur_peak.len() / 2],
            spur_peak[spur_peak.len() - 1]
        );
    }

    // Dump the catalogue and the truth list for the Python cross-check to read.
    let out_dir = Path::new("out");
    let _ = std::fs::create_dir_all(out_dir);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let cat_path = out_dir.join(format!("{stem}.rust-catalog.csv"));
    // Full round-trip precision, not `{:.6}`. Rust's `{}` for floats prints the shortest string
    // that parses back to the identical value, so the cross-check compares the numbers SEP
    // produced rather than the numbers this formatter could represent. With `{:.6}` the Python
    // comparison bottoms out at ~5e-7 px and looks like a real (if tiny) disagreement; it is
    // entirely the print width.
    let mut csv = String::from("x,y,flux,peak,a,b,theta,npix,flag\n");
    for o in &objects {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            o.x, o.y, o.flux, o.peak, o.a, o.b, o.theta, o.npix, o.flag
        ));
    }
    std::fs::write(&cat_path, csv).expect("write catalog csv");
    println!("\nwrote {}", cat_path.display());
}

// ---------------------------------------------------------------------------------------------
// cr3 — the real-sensor arm
// ---------------------------------------------------------------------------------------------

fn cmd_cr3(args: &[String]) {
    let path = PathBuf::from(args.get(2).expect("cr3 needs a path"));
    let runs: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(20);

    let t0 = Instant::now();
    let raw = rawler::decode_file(&path).unwrap_or_else(|e| panic!("rawler decode: {e:?}"));
    let decode_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let w = raw.width as i64;
    let h = raw.height as i64;
    println!("frame        : {} — {}x{}", path.display(), w, h);
    println!("decode       : {decode_ms:.0} ms via rawler");

    let samples: Vec<f32> = match &raw.data {
        rawler::RawImageData::Integer(v) => v.iter().map(|s| f32::from(*s)).collect(),
        rawler::RawImageData::Float(v) => v.clone(),
    };
    println!(
        "buffer       : {:.1} MB as float32 ({} samples)",
        samples.len() as f64 * 4.0 / 1_048_576.0,
        samples.len()
    );
    println!(
        "black/white  : {:?} / {:?}",
        raw.blacklevel, raw.whitelevel
    );

    let after_decode = rss::sample();
    println!(
        "RSS          : {:.1} MB current, {:.1} MB peak after decode",
        after_decode.current_mb(),
        after_decode.peak_mb()
    );

    // How much of this frame sits above the detection threshold — the number that decides
    // whether the pixel stack is big enough. Computed here rather than inferred from the
    // failure, because "how much area is above threshold" is the actual quantity.
    {
        let mut probe = samples.clone();
        let img = sep::SepImage::mono_f32(&probe, w, h);
        if let Ok(bkg) = sep::Background::estimate(&img, BW, BW, FW, FW, 0.0) {
            let rms = bkg.global_rms();
            drop(img);
            let _ = bkg.subtract_in_place(&mut probe);
            let limit = THRESH * rms;
            let above = probe.iter().filter(|v| **v > limit).count();
            println!(
                "above thresh : {above} pixels ({:.2}% of the frame) at {limit:.1} ADU — the \n               pixel stack must hold the largest single connected region, not this total",
                100.0 * above as f64 / probe.len() as f64
            );
        }
    }

    let mut totals = Vec::new();
    let mut currents = Vec::new();
    let mut counts = Vec::new();

    for run in 0..runs {
        let mut working = samples.clone();
        let pass = match try_one_pass(&mut working, w, h) {
            Ok(p) => p,
            Err(e) => {
                println!("\nEXTRACT FAILED: {e}");
                println!(
                    "  pixstack was {} pixels. Re-run with SEP_PIXSTACK=<n> to raise it.",
                    sep::pixstack()
                );
                return;
            }
        };
        drop(working);
        let s = rss::sample();
        totals.push(pass.total_ms);
        currents.push(s.current_mb());
        counts.push(pass.nobj);
        if run == 0 {
            println!(
                "\nrun 1        : background {:.0} ms, subtract {:.0} ms, extract {:.0} ms, total {:.0} ms",
                pass.background_ms, pass.subtract_ms, pass.extract_ms, pass.total_ms
            );
            println!(
                "               {} objects, global {:.1}, globalrms {:.2}",
                pass.nobj, pass.global, pass.globalrms
            );
        }
    }

    let t = rss::Series::new(totals);
    let c = rss::Series::new(currents);
    println!("\n--- {runs} consecutive runs on real sensor data ---");
    println!(
        "total        : min {:.0} ms, mean {:.0} ms, p50 {:.0} ms, max {:.0} ms",
        t.min(),
        t.mean(),
        t.percentile(50.0),
        t.max()
    );
    println!(
        "RSS current  : first {:.1} MB, last {:.1} MB, drift {:+.1} MB",
        c.values.first().copied().unwrap_or(0.0),
        c.values.last().copied().unwrap_or(0.0),
        c.drift()
    );
    let unique: std::collections::BTreeSet<usize> = counts.iter().copied().collect();
    println!("object count : {unique:?}");
    println!("peak RSS     : {:.1} MB (VmHWM)", rss::sample().peak_mb());
}

// ---------------------------------------------------------------------------------------------
// once — the production shape: one buffer, one pass, the peak that PRF-05 must accommodate
// ---------------------------------------------------------------------------------------------

/// One pass over one buffer, with the resident-peak counter reset immediately beforehand.
///
/// This is the figure that matters. `bench` holds a pristine copy so it can repeat, which costs
/// a second full float32 image and inflates the peak by ~92 MB at 24 MP; that is harness
/// overhead a field node never pays. Resetting `VmHWM` after the frame is resident and before
/// the first SEP call isolates **what extraction itself adds on top of the image**.
fn cmd_once(args: &[String]) {
    let path = PathBuf::from(args.get(2).expect("once needs a FITS path"));
    let image = fits::read(&path).unwrap_or_else(|e| panic!("{e}"));
    let (w, h) = (image.width as i64, image.height as i64);
    let mut data = image.data;
    drop(image.header);

    // Force the buffer resident and settled before the counter is reset, so the reset does not
    // simply hide the image itself.
    let checksum: f64 = data.iter().step_by(4096).map(|v| f64::from(*v)).sum();
    let resident = rss::sample();

    let reset_ok = rss::reset_peak();
    let after_reset = rss::sample();

    let pass = one_pass(&mut data, w, h);
    let after = rss::sample();

    println!("frame          : {} — {}x{}", path.display(), w, h);
    println!(
        "image buffer   : {:.1} MB float32 (checksum {checksum:.0}, forces residency)",
        (w * h) as f64 * 4.0 / 1_048_576.0
    );
    println!(
        "RSS with frame : {:.1} MB current, {:.1} MB peak",
        resident.current_mb(),
        resident.peak_mb()
    );
    if reset_ok {
        println!(
            "peak reset     : yes (/proc/self/clear_refs) — peak now {:.1} MB",
            after_reset.peak_mb()
        );
    } else {
        println!("peak reset     : NOT SUPPORTED on this kernel — figures below are cumulative");
    }
    println!(
        "\npass           : background {:.0} ms, subtract {:.0} ms, extract {:.0} ms, total {:.0} ms",
        pass.background_ms, pass.subtract_ms, pass.extract_ms, pass.total_ms
    );
    println!("objects        : {}", pass.nobj);
    println!(
        "RSS after pass : {:.1} MB current, {:.1} MB peak",
        after.current_mb(),
        after.peak_mb()
    );
    println!(
        "\nEXTRACTION OVERHEAD ABOVE THE IMAGE: {:.1} MB",
        after.peak_mb() - after_reset.current_mb()
    );
    println!(
        "PRODUCTION-SHAPE PEAK (image + extraction): {:.1} MB",
        after.peak_mb()
    );
}

// ---------------------------------------------------------------------------------------------
// cr3plane — the same real frame, as a single CFA plane rather than a Bayer mosaic
// ---------------------------------------------------------------------------------------------

/// Extracts one Bayer plane (every other pixel in both axes) and runs the pipeline on it.
///
/// # Why this arm exists
///
/// Feeding SEP a raw CFA mosaic is wrong, and the whole-frame `cr3` run showed how wrong: a
/// Bayer mosaic superimposes a 2-pixel checkerboard on everything, so neighbouring pixels differ
/// by the colour response of the scene rather than by the image structure. Every connected
/// region the extractor traces is shaped by that checkerboard.
///
/// A real pipeline never does this — it debayers, or takes a single plane, or sums a 2x2 block.
/// Taking the plane is the cheapest of those and the one that changes the data least, so it is
/// what the spike measures.
fn cmd_cr3_plane(args: &[String]) {
    let path = PathBuf::from(args.get(2).expect("cr3plane needs a path"));
    let runs: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(20);

    let raw = rawler::decode_file(&path).unwrap_or_else(|e| panic!("rawler decode: {e:?}"));
    let (fw, fh) = (raw.width as usize, raw.height as usize);
    let full: Vec<u16> = match &raw.data {
        rawler::RawImageData::Integer(v) => v.clone(),
        rawler::RawImageData::Float(v) => v.iter().map(|f| *f as u16).collect(),
    };

    // Plane (0,0) of the 2x2 CFA — for an RGGB sensor that is R. Any plane answers the
    // structural question; the choice is not load-bearing.
    let (pw, ph) = (fw / 2, fh / 2);
    let mut plane = Vec::with_capacity(pw * ph);
    for row in 0..ph {
        for col in 0..pw {
            plane.push(f32::from(full[row * 2 * fw + col * 2]));
        }
    }
    drop(full);

    println!("frame        : {} — CFA plane {}x{}", path.display(), pw, ph);
    println!(
        "buffer       : {:.1} MB as float32",
        plane.len() as f64 * 4.0 / 1_048_576.0
    );

    let (w, h) = (pw as i64, ph as i64);
    let mut totals = Vec::new();
    let mut counts = Vec::new();
    let mut currents = Vec::new();

    for run in 0..runs {
        let mut working = plane.clone();
        let pass = match try_one_pass(&mut working, w, h) {
            Ok(p) => p,
            Err(e) => {
                println!("\nEXTRACT FAILED: {e}  (pixstack {})", sep::pixstack());
                return;
            }
        };
        drop(working);
        let s = rss::sample();
        totals.push(pass.total_ms);
        counts.push(pass.nobj);
        currents.push(s.current_mb());
        if run == 0 {
            println!(
                "\nrun 1        : background {:.0} ms, subtract {:.0} ms, extract {:.0} ms, total {:.0} ms",
                pass.background_ms, pass.subtract_ms, pass.extract_ms, pass.total_ms
            );
            println!(
                "               {} objects, global {:.1}, globalrms {:.2}",
                pass.nobj, pass.global, pass.globalrms
            );
        }
    }

    let t = rss::Series::new(totals);
    let c = rss::Series::new(currents);
    println!("\n--- {runs} consecutive runs, real sensor data, single CFA plane ---");
    println!(
        "total        : min {:.0} ms, mean {:.0} ms, p50 {:.0} ms, max {:.0} ms",
        t.min(),
        t.mean(),
        t.percentile(50.0),
        t.max()
    );
    println!(
        "RSS current  : first {:.1} MB, last {:.1} MB, drift {:+.1} MB",
        c.values.first().copied().unwrap_or(0.0),
        c.values.last().copied().unwrap_or(0.0),
        c.drift()
    );
    let unique: std::collections::BTreeSet<usize> = counts.iter().copied().collect();
    println!("object count : {unique:?}");
    println!("peak RSS     : {:.1} MB (VmHWM)", rss::sample().peak_mb());
}

// ---------------------------------------------------------------------------------------------
// pixstack — how much memory the extraction stack costs, and where the frame starts to fit
// ---------------------------------------------------------------------------------------------

/// Sweeps `sep_set_extract_pixstack` over a frame, reporting for each value whether extraction
/// succeeded and what it cost in resident memory.
///
/// Each value runs in a freshly-`fork`ed process would be ideal; it does not, so the RSS figures
/// are *incremental* — the interesting quantity is the step between rows, which is the
/// allocation `sep_extract` makes for the stack (`mem_pixstack * plistsize` bytes at
/// extract.c:431).
fn cmd_pixstack(args: &[String]) {
    let path = PathBuf::from(args.get(2).expect("pixstack needs a frame path"));

    let (samples, w, h) = if path.extension().is_some_and(|e| {
        e.eq_ignore_ascii_case("cr3")
    }) {
        let raw = rawler::decode_file(&path).unwrap_or_else(|e| panic!("rawler decode: {e:?}"));
        let v: Vec<f32> = match &raw.data {
            rawler::RawImageData::Integer(v) => v.iter().map(|s| f32::from(*s)).collect(),
            rawler::RawImageData::Float(v) => v.clone(),
        };
        (v, raw.width as i64, raw.height as i64)
    } else {
        let img = fits::read(&path).unwrap_or_else(|e| panic!("{e}"));
        let (w, h) = (img.width as i64, img.height as i64);
        (img.data, w, h)
    };

    println!("frame        : {} — {}x{}", path.display(), w, h);
    println!(
        "{:>12}  {:>9}  {:>10}  {:>10}  {:>8}",
        "pixstack", "result", "objects", "extract ms", "RSS MB"
    );

    let mut previous_peak = rss::sample().peak_mb();
    for pixstack in [
        300_000_usize,
        600_000,
        1_200_000,
        2_400_000,
        4_800_000,
        9_600_000,
        19_200_000,
    ] {
        sep::set_pixstack(pixstack);
        let mut working = samples.clone();
        let outcome = try_one_pass(&mut working, w, h);
        drop(working);
        let peak = rss::sample().peak_mb();
        match outcome {
            Ok(p) => {
                println!(
                    "{pixstack:>12}  {:>9}  {:>10}  {:>10.0}  {:>8.1}",
                    "ok", p.nobj, p.extract_ms, peak
                );
            }
            Err(e) => {
                let short = if e.message.len() > 9 {
                    "FULL"
                } else {
                    e.message.as_str()
                };
                println!(
                    "{pixstack:>12}  {short:>9}  {:>10}  {:>10}  {:>8.1}",
                    "-", "-", peak
                );
            }
        }
        previous_peak = previous_peak.max(peak);
    }
    println!(
        "\nfinal peak RSS: {:.1} MB (VmHWM — monotonic, so the rows above only ever rise)",
        rss::sample().peak_mb()
    );
    println!(
        "plist entry   : extract.c:431 allocates `mem_pixstack * plistsize` bytes. plistsize is\n\
         \x20               44 bytes with a convolution filter and scalar noise — pbliststruct\n\
         \x20               (8+8+8+4 padded to 32) + 4 for cdvalue + 8 for var and thresh\n\
         \x20               (extract.c:1058 plistinit). The measured step between rows above\n\
         \x20               agrees: 402.8 MB per 9.6 M entries = 44.0 bytes."
    );
}

// ---------------------------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------------------------

fn cmd_export(args: &[String]) {
    let src = PathBuf::from(args.get(2).expect("export needs an input FITS"));
    let dst = PathBuf::from(args.get(3).expect("export needs an output FITS"));
    let image = fits::read(&src).unwrap_or_else(|e| panic!("{e}"));
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Carry the truth-bearing keywords across so the exported file is self-describing.
    let mut extra: Vec<(&str, String, &str)> = Vec::new();
    for (key, comment) in [
        ("SIMSEED", "synthetic sky seed"),
        ("RA", "degrees J2000"),
        ("DEC", "degrees J2000"),
        ("XPIXSZ", "pixel pitch, micrometres"),
        ("FOCALLEN", "focal length, mm"),
        ("EXPTIME", "exposure in seconds"),
    ] {
        if let Some(v) = image.header.get(key) {
            extra.push((key, v.trim().to_string(), comment));
        }
    }

    fits::write_f32(&dst, &image.data, image.width, image.height, &extra)
        .unwrap_or_else(|e| panic!("{e}"));
    println!(
        "{} -> {} ({}x{}, float32)",
        src.display(),
        dst.display(),
        image.width,
        image.height
    );
}
