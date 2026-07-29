# M0-T02 — Core domain types and error model

**Milestone:** M0 · **Depends on:** M0-T01 · **Crates:** astroctl-core
**Size:** M · **Status:** done
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

- [x] Unit tests: normalization/validation edges (24.0h→0.0, −90/+90 inclusive, NaN rejected), serde round-trips, display formatting golden cases
- [x] `ErrorCode` → HTTP status mapping matches SDD §4.2 table exactly (table-driven test)
- [x] No public field of raw `f64` for a coordinate anywhere in the crate

## Result

Landed in `crates/astroctl-core/src/{types.rs,error.rs}`; 33 unit tests. Implementing §4.1/§4.2
found five defects in them, fixed in **SDD v1.7.0** and summarized in that change note: `AltAz`
held public raw `f64` fields (now `AltDegrees`/`AzDegrees`), the derived `Deserialize` bypassed
the validating constructors (now `try_from`), `Axis`/`Direction`/`SlewSpeed` had no serde derives
despite §5.8.1 deserializing them, the HTTP mapping omitted `DeviceError::Protocol` and `Busy`,
and the "closed enum shared with the UI" was never enumerated. `ErrorCode` is now defined and
tabulated in §4.2.

Two follow-ups this task could not close:
- **`DISK_FULL` has two statuses in the pack** — 507 in SDD §5.11.2, 409 in M1-T08. The code maps
  to 507; M1-T08 needs correcting or a second code.
- `GuideRate` out-of-range currently raises `CoreError::InvalidCoordinate` (a rate is not a
  coordinate). Kept single-variant deliberately; rename if a second non-coordinate quantity
  appears.
