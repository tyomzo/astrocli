# M1-T01 — HAL traits, capabilities, registry

**Milestone:** M1 · **Track:** A · **Depends on:** M0 · **Crates:** astroctl-hal
**Spec:** SDD §5.1 (verbatim trait signatures); PRD §4.1, HAL-01..08; ADD ADR-02

## Objective

The frozen extension contract: `MountDevice`, `Camera`, `GuideCamera` async traits, capability
and info structs, and the config-name → factory registry.

## Scope

- Traits exactly per SDD §5.1 (async_trait, `Send + Sync`, `Arc<dyn …>` usable); `Camera`/`GuideCamera` completed from PRD §4.1 method lists with the same conventions
- `MountCapabilities`, `CameraCapabilities`, `GuideCameraCapabilities`, `DeviceInfo` (serde)
- `DriverRegistry`: static registration (feature-gated driver list), `create_mount(name, cfg)`, `create_camera(name, cfg)`; unknown name → error listing available drivers
- `probe()` optional per factory returning `Vec<DetectedDevice>` (HAL-08) — trait + plumbing only; simulators report themselves
- Doc comments on every trait method: semantics, cancel-safety, error contract (these docs are the driver-author API)

Out of scope: any concrete driver (T02/T06), FilterWheel/Focuser (Phase 4).

## Acceptance criteria

- [ ] A test double implementing each trait compiles against `Arc<dyn>` usage patterns
- [ ] Registry: create by name, unknown-name error message lists drivers, feature-gating verified (`--no-default-features` builds without simulators)
- [ ] `cargo doc` output for the traits reads as a complete driver-author contract
