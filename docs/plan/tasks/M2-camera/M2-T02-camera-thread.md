# M2-T02 — Camera thread, command channel, connect/settings

**Milestone:** M2 · **Depends on:** M2-T01 findings · **Crates:** astroctl-drivers
**Size:** M · **Status:** not started
**Spec:** SDD §5.3.1 (thread model, CamCmd set); PRD CAM-01/02

## Objective

The `CanonGPhoto2Camera` skeleton: dedicated OS thread owning the gphoto2 context, command
channel with per-operation timeouts, connect/disconnect and settings — the structure every
later operation slots into.

## Scope

- Driver struct implementing `Camera`; spawns the camera thread on `connect`
- `CamCmd` enum + std mpsc → thread; tokio oneshot replies; per-operation-class timeouts per SDD §5.3.1 (config-overridable)
- **Detect and explain the gvfs steal.** A desktop `gvfsd-gphoto2` auto-mount holds the USB claim exclusively and libgphoto2 reports only "Could not claim the USB device", which points nowhere useful. Observed for real during the M2-T01 spike. On that error, check for a gvfs gphoto2 mount and surface an actionable message naming it; document the fix (headless field node, mask the gvfs gphoto2 volume monitor, or a udev rule). A desktop environment silently taking the camera is a guaranteed field failure
- Connect: autodetect (single camera assumed; multiple → error listing), open, read capabilities → `CameraCapabilities` (values from T01's config-tree dump; pixel size etc. from equipment profile config)
- Settings: get/set ISO, shutter, aperture, format via config keys established in T01; `get_available_settings` from the camera's enumerated choices — never a hardcoded list
- Timeout ⇒ wedge protocol *stub*: channel dropped, error surfaced; full respawn logic in T04 (leave the seam + TODO)
- Registry name `"gphoto2"`; simulator remains default in example config until M2-T05

## Acceptance criteria

- [ ] With R10 attached: connect, read settings, change ISO from the API, see it on the camera body
- [ ] All blocking gphoto2 calls verifiably on the camera thread (assert via thread-name in tracing spans)
- [ ] Unit tests for the channel/timeout machinery against a mock `CamOps` (no hardware in CI)
- [ ] Evidence attached: log of a settings round-trip session
