# M2-T04 — Live view stream, battery/storage, wedge recovery

**Milestone:** M2 · **Depends on:** M2-T02 · **Crates:** astroctl-drivers
**Size:** M · **Status:** done (one acceptance criterion partly pending hardware — see below)
**Spec:** SDD §5.3.1 (wedge protocol), §5.7 source 1; PRD CAM-05/08, REL-03
**Tests gated:** T-CAM-1

## Objective

The remaining camera surface: live view streaming into the M1 pipeline, battery/storage
monitoring, and the full wedge-recovery protocol (the REL-03 path).

## Scope

- Live view: `LiveViewStart/Stop` on the camera thread; preview frames → watch channel → existing `/ws/liveview` plumbing (M1-T09 consumes unchanged). **Measured on the R10: 58.5 fps**, comfortably past PRF-02's 5 fps — rate-limit *down* for the link's sake (USB-11), do not chase throughput
- Confirm the M1-T09 capture-pause handling holds against the real 2.08 s stall (spike-measured): live view resumes without a reconnect, no spurious wedge alert, and the wedge detector still fires for a genuine stall
- Battery/storage polling (60 s + on-demand) → `camera.status` events
- Wedge recovery per SDD §5.3.1: operation-class timeout → thread declared wedged → abandon thread, spawn fresh thread + context, attempt USB reset (usbreset ioctl or unbind/rebind, document chosen mechanism), surface `camera.status: reconnecting` → `connected`; bounded retries then Faulted
- Cable-pull handling per the **executed** T01 step 7 (`spikes/gphoto2-r10/FINDINGS.md`). Three measured facts drive the design: (a) physical removal reports `Could not find the requested device on the USB port` while a claim conflict reports `Could not claim the USB device` — **branch on this and give different operator messages**; (b) the old `Camera` handle never recovers, so abandoning the thread and context is mandatory, not a fallback; (c) recovery itself is fast — 108 ms — when nothing else holds the device
- **Guard the recovery path against gvfs.** On replug, a desktop gvfs auto-mount grabbed the camera and blocked recovery for 80 s. Since REL-03 is a Must, the reconnect loop must detect the claim-conflict error, report gvfs by name if a `gphoto2` mount is present, and not silently retry forever — an operator staring at a stalled sequence deserves to be told which process took their camera

## Acceptance criteria

- [x] T-CAM-1 **closed 2026-08-01, real cable pull**: device-gone detected at once, recovery on attempt 5/6, **15.4 s pull-to-recovered** vs the 30 s budget, proof-capture `light_00020` (26.5 MB CR3). Round 1 reproduced the gvfs replug-steal and forced `docs/ops/camera-usb-claim.md`'s permanent fix onto this desktop (mask + D-Bus shadow, verified holding); round 2 sharpened the diagnosis — "Unknown model" during recovery is the USB re-enumeration window itself, ridden out by the bounded retry loop, never a gvfs symptom
  - Recovery loop proved end to end against the mock `CamOps` (`a_wedged_camera_recovers_by_itself_to_a_working_capture`, `recovery_reports_reconnecting_before_it_reports_connected`, `live_view_survives_a_recovery_and_resumes_into_the_same_stream`, and the whole `recovery::tests` module).
  - On hardware the run got as far as reproducing the *claim* branch live and no further — see "Hardware evidence" below. The full pull→recover→capture sequence is pending.
  - **"UI shows reconnecting→connected" is not achievable as specified** — see the §4.3 gap below.
- [ ] Live view runs 10 min without fps decay or memory growth (watch RSS) — `the_ten_minute_live_view_soak` is written and `#[ignore]`d; the mock soak (`a_short_live_view_soak_leaks_nothing_and_does_not_decay`) proves the plumbing holds no backlog and does not decay, which is all a mock can say.
- [x] Battery check closed 2026-08-01, as precisely as the hardware allows: the R10 body shows a segmented meter, not a percentage, so ±5 % cannot be read off it — the driver reported **100 %** and the body showed a **full meter**, which is agreement at the display's own quantisation. Recording the limitation rather than inventing a number

## Hardware evidence (2026-07-31)

The R10 returned to the bus mid-task and left it again before the run finished. What was measured:

- **Live view at the configured rate: PASSED.** 51 frames in 10.2 s = **5.0 fps** against
  `live_view_fps: 5`, mean **193 KB/frame**, worst gap 211 ms, first frame 101 ms (live-view
  startup). PRF-02 (≥ 5 fps) **MET**. Note the frame is *larger* than M2-T01's 133 KB — 193 KB at
  5 fps is 0.94 MB/s, and the pacing is what keeps it there rather than at the body's 58.5 fps,
  which would be **11 MB/s** on this body rather than the 7.8 MB/s the spike computed.
- **The gvfs claim branch: reproduced live, and the diagnosis worked.** The induced USB reset
  re-enumerated the device, gvfs auto-mounted it on hotplug, and the driver refused to connect with
  the full diagnosis — naming the mount path and printing `gio mount -u "gphoto2://Canon_Inc._…/"`.
  Running that exact command returned the camera immediately. This is the failure the spike
  measured at 80 s, met from the other direction and answered.
- **A third USB failure mode, previously unrecorded.** `USBDEVFS_RESET` left the device
  **enumerated** (still in `lsusb`, node present) with a dead session: every preview failed with
  `Unspecified error` and `read_settings` returned `Ok` with every field empty. Neither of M2-T01's
  two strings appears. Drove `LinkFault::Unresponsive` (recognised by repetition) and the
  empty-settings refusal in `backend.rs`.
- **The reset is destructive on this body.** A second reset took the R10 off the USB bus entirely
  and it did not return without a physical power cycle. A second independent sighting of the
  drop-off M2-T03 had to leave confounded with a flat battery — it is *not* only the battery.

### Pending hardware (needs the body back, and a human at the desk)

1. `t_cam_1_…` end to end: pull → reconnecting → connected → working capture. **Prefer the
   physical cable pull** (M2-T05) over the software reset now that the reset is known to knock this
   body off the bus.
2. `the_ten_minute_live_view_soak` — fps decay and RSS flatness over ten minutes.
3. `battery_and_storage_for_the_operator_to_check_against_the_body` — the ±5 % comparison against
   the body's own display, which only a human can make.
4. `an_aborted_bulb_does_not_poison_the_next_one` — **M2-T03's open question**, unanswered. The
   sequence is written and ready: abort a bulb, then take a 10 s bulb on a healthy body. Success
   confirms the buffer-orphan hypothesis; failure points at `eosremoterelease` needing an explicit
   reset to `None` after an early release.

## Spec gaps found

- **§4.3's `CameraStatus` has no `reconnecting` state.** It is `{connected, battery_pct, charging,
  storage_free_mb}` — a two-state boolean — while SDD §5.3.1 and this task's acceptance criterion
  both require the transition to be surfaced as `camera.status: reconnecting`. Adding a field is a
  frozen-contract change (the golden wire fixtures, the PWA mirror), which `tasks/README` rule 2
  says an implementation task does not invent. The driver therefore exposes the state
  (`CanonGPhoto2Camera::link_state`, `watch_link_state`, reachable via
  `CanonGPhoto2CameraFactory::create_gphoto2`) and **publishing it is outstanding work for the
  facade** — either a §4.3 payload change or an `Alert` with a free-string code, which is the
  no-schema-change route `DISK_LOW`/`CLOCK_UNSYNCED` already took.
- **SDD §5.3.1's `LiveViewStart`/`LiveViewStop` sketch cannot be implemented as drawn** — a thread
  looping on previews is a thread not reading its command channel. Corrected in the 1.26.0 note.
- **The spike's two-error-string finding is incomplete**, through no fault of the spike: pulling
  the cable produces one of two texts, and a bus-level reset produces a third state with neither.
