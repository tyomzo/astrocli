# M2-T04 — Live view stream, battery/storage, wedge recovery

**Milestone:** M2 · **Depends on:** M2-T02 · **Crates:** astroctl-drivers
**Spec:** SDD §5.3.1 (wedge protocol), §5.7 source 1; PRD CAM-05/08, REL-03
**Tests gated:** T-CAM-1

## Objective

The remaining camera surface: live view streaming into the M1 pipeline, battery/storage
monitoring, and the full wedge-recovery protocol (the REL-03 path).

## Scope

- Live view: `LiveViewStart/Stop` on the camera thread; preview frames → watch channel → existing `/ws/liveview` plumbing (M1-T09 consumes unchanged); target ≥ 5 fps on LAN per T01 measured capability (PRF-02)
- Battery/storage polling (60 s + on-demand) → `camera.status` events
- Wedge recovery per SDD §5.3.1: operation-class timeout → thread declared wedged → abandon thread, spawn fresh thread + context, attempt USB reset (usbreset ioctl or unbind/rebind, document chosen mechanism), surface `camera.status: reconnecting` → `connected`; bounded retries then Faulted
- Cable-pull handling informed by T01 findings; disconnect detection → same recovery path

## Acceptance criteria

- [ ] T-CAM-1: capture/settings/live-view session, then induced wedge (cable pull mid-liveview) → automatic recovery to working capture within 30 s, UI shows reconnecting→connected without reload
- [ ] Live view runs 10 min without fps decay or memory growth (watch RSS)
- [ ] Battery percentage matches camera body display ±5 %
