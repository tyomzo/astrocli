//! M2-T01 spike — Canon EOS R10 via the `gphoto2` crate, plus `rawler` CR3 validation.
//!
//! Report task, not production code. Every step reports what happened, including failures:
//! "this operation is not covered by the crate" is a result, not an error to hide.
//!
//! Usage:  cargo run --release -- [steps]     e.g. `-- 1 2 6`   (default: 1 2 6, the safe ones)
//!         Steps 3, 4, 5 actuate the shutter / stream video.
//!         Step 7 (cable pull) needs a human and is not automated here.
//!         Step 8 decodes the newest CR3 in ./out with rawler.

use gphoto2::{
    widget::{RadioWidget, TextWidget, ToggleWidget, Widget},
    Camera, Context, Result,
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const OUT: &str = "out";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let steps: Vec<u8> = if args.is_empty() {
        vec![1, 2, 6]
    } else {
        args.iter().filter_map(|a| a.parse().ok()).collect()
    };

    fs::create_dir_all(OUT).ok();
    println!("=== M2-T01 spike ===");
    println!("libgphoto2: {}", gphoto2::library_version().unwrap_or("unknown"));
    println!("steps requested: {steps:?}\n");

    // Step 8 needs no camera.
    if steps == [8] {
        step8_rawler();
        return;
    }

    let ctx = match Context::new() {
        Ok(c) => c,
        Err(e) => return println!("FATAL: Context::new failed: {e}"),
    };
    let t = Instant::now();
    let cam = match ctx.autodetect_camera().wait() {
        Ok(c) => {
            println!("[connect] autodetect OK in {:?}", t.elapsed());
            c
        }
        Err(e) => {
            println!("FATAL: autodetect failed: {e}");
            diagnose_claim_failure();
            return;
        }
    };

    for s in &steps {
        println!();
        match *s {
            1 => step1_identity_and_config(&cam),
            2 => step2_settings(&cam),
            3 => step3_capture(&cam),
            4 => step4_bulb(&cam),
            5 => step5_liveview(&cam, &ctx),
            6 => step6_battery_storage(&cam),
            7 => println!("[step 7] cable-pull test is manual — see FINDINGS.md"),
            8 => step8_rawler(),
            9 => step9_record(&cam, &ctx),
            n => println!("unknown step {n}"),
        }
    }
    println!("\n=== end ===");
}

/// libgphoto2 reports "Could not claim the USB device" for several distinct causes and gives
/// no hint which. The commonest on a desktop is a gvfs auto-mount holding the claim. Detect it
/// and say so, rather than leaving the operator to guess. (Observed for real, 2026-07-29.)
fn diagnose_claim_failure() {
    println!("\n--- diagnosing ---");
    let uid = unsafe { libc_getuid() };
    let gvfs = format!("/run/user/{uid}/gvfs");
    let mut culprit = None;
    if let Ok(rd) = fs::read_dir(&gvfs) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.contains("gphoto2") {
                culprit = Some(n);
            }
        }
    }
    match culprit {
        Some(mount) => {
            println!("  CAUSE FOUND: gvfs has the camera mounted and holds the USB claim.");
            println!("    mount: {gvfs}/{mount}");
            println!("  Release it now:");
            println!("    gio mount -u \"gphoto2://{}/\"",
                     mount.trim_start_matches("gphoto2:host="));
            println!("  Prevent it permanently (no root needed):");
            println!("    systemctl --user mask gvfs-gphoto2-volume-monitor.service");
            println!("    plus a shadowing D-Bus service file, see FINDINGS.md");
            println!("  On a headless field node this does not arise — there is no gvfs.");
        }
        None => {
            println!("  No gvfs gphoto2 mount found. Other causes to check:");
            println!("    - camera powered off, asleep, or not in PTP mode");
            println!("    - another process holds the device (lsof /dev/bus/usb/...)");
            println!("    - insufficient permissions on the USB node");
        }
    }
}

extern "C" { #[link_name = "getuid"] fn libc_getuid() -> u32; }

// ---------------------------------------------------------------- step 1

fn step1_identity_and_config(cam: &Camera) {
    println!("[step 1] identity + full config tree");
    let a = cam.abilities();
    println!("  model      : {:?}", a.model());
    println!("  driver     : {:?}", a.driver_status());
    println!("  usb_info   : {:?}", a.usb_info());
    println!("  camera_ops : {:?}", a.camera_operations());
    println!("  file_ops   : {:?}", a.file_operations());
    match cam.summary().map_err(|e| e.to_string()) {
        Ok(s) => {
            let path = Path::new(OUT).join("summary.txt");
            fs::write(&path, &s).ok();
            println!("  summary -> {} ({} bytes)", path.display(), s.len());
        }
        Err(e) => println!("  summary failed: {e}"),
    }

    let t = Instant::now();
    match cam.config().wait() {
        Ok(root) => {
            let mut buf = String::new();
            dump(&Widget::Group(root), 0, &mut buf);
            let path = Path::new(OUT).join("config-tree.txt");
            fs::write(&path, &buf).ok();
            println!(
                "  config tree read in {:?}, {} lines -> {}",
                t.elapsed(),
                buf.lines().count(),
                path.display()
            );
        }
        Err(e) => println!("  config() FAILED: {e}"),
    }
}

fn dump(w: &Widget, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match w {
        Widget::Group(g) => {
            out.push_str(&format!("{pad}[{}] {}\n", g.name(), g.label()));
            for child in g.children_iter() {
                dump(&child, depth + 1, out);
            }
        }
        Widget::Radio(r) => {
            let choices: Vec<String> = r.choices_iter().collect();
            out.push_str(&format!(
                "{pad}{} = {:?}  (radio, ro={}) choices={:?}\n",
                r.name(), r.choice(), r.readonly(), choices
            ));
        }
        Widget::Text(t) => out.push_str(&format!(
            "{pad}{} = {:?}  (text, ro={})\n", t.name(), t.value(), t.readonly())),
        Widget::Toggle(t) => out.push_str(&format!(
            "{pad}{} = {:?}  (toggle, ro={})\n", t.name(), t.toggled(), t.readonly())),
        Widget::Range(r) => out.push_str(&format!(
            "{pad}{} = {:?}  (range, ro={})\n", r.name(), r.value(), r.readonly())),
        Widget::Button(b) => out.push_str(&format!(
            "{pad}{} (button, ro={})\n", b.name(), b.readonly())),
        Widget::Date(d) => out.push_str(&format!(
            "{pad}{} = {:?} (date, ro={})\n", d.name(), d.timestamp(), d.readonly())),
    }
}

// ---------------------------------------------------------------- step 2

fn step2_settings(cam: &Camera) {
    println!("[step 2] settings get/set + enumerate");
    for key in ["iso", "shutterspeed", "aperture", "imageformat", "capturetarget"] {
        let t = Instant::now();
        match cam.config_key::<RadioWidget>(key).wait() {
            Ok(w) => {
                let choices: Vec<String> = w.choices_iter().collect();
                println!(
                    "  {key:14} = {:?}  ({} choices, {:?}) {}",
                    w.choice(),
                    choices.len(),
                    t.elapsed(),
                    if choices.len() <= 8 { format!("{choices:?}") } else { String::new() }
                );
            }
            Err(e) => match cam.config_key::<TextWidget>(key).wait() {
                Ok(w) => println!("  {key:14} = {:?}  (text)", w.value()),
                Err(_) => println!("  {key:14} NOT AVAILABLE: {e}"),
            },
        }
    }

    // Round-trip write: change ISO to another legal choice and back.
    if let Ok(w) = cam.config_key::<RadioWidget>("iso").wait() {
        let original = w.choice();
        let choices: Vec<String> = w.choices_iter().collect();
        if let Some(target) = choices.iter().find(|c| **c != original && c.parse::<u32>().is_ok()) {
            let t = Instant::now();
            w.set_choice(target).ok();
            let r = cam.set_config(&w).wait();
            println!("  iso {original} -> {target}: {:?} in {:?}", r.map(|_| "OK"), t.elapsed());
            w.set_choice(&original).ok();
            cam.set_config(&w).wait().ok();
            println!("  iso restored to {original}");
        }
    }
}

// ---------------------------------------------------------------- step 3

fn step3_capture(cam: &Camera) {
    println!("[step 3] timed capture + download");
    let t0 = Instant::now();
    let path = match cam.capture_image().wait() {
        Ok(p) => p,
        Err(e) => return println!("  capture_image FAILED: {e}"),
    };
    let t_capture = t0.elapsed();
    println!("  captured {:?}/{:?} in {:?}", path.folder(), path.name(), t_capture);

    let name = path.name().to_string();
    // gphoto2's download_to refuses to overwrite: unlink first, and use a unique name so
    // repeated runs do not collide (the camera reuses capt0000.cr3 in Internal RAM).
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let dest = PathBuf::from(OUT).join(format!("{stamp}_{name}"));
    let _ = fs::remove_file(&dest);
    let t1 = Instant::now();
    match cam.fs().download_to(&path.folder(), &name, &dest).wait() {
        Ok(_) => {
            let sz = fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            println!(
                "  downloaded -> {} ({:.1} MB) in {:?}  [{:.1} MB/s]",
                dest.display(),
                sz as f64 / 1e6,
                t1.elapsed(),
                (sz as f64 / 1e6) / t1.elapsed().as_secs_f64()
            );
        }
        Err(e) => println!("  download FAILED: {e}"),
    }
}

// ---------------------------------------------------------------- step 4 (THE question)

fn step4_bulb(cam: &Camera) {
    println!("[step 4] BULB — the critical unknown (10 s exposure)");

    // Canon EOS path: eosremoterelease radio widget.
    match cam.config_key::<RadioWidget>("eosremoterelease").wait() {
        Ok(w) => {
            let choices: Vec<String> = w.choices_iter().collect();
            println!("  eosremoterelease present, choices={choices:?}");
            let press = choices.iter().find(|c| c.contains("Full")).cloned();
            let release = choices.iter().find(|c| c.contains("Release Full")).cloned();
            match (press, release) {
                (Some(p), Some(r)) => {
                    let t = Instant::now();
                    w.set_choice(&p).ok();
                    if let Err(e) = cam.set_config(&w).wait() {
                        return println!("  press '{p}' FAILED: {e}");
                    }
                    println!("  pressed '{p}', holding 10 s…");
                    std::thread::sleep(Duration::from_secs(10));
                    w.set_choice(&r).ok();
                    match cam.set_config(&w).wait() {
                        Ok(_) => println!("  released '{r}' — total {:?}", t.elapsed()),
                        Err(e) => return println!("  release FAILED: {e} — SHUTTER MAY STILL BE OPEN"),
                    }
                    drain_events(cam, Duration::from_secs(30));
                    println!("  VERDICT: bulb via crate = WORKS (verify exposure time in EXIF)");
                }
                _ => println!("  VERDICT: eosremoterelease lacks Press/Release Full — CLI fallback needed"),
            }
        }
        Err(e) => {
            println!("  eosremoterelease NOT AVAILABLE via crate: {e}");
            match cam.config_key::<ToggleWidget>("bulb").wait() {
                Ok(_) => println!("  'bulb' toggle exists — try that path"),
                Err(e2) => println!("  'bulb' toggle also absent: {e2}"),
            }
            println!("  VERDICT: bulb NOT covered by bindings — CLI fallback required (camera.ops_via_cli = [\"bulb\"])");
        }
    }
}

fn drain_events(cam: &Camera, budget: Duration) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match cam.wait_event(Duration::from_secs(2)).wait() {
            Ok(gphoto2::camera::CameraEvent::NewFile(p)) => {
                let name = p.name().to_string();
                let dest = PathBuf::from(OUT).join(&name);
                let _ = cam.fs().download_to(&p.folder(), &name, &dest).wait();
                let sz = fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                println!("  event NewFile {name} -> {:.1} MB", sz as f64 / 1e6);
                return;
            }
            Ok(gphoto2::camera::CameraEvent::Timeout) => continue,
            Ok(other) => println!("  event {other:?}"),
            Err(e) => return println!("  wait_event error: {e}"),
        }
    }
    println!("  no NewFile event within budget");
}

// ---------------------------------------------------------------- step 5

fn step5_liveview(cam: &Camera, ctx: &Context) {
    println!("[step 5] live view — 15 s of preview frames");
    let end = Instant::now() + Duration::from_secs(15);
    let (mut n, mut bytes, mut worst) = (0u32, 0usize, Duration::ZERO);
    let start = Instant::now();
    while Instant::now() < end {
        let t = Instant::now();
        match cam.capture_preview().wait() {
            Ok(f) => {
                let d = t.elapsed();
                worst = worst.max(d);
                bytes += f.get_data(ctx).wait().map(|d| d.len()).unwrap_or(0);
                n += 1;
            }
            Err(e) => {
                println!("  capture_preview FAILED after {n} frames: {e}");
                return;
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "  {n} frames in {secs:.1} s = {:.1} fps, mean {:.0} KB/frame, worst frame {:?}",
        n as f64 / secs,
        if n > 0 { bytes as f64 / n as f64 / 1024.0 } else { 0.0 },
        worst
    );
    println!("  PRF-02 needs >= 5 fps on LAN: {}", if n as f64 / secs >= 5.0 { "MET" } else { "NOT MET" });
}

// ---------------------------------------------------------------- step 6

fn step6_battery_storage(cam: &Camera) {
    println!("[step 6] battery + storage");
    for key in ["batterylevel", "serialnumber", "cameramodel", "lensname"] {
        match cam.config_key::<TextWidget>(key).wait() {
            Ok(w) => println!("  {key:14} = {:?}", w.value()),
            Err(_) => match cam.config_key::<RadioWidget>(key).wait() {
                Ok(w) => println!("  {key:14} = {:?}", w.choice()),
                Err(e) => println!("  {key:14} unavailable: {e}"),
            },
        }
    }
    match cam.storages().wait() {
        Ok(s) => {
            for st in &s {
                println!("  storage: {:?}", st);
            }
            if s.is_empty() {
                println!("  no storage reported (card inserted?)");
            }
        }
        Err(e) => println!("  storages() FAILED: {e}"),
    }
}

// ---------------------------------------------------------------- step 9

/// Record live-view frames with millisecond timestamps, for optical motion detection.
fn step9_record(cam: &Camera, ctx: &Context) {
    let secs: u64 = std::env::var("REC_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(70);
    let dir = PathBuf::from(OUT).join("lv");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).ok();
    println!("[step 9] recording live view to {} for {secs}s", dir.display());
    let t0 = Instant::now();
    let mut n = 0u32;
    while t0.elapsed() < Duration::from_secs(secs) {
        match cam.capture_preview().wait() {
            Ok(f) => match f.get_data(ctx).wait() {
                Ok(d) => {
                    let ms = t0.elapsed().as_millis();
                    let p = dir.join(format!("{:07}_{:05}.jpg", ms, n));
                    fs::write(&p, &d).ok();
                    n += 1;
                }
                Err(e) => { println!("  get_data failed: {e}"); return; }
            },
            Err(e) => { println!("  capture_preview failed after {n}: {e}"); return; }
        }
    }
    println!("  recorded {n} frames in {:?}", t0.elapsed());
}

// ---------------------------------------------------------------- step 8

fn step8_rawler() {
    println!("[step 8] rawler CR3 decode validation");
    let mut raws: Vec<PathBuf> = fs::read_dir(OUT)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("cr3"))
                .unwrap_or(false)
        })
        .collect();
    raws.sort();
    let Some(file) = raws.last() else {
        return println!("  no .cr3 in {OUT}/ — run step 3 first");
    };
    println!("  file: {} ({:.1} MB)", file.display(), fs::metadata(file).map(|m| m.len()).unwrap_or(0) as f64 / 1e6);

    let t = Instant::now();
    match rawler::decode_file(file) {
        Ok(img) => {
            println!("  decoded in {:?}", t.elapsed());
            println!("  make/model : {:?} / {:?}", img.clean_make, img.clean_model);
            println!("  dimensions : {} x {}", img.width, img.height);
            println!("  cpp/bps    : {} / {}", img.cpp, img.bps);
            println!("  photometric: {:?}", img.photometric);
            println!("  black/white: {:?} / {:?}", img.blacklevel, img.whitelevel);
        }
        Err(e) => return println!("  DECODE FAILED: {e}  <-- reopens PRD §7 decoder choice"),
    }

    // Repeat decodes: PRF-05 cares about resident growth, not one-shot cost.
    println!("  20 consecutive decodes (watching peak RSS)…");
    let base = peak_rss_kb();
    let t = Instant::now();
    for _ in 0..20 {
        if rawler::decode_file(file).is_err() {
            return println!("  a later decode failed");
        }
    }
    println!(
        "  mean {:?}/decode; peak RSS {} MB -> {} MB (VmHWM)",
        t.elapsed() / 20,
        base / 1024,
        peak_rss_kb() / 1024
    );
}

fn peak_rss_kb() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}

#[allow(dead_code)]
fn unused(_: Result<()>) {}
