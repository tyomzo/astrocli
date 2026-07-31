//! Why libgphoto2 could not claim the camera — and what the operator should do about it.
//!
//! # The failure this exists for
//!
//! A desktop `gvfs` auto-mounts any camera it sees and holds the USB claim **exclusively**.
//! libgphoto2 then reports `Could not claim the USB device` and nothing else: no mention of gvfs,
//! no process name, no suggestion. An operator reading that message has no path from it to the
//! fix, and the fix is one command.
//!
//! M2-T01 met this for real and then reproduced it deliberately
//! (`spikes/skywatcher-heq5/FINDINGS.md`, `spikes/gphoto2-r10/FINDINGS.md`). It is not a
//! cosmetic problem: after a cable replug, a fresh context failed for **80 seconds straight**
//! with that message because gvfs had grabbed the device on hotplug, and releasing the mount
//! restored autodetect in 108 ms. REL-03 ("USB disconnect detection and graceful recovery") is a
//! Must, and on a field node running a desktop session it fails — in the middle of a night,
//! after a cable knock, exactly when recovery matters.
//!
//! # What this module promises, and what it refuses to promise
//!
//! It **names a cause only when it has found one.** If a gphoto2 gvfs mount is present, that is
//! the cause and the message says so and gives the release command. If none is present, the
//! message lists the causes that remain rather than asserting the most likely one — an error that
//! confidently blames the wrong thing costs more than one that says "it is one of these four".
//!
//! It is also deliberately *diagnostic only*. Nothing here unmounts anything. Releasing another
//! session's mount is the operator's call, and a driver that silently tore down a desktop mount
//! would be a surprising thing to have installed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The `gvfs` mount directory prefix for a camera, e.g.
/// `gphoto2:host=%5Busb%3A005%2C007%5D`.
///
/// gvfs names one directory per mount under the runtime gvfs root, and the scheme prefix is what
/// distinguishes a camera mount from an SFTP or MTP one. Matching the prefix rather than the
/// whole name is the point: the host part is the device address and is different every time.
const GPHOTO2_MOUNT_PREFIX: &str = "gphoto2:host=";

/// A gvfs mount holding a camera.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GvfsCameraMount {
    /// The mount directory, e.g. `/run/user/1000/gvfs/gphoto2:host=%5Busb%3A005%2C007%5D`.
    pub(crate) dir: PathBuf,
    /// The `host=` value verbatim, e.g. `%5Busb%3A005%2C007%5D`.
    ///
    /// Kept URL-encoded exactly as gvfs wrote it. The spike tested both the encoded and the
    /// decoded form of the unmount command and found both work, so passing through what is
    /// already on disk avoids a decode step that could only introduce a bug.
    pub(crate) host: String,
}

impl GvfsCameraMount {
    /// The command that releases this mount.
    pub(crate) fn release_command(&self) -> String {
        format!("gio mount -u \"gphoto2://{}/\"", self.host)
    }
}

/// Where gvfs puts its mounts for the user running this process.
///
/// `XDG_RUNTIME_DIR` first because it is the definition — `/run/user/<uid>` is merely what it is
/// set to on every system we have seen. Falling back to the uid keeps the check working for a
/// process started without a session environment (a systemd unit), which is precisely how the
/// field node runs.
pub(crate) fn gvfs_root() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("gvfs");
    }
    // `/proc/self` is owned by the process's real uid, which is the one gvfs would have used.
    // Reading it avoids a `libc` dependency for a single `getuid()`.
    let uid = std::fs::metadata("/proc/self")
        .map(|meta| {
            use std::os::unix::fs::MetadataExt as _;
            meta.uid()
        })
        .unwrap_or(0);
    PathBuf::from(format!("/run/user/{uid}/gvfs"))
}

/// Looks for a camera mount under `gvfs_root`.
///
/// Takes the root as an argument rather than calling [`gvfs_root`] so the scan is testable
/// against a directory a test built — the alternative is a function that can only be exercised
/// on a machine that happens to have a camera mounted, i.e. never in CI.
pub(crate) fn find_camera_mount(gvfs_root: &Path) -> Option<GvfsCameraMount> {
    // A missing root is the normal case on a headless node: no gvfs, no directory. Not an error,
    // and not worth distinguishing from "root exists and is empty" — both mean "gvfs is not the
    // cause here".
    let entries = std::fs::read_dir(gvfs_root).ok()?;
    entries.flatten().find_map(|entry| {
        let name = entry.file_name();
        let host = name
            .to_str()
            .and_then(|name| name.strip_prefix(GPHOTO2_MOUNT_PREFIX))?;
        Some(GvfsCameraMount {
            dir: entry.path(),
            host: host.to_owned(),
        })
    })
}

/// Turns `Could not claim the USB device` into something an operator can act on.
///
/// `detail` is libgphoto2's own message, kept because it is what a bug report will be searched
/// for. Everything after it is ours.
pub(crate) fn explain_claim_failure(detail: &str, gvfs_root: &Path) -> String {
    let mut message = format!("the camera is connected but could not be claimed ({detail}). ");

    if let Some(mount) = find_camera_mount(gvfs_root) {
        // A cause we have actually established, so it is stated as one.
        let _ = write!(
            message,
            "The desktop file manager (gvfs) has it mounted at {} and holds the USB claim \
             exclusively. Release that mount and the camera is reachable again immediately:\n    \
             {}\nTo stop this recurring — it also grabs the camera on replug, which breaks \
             recovery mid-session — see docs/ops/camera-usb-claim.md.",
            mount.dir.display(),
            mount.release_command(),
        );
        return message;
    }

    // No cause established. List, do not guess. Ordered by how often each one is the answer.
    let _ = write!(
        message,
        "No gvfs camera mount is holding it (checked {}), so it is one of these:\n    \
         - the camera is switched off, asleep, or its mode dial moved since it was plugged in\n    \
         - the camera is in mass-storage mode rather than PTP\n    \
         - another program already has it — a second astroctl, or a photo importer\n    \
         - this user may not open the USB device (udev rule, or membership of plugdev)",
        gvfs_root.display(),
    );
    message
}

/// Whether a libgphoto2 error text is the exclusive-claim failure this module explains.
///
/// The two USB failures are **distinguishable**, which M2-T01 established by pulling the cable:
/// a missing camera is `Could not find the requested device on the USB port`, while a camera
/// present but held by something else is `Could not claim the USB device`. Branching on that is
/// what lets the driver say "unplugged" and "something else has it" as different things instead
/// of surfacing one opaque message for both — M2-T04's recovery path depends on the same split.
pub(crate) fn is_claim_failure(detail: &str) -> bool {
    detail.contains("claim")
}

/// Whether a libgphoto2 error text is the *device is not there* failure — the other half of the
/// pair [`is_claim_failure`] documents.
///
/// The literal string the spike recorded when the cable was pulled mid-stream is `Could not find
/// the requested device on the USB port`. Matched on `find the requested device` rather than on
/// the whole sentence because the port clause varies with the transport and libgphoto2's wording
/// is not a documented interface — but matched on *more* than one word, unlike
/// [`is_claim_failure`], because "find" alone appears in unrelated messages ("Could not find the
/// requested file") and a recovery loop that mistook a missing *file* for a missing *camera*
/// would tear down a working link.
///
/// M2-T04 uses this to branch the recovery loop's operator message: a vanished device is a cable,
/// a power switch or a flat battery, and telling that operator to go hunting for a gvfs mount
/// sends them to the wrong place at the worst possible time.
pub(crate) fn is_device_missing(detail: &str) -> bool {
    detail.contains("find the requested device")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{explain_claim_failure, find_camera_mount, is_claim_failure, is_device_missing};

    /// Builds a directory that looks like a gvfs root with the given mount names in it.
    fn gvfs_root_containing(names: &[&str]) -> tempdir::TempDir {
        let dir = tempdir::TempDir::new();
        for name in names {
            std::fs::create_dir(dir.path().join(name)).expect("creates a mount directory");
        }
        dir
    }

    /// The exact mount name the spike observed, so the parser is tested against the real thing
    /// rather than against a tidied-up version of it.
    const OBSERVED_MOUNT: &str = "gphoto2:host=%5Busb%3A005%2C007%5D";

    #[test]
    fn the_camera_mount_is_found_among_the_other_mounts_a_desktop_has() {
        let root = gvfs_root_containing(&[
            "sftp:host=build.example",
            OBSERVED_MOUNT,
            "mtp:host=%5Busb%3A001%2C013%5D",
        ]);
        let mount = find_camera_mount(root.path()).expect("finds the camera mount");
        assert_eq!(mount.host, "%5Busb%3A005%2C007%5D");
        assert_eq!(
            mount.release_command(),
            // Verbatim from spikes/skywatcher-heq5/FINDINGS.md, where both this form and the
            // URL-decoded one were tested against the real mount and both worked.
            "gio mount -u \"gphoto2://%5Busb%3A005%2C007%5D/\""
        );
    }

    #[test]
    fn a_root_with_no_camera_mount_and_a_root_that_does_not_exist_both_report_nothing() {
        let root = gvfs_root_containing(&["sftp:host=build.example"]);
        assert_eq!(find_camera_mount(root.path()), None);
        // The headless field node: no gvfs, so no directory. Must not panic, must not be an
        // error — it is the *good* state.
        assert_eq!(
            find_camera_mount(Path::new("/nonexistent/run/user/1000/gvfs")),
            None
        );
    }

    #[test]
    fn a_found_mount_is_named_with_the_command_that_releases_it() {
        let root = gvfs_root_containing(&[OBSERVED_MOUNT]);
        let message = explain_claim_failure("Could not claim the USB device", root.path());

        // libgphoto2's own words survive, because that is what gets searched for.
        assert!(
            message.contains("Could not claim the USB device"),
            "{message}"
        );
        // The cause, the evidence for it, and the fix.
        assert!(message.contains("gvfs"), "{message}");
        assert!(message.contains(OBSERVED_MOUNT), "{message}");
        assert!(
            message.contains("gio mount -u \"gphoto2://%5Busb%3A005%2C007%5D/\""),
            "{message}"
        );
        // And where the permanent fix lives, since releasing the mount does not stop it coming
        // back on the next replug.
        assert!(
            message.contains("docs/ops/camera-usb-claim.md"),
            "{message}"
        );
    }

    #[test]
    fn with_no_mount_found_the_message_lists_causes_instead_of_picking_one() {
        let root = gvfs_root_containing(&[]);
        let message = explain_claim_failure("Could not claim the USB device", root.path());

        // The one thing this must not do is blame gvfs when it has not found a gvfs mount.
        assert!(
            !message.contains("gio mount -u"),
            "a release command implies a mount was found: {message}"
        );
        for cause in ["asleep", "mass-storage", "another program", "udev"] {
            assert!(message.contains(cause), "missing `{cause}` in: {message}");
        }
    }

    #[test]
    fn a_missing_camera_is_not_diagnosed_as_a_claim_failure() {
        // The distinction M2-T01 established by pulling the cable. Getting this backwards would
        // send an operator hunting for a gvfs mount when the camera is simply unplugged.
        assert!(is_claim_failure("Could not claim the USB device"));
        assert!(!is_claim_failure(
            "Could not find the requested device on the USB port"
        ));
    }

    #[test]
    fn the_two_usb_failures_are_recognised_as_different_things() {
        // Both halves of the pair, and both negatives — the recovery loop branches on exactly
        // this and gives two different operator messages, so a predicate that answered `true` to
        // both would collapse the branch it exists to make.
        let gone = "Could not find the requested device on the USB port";
        let claimed = "Could not claim the USB device";

        assert!(is_device_missing(gone));
        assert!(!is_device_missing(claimed));
        assert!(is_claim_failure(claimed));
        assert!(!is_claim_failure(gone));
    }

    #[test]
    fn a_missing_file_is_not_a_missing_camera() {
        // Why the match is on three words rather than on `find`. libgphoto2 says "Could not find
        // the requested file" when a download names a frame the body has already overwritten —
        // a per-frame failure. A recovery loop that read that as a vanished device would abandon
        // a perfectly healthy camera thread and lose the rest of the sequence with it.
        assert!(!is_device_missing("Could not find the requested file"));
        assert!(!is_device_missing("File exists: /tmp/light_1.cr3"));
    }

    /// A directory that deletes itself, so these tests leave nothing behind.
    ///
    /// Local rather than a dependency: this crate has no `tempfile` dev-dependency and one
    /// three-line helper is a smaller thing to own than a new entry in the dependency matrix.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub(super) struct TempDir(PathBuf);

        impl TempDir {
            pub(super) fn new() -> Self {
                let unique = format!(
                    "astroctl-gvfs-test-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                );
                let path = std::env::temp_dir().join(unique.replace(['(', ')', ' '], ""));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("creates the temp root");
                Self(path)
            }

            pub(super) fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
