# M2-T04 — Live view stream, battery/storage, wedge recovery

**Milestone:** M2 · **Depends on:** M2-T02 · **Crates:** astroctl-drivers
**Size:** M · **Status:** not started
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

- [ ] T-CAM-1: capture/settings/live-view session, then induced wedge (cable pull mid-liveview) → automatic recovery to working capture within 30 s, UI shows reconnecting→connected without reload
- [ ] Live view runs 10 min without fps decay or memory growth (watch RSS)
- [ ] Battery percentage matches camera body display ±5 %
