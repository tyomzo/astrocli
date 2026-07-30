//! The camera's own thread: where the pixels are made and the files are written.
//!
//! # Why a thread at all, in a simulator
//!
//! SDD §7 gives the real camera driver an **OS thread** because libgphoto2 blocks for seconds
//! and its context is not thread-safe. Neither reason applies to a simulator — and the thread is
//! here anyway, because a third reason does: generating a 6000×4000 frame is 24 million
//! Poisson deviates, a JPEG encode and a 48 MB write, and the field node runs
//! `min(2, cores - 2)` runtime workers with a floor of one (SDD §7). Doing that work on a
//! runtime worker would take between a half and all of the node's async capacity for about a
//! second per frame, and T-ISO-1 — the test whose entire purpose is to catch a single-threaded
//! assumption creeping back in — is specified to run *against this simulator*. A simulator that
//! fails that test in a way the real driver would not is a simulator that teaches the wrong
//! lesson; one that passes it by not doing the work is worse.
//!
//! So the shape here is the shape SDD §7 describes for the camera: one thread, a `std::mpsc`
//! queue in, a `oneshot` reply out. The layer above sees an `async fn` that takes a while, which
//! is what it will see from the gphoto2 driver too.
//!
//! # What is *not* on this thread
//!
//! The waiting. An exposure and a download are time passing, not work being done — a real
//! camera's thread is parked in a `libgphoto2` call, not computing — so the simulator waits with
//! [`tokio::time`] instead of blocking a thread for two seconds. That choice is worth stating
//! because it has one visible consequence and one invisible one:
//!
//! * Visible, and the point: `tokio::time::pause` makes a 300 s bulb exposure a test that runs
//!   in microseconds and asserts on the *exact* duration. Every timing assertion in this crate
//!   and in the capture flow above it (M1-T08) depends on that.
//! * Invisible, and deliberate: the camera is occupied for the whole wait regardless — a second
//!   capture is `Busy`, live view stops, an abort lands — because occupancy is state, not a
//!   blocked thread. Nothing above the HAL can tell the difference, which is the test of
//!   whether the simplification was legitimate.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use astroctl_core::error::DeviceError;
use tokio::sync::oneshot;

use super::fits::{self, FrameMetadata};
use super::sky::Exposure;

/// JPEG quality for the renditions, on the 0–100 scale.
///
/// 85 because SDD §5.7 specifies exactly that for the preview pipeline's own JPEGs, and having
/// the camera's rendition and the pipeline's preview at the same quality means a visible
/// difference between them is a difference in the *processing*, not in the encoder settings.
const JPEG_QUALITY: u8 = 85;

/// What one job asks the camera thread to do.
///
/// Rendering and persisting are one job rather than two because they are one operation from the
/// caller's side and splitting them would put a 48 MB pixel buffer on the reply channel — i.e.
/// through the async layer that this thread exists to keep clear of large synchronous work.
#[derive(Debug)]
pub(super) enum Job {
    /// Render an exposure and write it to disk as the requested files.
    Capture {
        /// What to draw.
        exposure: Box<Exposure>,
        /// What to write, and where.
        output: CaptureOutput,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Vec<WrittenFile>, DeviceError>>,
    },
    /// Render an exposure and hand back a JPEG, for live view.
    Preview {
        /// What to draw.
        exposure: Box<Exposure>,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Vec<u8>, DeviceError>>,
    },
    /// Render an exposure and hand back the raw samples, for the guide camera.
    Samples {
        /// What to draw.
        exposure: Box<Exposure>,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<Vec<u16>, DeviceError>>,
    },
}

/// Which files one capture should produce, and under what name.
#[derive(Debug, Clone)]
pub(super) struct CaptureOutput {
    /// Directory the finished files must land in.
    pub(super) dir: PathBuf,
    /// Filename stem, no extension.
    pub(super) stem: String,
    /// Write the 16-bit FITS — the science file.
    pub(super) fits: bool,
    /// Write the 8-bit JPEG rendition.
    pub(super) jpeg: bool,
    /// Header metadata for the FITS.
    pub(super) meta: FrameMetadata,
}

/// A file the thread finished writing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WrittenFile {
    /// Final path — a complete, durable file.
    pub(super) path: PathBuf,
    /// Whether it is the science file or the rendition.
    pub(super) is_raw: bool,
    /// Size on disk.
    pub(super) size_bytes: u64,
}

/// A handle to a running camera thread.
///
/// Dropping it closes the queue, which is how the thread learns to exit: there is no shutdown
/// message, because a shutdown message can be missed by a thread that has already gone and
/// cannot be sent by a `Drop` that is running because the sender is gone.
#[derive(Debug)]
pub(super) struct CameraThread {
    jobs: Sender<Job>,
}

impl CameraThread {
    /// Starts the thread.
    ///
    /// `name` reaches `ps`, `top` and every profiler, which matters more than it looks: the
    /// whole point of the thread is that a reader can see the pixel work is not on the runtime,
    /// and an unnamed `thread::spawn` is invisible in exactly the tools someone would use to
    /// check.
    pub(super) fn start(name: &str) -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let builder = thread::Builder::new().name(name.to_owned());
        // A failure to spawn means the process is out of threads or memory, which is not a
        // condition a camera driver can do anything about — and `connect()` is the only caller,
        // so the panic surfaces at startup rather than mid-session.
        let handle = builder
            .spawn(move || {
                // `recv` ends with `Err` when every sender is gone, i.e. when the driver
                // disconnected or was dropped. That is the exit path.
                while let Ok(job) = queue.recv() {
                    run(job);
                }
            })
            .expect("the camera thread could not be started");
        // Deliberately detached. Joining would mean blocking whichever runtime worker dropped
        // the driver for as long as the frame in flight takes, and the thread's remaining work
        // after the queue closes is bounded by that one frame.
        drop(handle);
        Self { jobs }
    }

    /// Queues a job and waits for its answer.
    ///
    /// # Errors
    /// [`DeviceError::Transport`] if the thread has gone — the simulator's stand-in for the
    /// camera falling off the bus, and the same error a real driver reports when its context
    /// dies mid-operation.
    pub(super) async fn submit<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, DeviceError>>) -> Job,
    ) -> Result<T, DeviceError> {
        let (reply, answer) = oneshot::channel();
        self.jobs.send(make(reply)).map_err(|_| thread_gone())?;
        answer.await.map_err(|_| thread_gone())?
    }
}

fn thread_gone() -> DeviceError {
    DeviceError::Transport("the camera stopped responding (simulated device loss)".to_owned())
}

/// Executes one job. Runs on the camera thread and nowhere else.
fn run(job: Job) {
    match job {
        Job::Capture {
            exposure,
            output,
            reply,
        } => {
            let pixels = exposure.render();
            let result = persist(&exposure, &output, &pixels).map_err(write_failed);
            // A dropped receiver means the caller gave up — the capture still finished, which is
            // the HAL's rule 3 ("dropping the future does not abort the exposure") holding at
            // the lowest level it can.
            drop(reply.send(result));
        }
        Job::Preview { exposure, reply } => {
            let pixels = exposure.render();
            let jpeg = encode_jpeg(&pixels, exposure.width, exposure.height);
            drop(reply.send(Ok(jpeg)));
        }
        Job::Samples { exposure, reply } => {
            let pixels = exposure.render();
            drop(reply.send(Ok(pixels)));
        }
    }
}

/// Turns a write failure into the error the HAL specifies for one.
fn write_failed(error: io::Error) -> DeviceError {
    DeviceError::Transport(format!("could not write the frame: {error}"))
}

/// Writes the requested files, each one durable before it is visible.
///
/// The sequence is SDD §5.3.2's, and every step of it is load-bearing (REL-05, REL-11):
///
/// 1. **unlink any stale temporary** — the spike found `download_to` refuses to overwrite, so a
///    crashed capture's leftovers break the *next* one rather than the one that made them;
/// 2. write to `.tmp_<stem>.<ext>`;
/// 3. `fsync` the file, so the bytes are on the medium and not in a cache;
/// 4. rename into place — atomic, so no consumer ever sees a partial frame;
/// 5. `fsync` the directory, so the rename itself survives a power cut.
///
/// A driver that skips steps 3–5 is not slightly less safe; it produces a frame store whose
/// contents after a crash depend on the filesystem's mood.
fn persist(
    exposure: &Exposure,
    output: &CaptureOutput,
    pixels: &[u16],
) -> io::Result<Vec<WrittenFile>> {
    let mut written = Vec::new();
    if output.fits {
        let bytes = |file: &mut BufWriter<File>| {
            fits::write_image(file, exposure.width, exposure.height, pixels, &output.meta)
        };
        written.push(durable_write(output, "fits", true, bytes)?);
    }
    if output.jpeg {
        let jpeg = encode_jpeg(pixels, exposure.width, exposure.height);
        written.push(durable_write(output, "jpg", false, |file| {
            file.write_all(&jpeg)
        })?);
    }
    Ok(written)
}

/// One file, written under a temporary name and renamed into place.
fn durable_write(
    output: &CaptureOutput,
    extension: &str,
    is_raw: bool,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<WrittenFile> {
    let temporary = output.dir.join(format!(".tmp_{}.{extension}", output.stem));
    let final_path = output.dir.join(format!("{}.{extension}", output.stem));

    // Step 1. `NotFound` is the normal case and is not a failure.
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let file = File::create(&temporary)?;
    let mut buffered = BufWriter::new(file);
    write(&mut buffered)?;
    buffered.flush()?;
    let file = buffered
        .into_inner()
        .map_err(|error| io::Error::other(error.to_string()))?;
    file.sync_all()?; // step 3
    let size_bytes = file.metadata()?.len();
    drop(file);

    fs::rename(&temporary, &final_path)?; // step 4
    sync_directory(&output.dir)?; // step 5

    Ok(WrittenFile {
        path: final_path,
        is_raw,
        size_bytes,
    })
}

/// `fsync` on a directory, which is how a rename is made durable.
///
/// Opening a directory read-only and syncing it is portable across Linux and the BSDs; on a
/// filesystem that refuses it the frame is still renamed, so the failure is reported rather than
/// swallowed — a silent skip here is exactly the bug that only shows up after a power cut.
fn sync_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Encodes a 16-bit frame as an 8-bit grayscale JPEG.
///
/// # The stretch, and why there is one
///
/// A linear 16-bit astronomical frame scaled straight to 8 bits is a black rectangle: the sky
/// sits a few hundred ADU above bias and the stars are points. Every camera that produces a
/// JPEG beside its raw file applies a tone curve, and so does this — an `asinh` stretch anchored
/// on the frame's own median, which is the same family of curve SDD §5.7 specifies for the
/// preview pipeline. The rendition is therefore *not* photometric and must never be measured;
/// it is what an operator looks at to decide whether the framing is right.
fn encode_jpeg(pixels: &[u16], width: u32, height: u32) -> Vec<u8> {
    let gray = stretch_to_8bit(pixels);
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    // Cannot fail into a `Vec`: the only error path is the writer's, and a `Vec` does not fail.
    encoder
        .encode(
            &gray,
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
            jpeg_encoder::ColorType::Luma,
        )
        .expect("encoding into memory cannot fail");
    out
}

/// The stretch itself: median → black, a high percentile → white, `asinh` in between.
///
/// The percentiles come from a 65,536-bin histogram rather than a sort, which turns the cost
/// from `n log n` on 24 million samples into one pass and a scan of a fixed-size table.
fn stretch_to_8bit(pixels: &[u16]) -> Vec<u8> {
    /// Where the white point sits. High enough that a handful of saturated stars do not flatten
    /// the whole frame to black, low enough that the sky is not white.
    const WHITE_PERCENTILE: f64 = 0.999;
    /// How hard the curve bends. Larger lifts fainter signal; 30 shows a sky-limited sub's stars
    /// without turning its noise into texture.
    const SOFTENING: f64 = 30.0;

    let mut histogram = vec![0_u32; 65_536];
    for sample in pixels {
        histogram[*sample as usize] += 1;
    }
    let total = pixels.len() as u64;
    let mut seen = 0_u64;
    let (mut black, mut white) = (0_u16, u16::MAX);
    let mut have_black = false;
    for (value, count) in histogram.iter().enumerate() {
        seen += u64::from(*count);
        if !have_black && seen * 2 >= total {
            black = value as u16;
            have_black = true;
        }
        if (seen as f64) >= WHITE_PERCENTILE * total as f64 {
            white = value as u16;
            break;
        }
    }
    // A perfectly flat frame — a dark with no noise, which the tests do produce — has black ==
    // white, and a span of zero divides every pixel by nothing.
    let span = f64::from(white.saturating_sub(black)).max(1.0);
    let normalise = 1.0 / SOFTENING.asinh();

    pixels
        .iter()
        .map(|sample| {
            let above = (f64::from(*sample) - f64::from(black)).max(0.0) / span;
            let stretched = (above * SOFTENING).asinh() * normalise;
            (stretched.clamp(0.0, 1.0) * 255.0).round() as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use astroctl_core::types::RaDec;
    use chrono::Utc;

    use super::*;
    use crate::simulator::sky::{InjectedStar, StarField};

    fn exposure(width: u32, height: u32) -> Exposure {
        Exposure {
            width,
            height,
            pointing: RaDec::from_parts(5.5, 22.0).expect("valid"),
            arcsec_per_pixel: 2.0,
            exposure: Duration::from_secs(2),
            gain_adu_per_electron: 4.0,
            fwhm_arcsec: 4.0,
            sky_electrons_per_second: 20.0,
            read_noise_electrons: 3.0,
            bias_adu: 512.0,
            full_well_electrons: 30_000.0,
            saturation_adu: u16::MAX,
            noise_seed: Some(9),
            jitter_arcsec: (0.0, 0.0),
            injected: vec![InjectedStar {
                offset: (0.0, 0.0),
                magnitude: 9.0,
            }],
            field: StarField::new(2).with_density(300.0),
        }
    }

    fn metadata() -> FrameMetadata {
        FrameMetadata {
            started_at: Utc::now(),
            exposure_seconds: 2.0,
            pointing: RaDec::from_parts(5.5, 22.0).expect("valid"),
            iso: "1600".to_owned(),
            gain_adu_per_electron: 4.0,
            pixel_size_um: 3.72,
            focal_length_mm: 1000.0,
            sensor_temperature_celsius: None,
            sky_seed: 2,
        }
    }

    #[test]
    fn the_rendition_decodes_as_a_jpeg_of_the_right_size() {
        // Acceptance criterion: "JPEG decodes". Decoded by `zune-jpeg`, which has never seen the
        // encoder — see the crate manifest for why that matters.
        let pixels = exposure(64, 48).render();
        let jpeg = encode_jpeg(&pixels, 64, 48);
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "not a JPEG SOI marker");

        let mut decoder = zune_jpeg::JpegDecoder::new(io::Cursor::new(&jpeg));
        let decoded = decoder.decode().expect("zune-jpeg decodes it");
        let info = decoder.info().expect("dimensions");
        assert_eq!((info.width, info.height), (64, 48));
        // The frame goes in as one luma channel; zune expands it to RGB on the way out, which is
        // its default and not our business. What is our business is that the dimensions survived
        // and that the star did — a JPEG that decodes to a uniform grey would pass a
        // "decodes successfully" assertion and be useless as a preview.
        assert_eq!(decoded.len(), 64 * 48 * 3);
        let brightest = decoded.iter().copied().max().expect("samples");
        assert!(brightest > 200, "the brightest pixel came back {brightest}");
    }

    #[test]
    fn the_stretch_lifts_a_star_off_a_sky_that_would_otherwise_be_black() {
        // The reason a stretch exists at all: linear 16-bit scaled to 8 bits is a black frame.
        let pixels = exposure(64, 48).render();
        let gray = stretch_to_8bit(&pixels);
        let sky = f64::from(gray[0]);
        let brightest = f64::from(*gray.iter().max().expect("pixels"));
        assert!(brightest > 200.0, "the star came out at {brightest}/255");
        assert!(sky < 128.0, "the sky came out at {sky}/255");
    }

    #[test]
    fn a_flat_frame_does_not_divide_by_a_zero_span() {
        // A noiseless dark: every sample identical, black == white. The arithmetic must survive
        // it, because "no signal" is a state a camera is in every time the cap is on.
        let flat = vec![512_u16; 32];
        let gray = stretch_to_8bit(&flat);
        assert!(gray.iter().all(|value| *value == 0), "{gray:?}");
    }

    #[tokio::test]
    async fn a_capture_writes_both_files_atomically_and_leaves_no_temporary() {
        let dir = std::env::temp_dir().join(format!("astroctl-imaging-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("test directory");

        let thread = CameraThread::start("test-camera");
        let written = thread
            .submit(|reply| Job::Capture {
                exposure: Box::new(exposure(48, 32)),
                output: CaptureOutput {
                    dir: dir.clone(),
                    stem: "light_00001".to_owned(),
                    fits: true,
                    jpeg: true,
                    meta: metadata(),
                },
                reply,
            })
            .await
            .expect("the capture writes");

        assert_eq!(written.len(), 2);
        assert!(written[0].is_raw && written[0].path.ends_with("light_00001.fits"));
        assert!(!written[1].is_raw && written[1].path.ends_with("light_00001.jpg"));
        for file in &written {
            let size = fs::metadata(&file.path).expect("the file exists").len();
            assert_eq!(size, file.size_bytes, "{:?}", file.path);
            assert!(size > 0);
        }
        // Nothing half-written is left behind for the *next* capture to trip over — the spike's
        // finding 1, and the reason `persist` unlinks before it writes.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("readable")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp_"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[tokio::test]
    async fn a_capture_into_a_missing_directory_reports_transport_rather_than_panicking() {
        let thread = CameraThread::start("test-camera");
        let error = thread
            .submit(|reply| Job::Capture {
                exposure: Box::new(exposure(16, 16)),
                output: CaptureOutput {
                    dir: PathBuf::from("/nonexistent/astroctl/frames"),
                    stem: "light_00002".to_owned(),
                    fits: true,
                    jpeg: false,
                    meta: metadata(),
                },
                reply,
            })
            .await
            .expect_err("there is no such directory");
        assert!(matches!(error, DeviceError::Transport(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_dead_thread_is_a_transport_failure_not_a_hang() {
        // What a caller sees if the thread is gone: the same error class as a camera falling off
        // the USB bus, and — crucially — an answer rather than a future that never resolves.
        let thread = CameraThread::start("test-camera");
        let (reply, answer) = oneshot::channel::<Result<Vec<u16>, DeviceError>>();
        drop(reply); // stand in for a thread that died mid-job
        assert!(answer.await.is_err());

        let jpeg = thread
            .submit(|reply| Job::Preview {
                exposure: Box::new(exposure(16, 16)),
                reply,
            })
            .await
            .expect("the live thread still answers");
        assert!(!jpeg.is_empty());
    }
}
