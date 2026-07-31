# The camera is plugged in and astroctl cannot claim it

**Governs:** PRD REL-03 · SDD §5.3.1 · Implemented by M2-T02 (`crates/astroctl-drivers/src/gphoto2/gvfs.rs`)
**Applies to:** any node running a desktop session. A headless field node needs none of this.

The symptom is one message from libgphoto2:

```
Could not claim the USB device
```

The camera is connected, switched on, and visible to `lsusb`. Something else is holding it.

---

## 1. Why this has a runbook

This is not a nuisance. It broke REL-03 in testing, and it broke it in the worst possible way.

M2-T01 pulled the USB cable mid-session to exercise the recovery path. After replugging, a fresh
libgphoto2 context failed to find the camera for **80 seconds straight** with the message above.
The camera was fine. `gvfs` — the desktop's virtual filesystem layer — had auto-mounted it on
hotplug and was holding the USB claim exclusively. Releasing the mount restored autodetect in
**108 ms**.

So on a node with a desktop session, REL-03 ("USB disconnect detection and graceful recovery")
fails — not at startup where it is merely confusing, but in the middle of a night, after a cable
knock, at exactly the moment recovery matters. The prevention below is a precondition for REL-03
on such a node, not optional hardening.

---

## 2. Right now: release the mount

The driver diagnoses this itself. When it cannot claim the camera it scans
`/run/user/<uid>/gvfs/` and, if it finds a camera mount, names it and prints the release command,
so the error you get already contains the answer:

```
the camera is connected but could not be claimed (Could not claim the USB device). The desktop
file manager (gvfs) has it mounted at /run/user/1000/gvfs/gphoto2:host=%5Busb%3A005%2C007%5D and
holds the USB claim exclusively. Release that mount and the camera is reachable again immediately:
    gio mount -u "gphoto2://%5Busb%3A005%2C007%5D/"
```

Run the command it gives you. Both the URL-encoded form above and the decoded form
(`gio mount -u "gphoto2://[usb:005,007]/"`) were tested against a real mount and both work.

If **no** gvfs mount was found, the driver says so and lists what else it can be rather than
guessing — the camera asleep or switched off, the body in mass-storage rather than PTP mode,
another program holding the device, or a permissions problem. It does not assert a cause it has
not established.

Closing the file-manager window is **not** enough, and neither is unplugging and replugging: gvfs
grabs the camera again on hotplug. That is the whole problem.

---

## 3. Permanently: close both activation paths

**Masking the systemd user unit alone does not work**, and this is the part worth knowing before
you spend an evening on it. `/usr/lib/systemd/user/gvfs-gphoto2-volume-monitor.service` is
`Type=dbus`, and `/usr/share/dbus-1/services/org.gtk.vfs.GPhoto2VolumeMonitor.service` carries a
direct `Exec=`. D-Bus can therefore start the binary even with the unit masked. Both paths have to
be closed:

```sh
systemctl --user mask gvfs-gphoto2-volume-monitor.service

mkdir -p ~/.local/share/dbus-1/services
cat > ~/.local/share/dbus-1/services/org.gtk.vfs.GPhoto2VolumeMonitor.service <<'EOD'
[D-BUS Service]
Name=org.gtk.vfs.GPhoto2VolumeMonitor
Exec=/bin/false
EOD
```

Then log out and back in, or reboot — a running volume monitor keeps working until its session
ends.

Both steps are **user-level: no root required**, and both are reversible:

```sh
systemctl --user unmask gvfs-gphoto2-volume-monitor.service
rm ~/.local/share/dbus-1/services/org.gtk.vfs.GPhoto2VolumeMonitor.service
```

Only camera integration is affected. Phones, USB sticks and network shares still mount normally —
this shadows the gphoto2 volume monitor, not gvfs.

---

## 4. What is verified and what is not

Verified on real hardware (M2-T01, `spikes/skywatcher-heq5/FINDINGS.md`):

- gvfs does grab the camera on hotplug, and does so precisely when the recovery path needs it
- releasing the mount restores access immediately (autodetect in 108 ms)
- both unmount command forms work
- the unit is `Type=dbus` and the D-Bus service file carries a direct `Exec=`, so the mask alone
  is provably insufficient and the override directory is user-writable

**Not verified end to end**: that applying section 3 suppresses a hotplug grab. The mechanism is
established, the outcome is not — it was deliberately not applied to the development workstation,
where camera integration in the file manager is wanted. Confirm it on the field node before
relying on it for an unattended session.

---

## 5. Do not apply this to a workstation you use as a workstation

On a machine where you also browse camera contents in a file manager, section 3 removes that. Use
section 2 when it bites instead. The permanent fix is for the field node, which has one job.
