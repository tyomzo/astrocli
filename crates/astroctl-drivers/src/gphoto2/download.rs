//! Getting one frame from the camera onto the disk, durably — SDD §5.3.2.
//!
//! Five steps, in this order, and the order is the design:
//!
//! ```text
//!   1. unlink  <dir>/.tmp_<stem>.<ext>      ← libgphoto2 refuses to overwrite (M2-T01 finding 1)
//!   2. download the camera's file onto it
//!   3. fsync the file                       ← the bytes are now on the platter
//!   4. rename onto <dir>/<stem>.<ext>       ← the frame becomes visible, atomically
//!   5. fsync the directory                  ← the name is now durable too
//! ```
//!
//! Steps 3–5 are what [`Camera::capture`](astroctl_hal::camera::Camera::capture) means by
//! "durable": a consumer scanning the session directory sees either no frame or a complete one,
//! never a torn download, and that stays true across a power cut rather than merely across a
//! crash (REL-05, REL-11).
//!
//! # Step 1 is not defensive tidying
//!
//! `download_to` returns `File exists` rather than truncating — M2-T01 measured it, and SDD
//! §5.3.2 records it. So a crash that leaves a stale `.tmp_` behind does not cost one frame, it
//! costs *every subsequent frame under that name*, permanently, with an error message that
//! describes a filesystem rather than a camera. The unlink is unconditional because the condition
//! under which it matters is precisely the one where nobody is looking.
//!
//! # Where this runs, and why it is not in the backend
//!
//! On the camera thread, blocking, like everything else below the channel — but *above*
//! [`CamOps`], because none of it needs a camera. The unlink, the fsync dance and the naming are
//! filesystem behaviour every `CamOps` implementation would otherwise repeat identically, and
//! putting them here is what lets CI test them against the mock on a machine with no libgphoto2
//! (SDD §5.3.1 obligation 2).

use std::fs;
use std::path::{Path, PathBuf};

use astroctl_core::error::DeviceError;
use astroctl_hal::camera::CapturedFileKind;

use super::ops::{CamOps, RawFileRef};

/// The temporary-name prefix, shared with every other writer in the system.
///
/// The frame store sweeps `.tmp_*` on startup (`astroctl-session`) and the stacking mirror uses
/// the same prefix. A driver inventing its own would leave debris that nothing collects.
const TMP_PREFIX: &str = ".tmp_";

/// The extension used when the camera's own filename carries none.
///
/// Should never fire on a body that names its files at all; it exists so that a nameless file is
/// still landed under *some* name rather than dropped, and `raw` rather than `cr3` because at
/// this point the driver genuinely does not know what it is holding.
const UNKNOWN_EXTENSION: &str = "raw";

/// A file that reached the disk and is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandedFile {
    /// The final path. A complete file by the time this exists.
    pub(crate) path: PathBuf,
    /// What it is, from the camera's own extension.
    pub(crate) kind: CapturedFileKind,
    /// Size on disk, so the caller can journal it without a `stat` (SDD §6).
    pub(crate) size_bytes: u64,
}

/// Downloads one camera-side file into `dir`/`stem` + the camera's own extension, durably.
///
/// The extension comes from the body, not from this driver: the R10 shooting RAW hands back
/// `capt0000.cr3` and the same body shooting RAW+JPEG also hands back a `.jpg`, and a driver that
/// hardcoded `cr3` would name a JPEG `.cr3` and hand it to a raw decoder.
///
/// # Errors
/// `Transport` for any filesystem or transfer failure, with the path named — a download that
/// failed because the session directory is read-only and one that failed because the camera was
/// unplugged are both transport failures, but only one of them is worth walking outside for.
pub(crate) fn land(
    ops: &mut dyn CamOps,
    file: &RawFileRef,
    dir: &Path,
    stem: &str,
) -> Result<LandedFile, DeviceError> {
    let extension = file
        .extension()
        .unwrap_or_else(|| UNKNOWN_EXTENSION.to_owned());
    let temporary = dir.join(format!("{TMP_PREFIX}{stem}.{extension}"));
    let final_path = dir.join(format!("{stem}.{extension}"));

    // Step 1. `NotFound` is the normal case and is not a failure — see the module docs for why
    // the abnormal case is worth this line.
    remove_if_present(&temporary)?;

    // Step 2. On this body the frame is already in libgphoto2's memory by now (see
    // `CamOps::capture`); what this call costs is the write, not the wire.
    let size_bytes = match ops.download(file, &temporary) {
        Ok(size) => size,
        Err(error) => {
            // A failed download must leave nothing behind, or the *retry* meets step 1's trap
            // from the other side: a partial file under the temporary name that the next attempt
            // would have to unlink anyway, and that a directory listing shows in the meantime.
            remove_quietly(&temporary);
            return Err(error);
        }
    };

    // Steps 3–5. Anything failing here leaves a temporary rather than a half-named frame, which
    // is the direction to fail in: `.tmp_` is swept, an incomplete `light_00042.cr3` is not.
    let opened = fs::File::open(&temporary).map_err(|error| transport(&temporary, &error))?;
    opened
        .sync_all()
        .map_err(|error| transport(&temporary, &error))?;
    drop(opened);

    fs::rename(&temporary, &final_path).map_err(|error| transport(&final_path, &error))?;

    // After the rename the frame is *visible*, so a failure from here on cannot simply be
    // returned: the caller would report a failed capture while a complete-looking frame sat in
    // the session directory — and for a RAW+JPEG pair, a lone raw with no JPEG, which is exactly
    // the state this module promises cannot happen. So the name is taken back before the error
    // is. (`sync_directory` fails on EIO, on a writeback error surfaced at fsync, or on a
    // filesystem that refuses fsync on a directory at all.)
    if let Err(error) = sync_directory(dir) {
        remove_quietly(&final_path);
        return Err(error);
    }

    Ok(LandedFile {
        path: final_path,
        kind: file.kind(),
        size_bytes,
    })
}

/// Removes every temporary this capture might have left, ignoring what is not there.
///
/// Called when a capture is abandoned. "Nothing on disk" is a promise the abort contract makes
/// (`Camera::abort_capture`), and a `.tmp_` file is on disk however invisible its name makes it
/// to a frame browser.
pub(crate) fn sweep(dir: &Path, stem: &str, files: &[RawFileRef]) {
    for file in files {
        let extension = file
            .extension()
            .unwrap_or_else(|| UNKNOWN_EXTENSION.to_owned());
        remove_quietly(&dir.join(format!("{TMP_PREFIX}{stem}.{extension}")));
    }
}

/// Deletes a path, treating "it was not there" as success.
fn remove_if_present(path: &Path) -> Result<(), DeviceError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(transport(path, &error)),
    }
}

/// Deletes a path on a cleanup route, where a failure has nowhere useful to go.
fn remove_quietly(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove a temporary capture file; the next capture under this name \
                 will remove it before downloading"
            );
        }
    }
}

/// fsyncs a directory, which is what makes a rename survive a power cut.
fn sync_directory(dir: &Path) -> Result<(), DeviceError> {
    fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| transport(dir, &error))
}

/// A filesystem failure, with the path that failed.
fn transport(path: &Path, error: &std::io::Error) -> DeviceError {
    DeviceError::Transport(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use astroctl_core::error::DeviceError;
    use astroctl_hal::camera::CapturedFileKind;

    use super::super::mock::MockState;
    use super::super::ops::{CamOps, RawFileRef};
    use super::{land, sweep, TMP_PREFIX};

    /// A camera-side file reference.
    fn file(name: &str) -> RawFileRef {
        RawFileRef {
            folder: "/".to_owned(),
            name: name.to_owned(),
        }
    }

    /// A path inside the process's temporary directory, unique to this test.
    ///
    /// Tests in one binary run in parallel, so a shared directory would make two landings race
    /// over one filename and fail in whichever order the scheduler picked.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("astroctl-gpdl-{}-{name}", std::process::id()));
        // A leftover from a previous run would make the "nothing on disk" assertions pass or fail
        // for a reason that has nothing to do with this run.
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("a writable temporary directory");
        dir
    }

    /// A mock `CamOps` and a scratch directory to land into.
    fn fixture(name: &str) -> (std::sync::Arc<MockState>, Box<dyn CamOps>, PathBuf) {
        use super::super::ops::CamOpsFactory;
        let (state, factory) = MockState::new();
        let ops = factory.build().expect("builds");
        (state, ops, scratch_dir(name))
    }

    #[test]
    fn a_landed_frame_is_named_from_the_stem_and_the_bodys_own_extension() {
        let (_state, mut ops, dir) = fixture("named");

        let landed = land(ops.as_mut(), &file("capt0000.cr3"), &dir, "light_00042").expect("lands");

        assert_eq!(landed.path, dir.join("light_00042.cr3"));
        assert_eq!(landed.kind, CapturedFileKind::Raw);
        assert!(landed.path.exists(), "the final file must exist");
        assert!(landed.size_bytes > 0);
        // And no temporary survives a successful landing.
        assert_eq!(temporaries(&dir), Vec::<PathBuf>::new());
    }

    #[test]
    fn the_extension_is_the_bodys_and_it_decides_what_the_file_is_for() {
        // The R10 shooting RAW+JPEG produces both, and the driver must not name the JPEG `.cr3`
        // and hand it to a raw decoder.
        let (_state, mut ops, dir) = fixture("extensions");

        let raw = land(ops.as_mut(), &file("IMG_0001.CR3"), &dir, "light_1").expect("lands");
        let jpeg = land(ops.as_mut(), &file("IMG_0001.JPG"), &dir, "light_1").expect("lands");

        // Upper case on the wire, lower case on the disk — one spelling per session directory.
        assert_eq!(raw.path, dir.join("light_1.cr3"));
        assert_eq!(raw.kind, CapturedFileKind::Raw);
        assert_eq!(jpeg.path, dir.join("light_1.jpg"));
        assert_eq!(jpeg.kind, CapturedFileKind::Jpeg);
    }

    #[test]
    fn a_stale_temporary_is_unlinked_before_every_download() {
        // M2-T01 finding 1, and the reason it is worth a test: `download_to` returns `File
        // exists` rather than truncating, so a stale `.tmp_` left by a crash would make every
        // retry under that name fail permanently.
        let (state, mut ops, dir) = fixture("stale");
        let stale = dir.join(format!("{TMP_PREFIX}light_00042.cr3"));
        std::fs::write(&stale, b"debris from a crashed run").expect("writes the stale temporary");

        land(ops.as_mut(), &file("capt0000.cr3"), &dir, "light_00042")
            .expect("a stale temporary must not fail the download");

        // The mock refuses to overwrite exactly as libgphoto2 does, so reaching this line at all
        // proves the unlink happened; assert the recorded fact too.
        assert!(
            state.download_saw_a_clear_path(),
            "the temporary must be unlinked *before* the download, not after"
        );
    }

    #[test]
    fn a_failed_download_leaves_nothing_behind() {
        let (state, mut ops, dir) = fixture("failed");
        state.fail_download_with("the camera was unplugged mid-transfer");

        let error = land(ops.as_mut(), &file("capt0000.cr3"), &dir, "light_1")
            .expect_err("the download failed");
        assert!(matches!(error, DeviceError::Transport(_)), "{error:?}");

        assert_eq!(
            listing(&dir),
            Vec::<String>::new(),
            "a failed download must leave neither a frame nor a temporary"
        );
    }

    #[test]
    fn the_frame_is_only_ever_visible_under_its_final_name() {
        // The durability claim, from a consumer's point of view: the mock records the path it was
        // handed, and it is never the one a frame browser scans for.
        let (state, mut ops, dir) = fixture("visible");

        land(ops.as_mut(), &file("capt0000.cr3"), &dir, "light_7").expect("lands");

        let written = state.download_destinations();
        assert_eq!(written.len(), 1);
        let temporary = &written[0];
        assert!(
            temporary
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(TMP_PREFIX)),
            "the download must go to a temporary, not to the final name: {temporary:?}"
        );
        assert_ne!(temporary, &dir.join("light_7.cr3"));
    }

    #[test]
    fn sweeping_removes_the_temporaries_an_abandoned_capture_would_have_used() {
        let dir = scratch_dir("sweep");
        let files = [file("capt0000.cr3"), file("capt0000.jpg")];
        for name in ["light_9.cr3", "light_9.jpg"] {
            std::fs::write(dir.join(format!("{TMP_PREFIX}{name}")), b"partial")
                .expect("writes a partial");
        }

        sweep(&dir, "light_9", &files);

        assert_eq!(
            listing(&dir),
            Vec::<String>::new(),
            "`nothing on disk` includes the temporaries"
        );
        // And sweeping again, with nothing there, is not an error.
        sweep(&dir, "light_9", &files);
    }

    #[test]
    fn a_file_the_body_did_not_name_still_lands() {
        let (_state, mut ops, dir) = fixture("unnamed");
        let landed = land(ops.as_mut(), &file("capt0000"), &dir, "light_3").expect("lands");
        assert_eq!(landed.path, dir.join("light_3.raw"));
        // Unknown, so not claimed to be a JPEG — it goes down the science-file path.
        assert_eq!(landed.kind, CapturedFileKind::Raw);
    }

    /// Every entry in a directory, by name.
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

    /// Every temporary in a directory.
    fn temporaries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .expect("reads the directory")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(TMP_PREFIX))
            })
            .collect()
    }
}
