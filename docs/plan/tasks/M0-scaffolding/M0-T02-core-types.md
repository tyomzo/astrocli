# M0-T02 — Core domain types and error model

**Milestone:** M0 · **Depends on:** M0-T01 · **Crates:** astroctl-core
**Size:** M · **Status:** not started
**Spec:** SDD §4.1 (domain types), §4.2 (error model), §2 (conventions)

## Objective

Implement the unit-safe domain type system and the closed error model every other crate
builds on. These are frozen contracts — get them right, they will not be revisited casually.

## Scope

- Newtypes `RaHours` (normalizing to [0,24)), `DecDegrees` (validating [-90,+90]); `RaDec`, `AltAz`, `TrackingMode`, `Axis`, `Direction`, `SlewSpeed` — serde impls exactly as SDD §4.1
- Constructors are the only way in; invalid input → `CoreError::InvalidCoordinate` (never clamp)
- Display impls: astronomical notation `HH:MM:SS.s` / `±DD°MM'SS"` (PRD USB-05)
- `DeviceError` enum per SDD §4.2 verbatim
- API error envelope types: `ApiError { v, code, message, detail, retryable }` with the closed `ErrorCode` enum and the HTTP status mapping table from SDD §4.2 as a function
- `DeviceInfo`, `BatteryStatus { percent, charging }`, `MountStatus`, capability structs (SDD §5.1)

Out of scope: coordinate *transforms* (alt/az from ra/dec — that's astroctl-planning, Phase 2a).

## Acceptance criteria

- [ ] Unit tests: normalization/validation edges (24.0h→0.0, −90/+90 inclusive, NaN rejected), serde round-trips, display formatting golden cases
- [ ] `ErrorCode` → HTTP status mapping matches SDD §4.2 table exactly (table-driven test)
- [ ] No public field of raw `f64` for a coordinate anywhere in the crate
