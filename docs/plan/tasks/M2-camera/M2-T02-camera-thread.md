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
- Connect: autodetect (single camera assumed; multiple → error listing), open, read capabilities → `CameraCapabilities` (values from T01's config-tree dump; pixel size etc. from equipment profile config)
- Settings: get/set ISO, shutter, aperture, format via config keys established in T01; `get_available_settings` from the camera's enumerated choices — never a hardcoded list
- Timeout ⇒ wedge protocol *stub*: channel dropped, error surfaced; full respawn logic in T04 (leave the seam + TODO)
- Registry name `"gphoto2"`; simulator remains default in example config until M2-T05

## Acceptance criteria

- [ ] With R10 attached: connect, read settings, change ISO from the API, see it on the camera body
- [ ] All blocking gphoto2 calls verifiably on the camera thread (assert via thread-name in tracing spans)
- [ ] Unit tests for the channel/timeout machinery against a mock `CamOps` (no hardware in CI)
- [ ] Evidence attached: log of a settings round-trip session
