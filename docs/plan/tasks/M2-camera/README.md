# M2 — Real Camera (Canon EOS R10)

**Goal:** replace `SimulatorCamera` with `CanonGPhoto2Camera` behind the unchanged `Camera`
trait. Nothing outside `astroctl-drivers` (plus the M1-T09 decoder gaining a CR3 variant)
changes. Requires the physical R10 on USB — desk work, no sky needed.

**Exit criteria (IMP §2/M2):** desk session — real CR3s captured from the PWA including bulb,
preview in UI ≤ 3 s after exposure end, frames transferred to the stack node, cable-pull
recovery works, T-CAM-1 green.

**Risk framing:** M2-T01 is the go/no-go spike on the plan's top risk (gphoto2 crate
coverage — ADD §10). Do not start T02+ before the spike's findings are written down.

## Tasks and order

| Task | Title | Depends on |
|------|-------|-----------|
| M2-T01 | Spike: bulb + CR3 download via gphoto2 crate (report task) | M1 |
| M2-T02 | Camera thread, command channel, connect/settings | T01 findings |
| M2-T03 | Capture + download durability + bulb + CLI fallback | T02 |
| M2-T04 | Live view stream, battery/storage, wedge recovery | T02 |
| M2-T05 | Desk integration: real-camera E2E + CR3 preview + soak | T03, T04 |

Hardware note for agents: tasks T01–T05 need the camera attached; they cannot run in CI.
Each task states what evidence to capture (logs, timings) so results are reviewable.
