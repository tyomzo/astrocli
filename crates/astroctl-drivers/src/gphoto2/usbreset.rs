//! The USB reset the recovery loop attempts when a fresh context is not enough — SDD §5.3.1.
//!
//! # Why this is the *last* rung and not the first
//!
//! SDD §5.3.1 lists "a USB reset is attempted" as part of the recovery path, and the obvious
//! reading is that it comes first: the device is misbehaving, so reset it. The measurement says
//! otherwise, and the ordering here is the measurement's.
//!
//! M2-T01 pulled the cable mid-stream and then measured what actually recovers
//! (`spikes/gphoto2-r10/FINDINGS.md` step 7). A **fresh `Context` plus autodetect reconnects in
//! 108 ms**. The old handle never does — five retries, all failed. And the one thing that *did*
//! block recovery, for eighty seconds, was not the device at all: gvfs auto-mounted the camera on
//! hotplug and held the USB claim.
//!
//! Two consequences follow, and both are load-bearing:
//!
//! 1. **A reset cannot fix the failure that was actually observed.** gvfs holds a claim through a
//!    device reset; it simply re-mounts. So resetting first would spend seconds of an operator's
//!    night on a mechanism with no measured benefit, ahead of the one with a measured 108 ms.
//! 2. **A reset makes the observed failure *more* likely.** `USBDEVFS_RESET` re-enumerates the
//!    port, and re-enumeration is a hotplug event — the precise trigger that invited gvfs to grab
//!    the camera in the first place. On the claim branch a reset is therefore worse than useless,
//!    which is why [`super::recovery`] never attempts one there.
//!
//! So the reset is kept, because SDD §5.3.1 asks for it and because a genuinely stuck endpoint is
//! a real (if unobserved) failure that nothing else clears — but it is attempted only after the
//! cheap path has failed repeatedly *and* the error says the device is gone rather than claimed.
//!
//! # M2-T04 measured the reset itself, and it is worse than the design assumed
//!
//! The ordering above was reasoned from M2-T01's numbers. M2-T04 then issued a real
//! `USBDEVFS_RESET` at the reference body, twice, and both consequences were bad:
//!
//! 1. **It does not produce the failure it looks like it produces.** The device stayed enumerated
//!    — still in `lsusb`, node still present — while every transfer on the open handle failed
//!    with libgphoto2's `Unspecified error`. That is neither of the two measured strings, which is
//!    what [`super::thread::LinkFault::Unresponsive`] exists to catch.
//! 2. **The second reset took the body off the bus entirely** and it did not come back without a
//!    physical power cycle. That matches what M2-T03 saw after its aborted-bulb run and had to
//!    leave confounded with a flat battery; it now has a second, independent sighting.
//!
//! The conclusion is not to delete this module — SDD §5.3.1 asks for the attempt and a stuck
//! endpoint is still a real failure — but it *sharpens the guard*, and the guard is what the
//! evidence changed:
//!
//! * A reset is attempted **only** when libgphoto2 says the device is not on the bus. On that
//!   branch [`find_camera`] almost always answers [`ResetOutcome::NoDevice`] and nothing happens,
//!   which is the correct outcome — there is no device to reset, only one to wait for.
//! * It is **never** attempted on the `Unresponsive` branch, which is the tempting one: the device
//!   is right there and a reset looks like the obvious remedy. On this body it would trade a
//!   session that a fresh context recovers in 108 ms for one that needs a human and a power cable.
//!
//! In other words the mechanism is present, correct, and deliberately almost unreachable. That is
//! the honest shape for a remedy that has been measured to do more harm than the disease.
//!
//! # The mechanism, and what it needs
//!
//! `USBDEVFS_RESET` (`_IO('U', 20)`, opcode `0x5514`) on the device node
//! `/dev/bus/usb/<bus>/<dev>`. Chosen over the two alternatives:
//!
//! | Mechanism | Privilege | Scope |
//! |---|---|---|
//! | `USBDEVFS_RESET` ioctl | write on the device node — `uaccess` for a seated user, else a udev rule | one device |
//! | `/sys/bus/usb/devices/<id>/authorized` ← 0/1 | root, always | one device, via deauthorize |
//! | `/sys/bus/usb/drivers/usb/{unbind,bind}` | root, always | one device, via the driver |
//!
//! The ioctl is the only one of the three that can succeed **unprivileged**, and the case that
//! matters most — an operator at a desk during a session — is exactly the case where `uaccess`
//! has already granted them the node. The other two need root unconditionally, which the field
//! node running as a systemd system service does not have and should not be given for this.
//!
//! Where the ioctl is refused, the failure is reported once with the remedy (a udev rule, which
//! `docs/ops/camera-usb-claim.md` already tells the operator how to write for this camera) and
//! recovery **continues without it**. A reset that cannot be performed is not a reason to stop
//! trying the thing that was measured to work.
//!
//! # Why the device search is a directory walk
//!
//! libgphoto2 knows the port (`usb:005,007`), but the driver only sees that string on a
//! *successful* open — and by the time a reset is wanted there has not been one. So the device is
//! found the way `lsusb` finds it: by reading `idVendor` under sysfs. Taking the sysfs root as an
//! argument, exactly as [`super::gvfs::find_camera_mount`] takes the gvfs root, is what makes the
//! search testable against a directory a test built rather than only on a machine that happens to
//! have a camera plugged in.

use std::path::{Path, PathBuf};

/// Canon Inc.'s USB vendor id.
///
/// The reference body's, and the only one this driver has been run against. A second supported
/// make adds an entry to [`CAMERA_VENDORS`] rather than a configuration key: an operator cannot
/// be expected to know their camera's USB vendor id, and a wrong one silently resets some other
/// device on the bus.
const CANON: &str = "04a9";

/// The vendors whose devices this driver may reset.
///
/// **An allow-list, not a scan for anything camera-shaped.** The reset is a physical act on a
/// shared bus: getting it wrong means re-enumerating someone's mount, guide camera or keyboard.
/// Matching only vendors this driver actually drives makes the blast radius a decision rather
/// than an accident.
const CAMERA_VENDORS: &[&str] = &[CANON];

/// Where the kernel publishes USB devices.
pub(crate) const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// `USBDEVFS_RESET` — `linux/usbdevice_fs.h`'s `_IO('U', 20)`.
///
/// Composed with rustix's own `_IO` equivalent rather than written as the number the macro
/// expands to (`0x5514`). Both are correct on Linux; this one is *checkable* against the kernel
/// header by eye, because `'U'` and `20` are what the header says, and it cannot silently be
/// wrong on an architecture whose `_IOC` layout differs.
#[cfg(target_os = "linux")]
const USBDEVFS_RESET: rustix::ioctl::Opcode = rustix::ioctl::opcode::none(b'U', 20);

/// A camera on the USB bus, as sysfs describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsbCamera {
    /// The sysfs device id, e.g. `1-3` — what the kernel calls the device.
    pub(crate) id: String,
    /// Bus number, e.g. 5.
    pub(crate) bus: u16,
    /// Device address on that bus, e.g. 7.
    pub(crate) address: u16,
}

impl UsbCamera {
    /// The devfs node to issue the ioctl on, e.g. `/dev/bus/usb/005/007`.
    ///
    /// Three digits, zero-padded, because that is how the kernel names them — `/dev/bus/usb/5/7`
    /// does not exist.
    pub(crate) fn device_node(&self) -> PathBuf {
        PathBuf::from(format!("/dev/bus/usb/{:03}/{:03}", self.bus, self.address))
    }
}

/// Finds the first camera this driver recognises under `sysfs_usb_devices`.
///
/// `None` is the ordinary answer when the camera has been unplugged — which is most of the time
/// a reset is being considered, and is itself informative: there is no device to reset, so the
/// recovery loop should keep waiting for one rather than escalating.
///
/// Entries are considered in sorted order so that a machine with two Canon bodies attached picks
/// the same one on every attempt rather than whichever the filesystem happened to list first.
/// (Opening either is already refused upstream — `CamOps::open` rejects a second camera by name.)
pub(crate) fn find_camera(sysfs_usb_devices: &Path) -> Option<UsbCamera> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(sysfs_usb_devices)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();

    entries.iter().find_map(|path| read_camera(path))
}

/// Reads one sysfs device directory, if it is a camera this driver may reset.
///
/// Interfaces (`1-3:1.0`) and the root hubs (`usb1`) are skipped by construction rather than by
/// name-matching: neither carries the `busnum`/`devnum`/`idVendor` triple, so the parse below
/// simply declines them.
fn read_camera(dir: &Path) -> Option<UsbCamera> {
    let vendor = attribute(dir, "idVendor")?;
    if !CAMERA_VENDORS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&vendor))
    {
        return None;
    }
    Some(UsbCamera {
        id: dir.file_name()?.to_string_lossy().into_owned(),
        bus: attribute(dir, "busnum")?.parse().ok()?,
        address: attribute(dir, "devnum")?.parse().ok()?,
    })
}

/// One sysfs attribute, trimmed. `None` if it is absent or unreadable.
fn attribute(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .map(|value| value.trim().to_owned())
}

/// What came of asking the kernel to reset the camera.
///
/// Three outcomes rather than a `Result<(), _>` because the recovery loop treats them
/// differently, and flattening the middle one into an error would make "the camera is not plugged
/// in" look like a failure of the reset rather than the answer to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResetOutcome {
    /// The ioctl succeeded. The device has re-enumerated.
    Reset {
        /// Which device, for the log line.
        node: PathBuf,
    },
    /// No camera is on the bus at all — nothing to reset, and not an error.
    NoDevice,
    /// The device is there and the kernel refused. Carries the operator-facing reason.
    Refused(String),
}

/// Asks the kernel to reset the camera, if one is attached.
///
/// **Blocking, and deliberately so.** It opens a device node and issues one ioctl; the call is
/// microseconds of syscall and then however long the device takes to re-enumerate, which the
/// kernel does not make us wait for. It is called from the recovery loop through
/// `spawn_blocking`, like every other filesystem touch on that path.
pub(crate) fn reset_camera(sysfs_usb_devices: &Path) -> ResetOutcome {
    let Some(camera) = find_camera(sysfs_usb_devices) else {
        return ResetOutcome::NoDevice;
    };
    reset_device(&camera)
}

/// Issues `USBDEVFS_RESET` on one device node.
#[cfg(target_os = "linux")]
fn reset_device(camera: &UsbCamera) -> ResetOutcome {
    use std::os::fd::AsFd;

    let node = camera.device_node();
    // Write access is what the ioctl needs; `O_WRONLY` rather than `O_RDWR` because nothing is
    // read back, and asking for less is what lets a narrower udev rule work.
    let file = match std::fs::OpenOptions::new().write(true).open(&node) {
        Ok(file) => file,
        Err(error) => return ResetOutcome::Refused(refusal(&node, &error.to_string())),
    };

    // SAFETY: `USBDEVFS_RESET` is a `_IO` opcode — it takes no argument and writes nothing back,
    // which is exactly what `NoArg` encodes. The file descriptor is a usbfs device node, which is
    // the only thing this opcode is defined for and which `find_camera` established by reading
    // the device out of sysfs. The call therefore reads and writes no memory of ours at all; its
    // effect is entirely on the device.
    let called = unsafe {
        rustix::ioctl::ioctl(file.as_fd(), rustix::ioctl::NoArg::<USBDEVFS_RESET>::new())
    };

    match called {
        Ok(()) => ResetOutcome::Reset { node },
        Err(error) => ResetOutcome::Refused(refusal(&node, &error.to_string())),
    }
}

/// A machine without usbfs cannot be asked to reset anything.
///
/// Not `unimplemented!`: this driver is built and tested on developer machines, and a
/// non-Linux build should compile and report honestly rather than panic the first time a camera
/// misbehaves.
#[cfg(not(target_os = "linux"))]
fn reset_device(camera: &UsbCamera) -> ResetOutcome {
    ResetOutcome::Refused(format!(
        "a USB reset of {} needs Linux usbfs, which this build does not have",
        camera.device_node().display()
    ))
}

/// The message an operator reads when the kernel would not let us reset the camera.
///
/// Names the node, the reason and the fix. A bare `Permission denied` would be true and useless:
/// the reader's next question is always "denied to whom, and how do I grant it", and the answer
/// is a udev rule the project already documents writing for this exact camera.
fn refusal(node: &Path, error: &str) -> String {
    format!(
        "could not reset the camera at {} ({error}). A USB reset needs write access to that \
         device node — a desktop session normally has it through udev's `uaccess`, a systemd \
         service does not. See docs/ops/camera-usb-claim.md for the udev rule. Recovery \
         continues without the reset.",
        node.display(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{find_camera, reset_camera, ResetOutcome, UsbCamera};

    /// Builds a directory shaped like `/sys/bus/usb/devices`.
    ///
    /// The entries are the real ones from a machine with the reference camera on it: a root hub,
    /// an interface directory, an unrelated device, and the camera.
    struct Sysfs(PathBuf);

    impl Sysfs {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("astroctl-usbreset-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the sysfs root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Adds a device directory with the given attributes.
        fn device(&self, id: &str, attributes: &[(&str, &str)]) -> &Self {
            let dir = self.0.join(id);
            std::fs::create_dir_all(&dir).expect("creates a device directory");
            for (name, value) in attributes {
                // A trailing newline, because that is how sysfs writes every attribute and a
                // parser that only works without one works only in this test.
                std::fs::write(dir.join(name), format!("{value}\n")).expect("writes an attribute");
            }
            self
        }

        /// The camera, as the reference machine reported it.
        fn with_the_reference_camera(&self) -> &Self {
            self.device(
                "1-3",
                &[
                    ("idVendor", "04a9"),
                    ("idProduct", "32f6"),
                    ("busnum", "5"),
                    ("devnum", "7"),
                ],
            )
        }

        /// The entries a real sysfs root has that are not devices with vendors.
        fn with_the_usual_clutter(&self) -> &Self {
            // A root hub: has a vendor, but Linux Foundation's, not a camera's.
            self.device(
                "usb1",
                &[("idVendor", "1d6b"), ("busnum", "1"), ("devnum", "1")],
            );
            // An interface directory: no `idVendor` at all.
            self.device("1-3:1.0", &[("bInterfaceClass", "06")]);
            self
        }
    }

    impl Drop for Sysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_camera_is_found_among_the_hubs_and_interfaces_a_real_bus_has() {
        let sysfs = Sysfs::new("found");
        sysfs.with_the_usual_clutter().with_the_reference_camera();

        let camera = find_camera(sysfs.path()).expect("the camera is on the bus");
        assert_eq!(
            camera,
            UsbCamera {
                id: "1-3".to_owned(),
                bus: 5,
                address: 7,
            }
        );
    }

    #[test]
    fn the_device_node_is_zero_padded_the_way_the_kernel_names_it() {
        // `/dev/bus/usb/5/7` does not exist; `/dev/bus/usb/005/007` does. Getting this wrong
        // turns every reset into `No such file or directory` and the reason would not be obvious
        // from the message.
        let camera = UsbCamera {
            id: "1-3".to_owned(),
            bus: 5,
            address: 7,
        };
        assert_eq!(
            camera.device_node(),
            Path::new("/dev/bus/usb/005/007"),
            "the kernel pads bus and device numbers to three digits"
        );
    }

    #[test]
    fn a_bus_with_no_camera_on_it_yields_nothing_rather_than_the_first_device() {
        // The common case when a reset is being considered at all: the cable is out. Answering
        // with *some* device would reset a stranger's hardware.
        let sysfs = Sysfs::new("empty");
        sysfs.with_the_usual_clutter();

        assert_eq!(find_camera(sysfs.path()), None);
        assert_eq!(reset_camera(sysfs.path()), ResetOutcome::NoDevice);
    }

    #[test]
    fn a_device_from_another_vendor_is_not_reset() {
        // The allow-list, asserted. A scan for "anything that looks like a camera" would happily
        // re-enumerate the mount's serial adapter on the same hub.
        let sysfs = Sysfs::new("stranger");
        sysfs.device(
            "2-1",
            &[
                ("idVendor", "1a86"),
                ("idProduct", "7523"),
                ("busnum", "2"),
                ("devnum", "4"),
            ],
        );
        assert_eq!(find_camera(sysfs.path()), None);
    }

    #[test]
    fn a_half_written_device_directory_is_skipped_rather_than_guessed_at() {
        // sysfs is a live filesystem: a device being enumerated while this reads it has some
        // attributes and not others. Parsing partial data into a bus and address of zero would
        // aim the reset at `/dev/bus/usb/000/000`.
        let sysfs = Sysfs::new("partial");
        sysfs.device("1-3", &[("idVendor", "04a9")]);
        assert_eq!(find_camera(sysfs.path()), None);
    }

    #[test]
    fn a_missing_sysfs_root_is_not_a_panic() {
        // A container without `/sys` mounted, which is how the field node's own image runs its
        // tests. There is nothing to reset and that is the answer, not a crash.
        assert_eq!(
            reset_camera(Path::new("/nonexistent/sys/bus/usb/devices")),
            ResetOutcome::NoDevice
        );
    }

    #[test]
    fn the_refusal_message_names_the_node_the_reason_and_the_fix() {
        let message = super::refusal(Path::new("/dev/bus/usb/005/007"), "Permission denied");
        assert!(message.contains("/dev/bus/usb/005/007"), "{message}");
        assert!(message.contains("Permission denied"), "{message}");
        // The operator's next question, answered in the message rather than in a wiki.
        assert!(message.contains("udev"), "{message}");
        assert!(message.contains("camera-usb-claim.md"), "{message}");
        // And the reassurance that matters most: a refused reset does not end the recovery.
        assert!(message.contains("Recovery continues"), "{message}");
    }
}
