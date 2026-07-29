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
- **Detect and explain the gvfs steal.** A desktop gvfs auto-mount holds the USB claim exclusively while libgphoto2 reports only "Could not claim the USB device" — an error that points nowhere useful. Observed for real, then reproduced and diagnosed in the M2-T01 spike (`spikes/skywatcher-heq5/FINDINGS.md` has the verified detection output and both working unmount forms). On a claim failure, scan `/run/user/<uid>/gvfs/` for a `gphoto2` mount and, if found, name it and give the release command; if not found, list the other causes rather than asserting one. **Note that masking the systemd user unit alone does not prevent it** — the D-Bus service file carries a direct `Exec`, so the activation path must be shadowed too; the verified procedure is in FINDINGS.md. Startup on the field node should refuse to proceed with a clear message rather than retrying blindly
- Connect: autodetect (single camera assumed; multiple → error listing), open, read capabilities → `CameraCapabilities` (values from T01's config-tree dump; pixel size etc. from equipment profile config)
- Settings: get/set ISO, shutter, aperture, format via config keys established in T01; `get_available_settings` from the camera's enumerated choices — never a hardcoded list
- Timeout ⇒ wedge protocol *stub*: channel dropped, error surfaced; full respawn logic in T04 (leave the seam + TODO)
- Registry name `"gphoto2"`; simulator remains default in example config until M2-T05

## Acceptance criteria

- [ ] With R10 attached: connect, read settings, change ISO from the API, see it on the camera body
- [ ] All blocking gphoto2 calls verifiably on the camera thread (assert via thread-name in tracing spans)
- [ ] Unit tests for the channel/timeout machinery against a mock `CamOps` (no hardware in CI)
- [ ] Evidence attached: log of a settings round-trip session
