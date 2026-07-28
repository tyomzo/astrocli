# AstroCtl — Software Design Description

**Document ID:** ASTROCTL-SDD-001
**Version:** 1.0.2
**Author:** Artiom
**Date:** 2026-07-28
**Status:** Draft
**Conformance:** ISO/IEC/IEEE 12207:2017 (Design Definition process, §6.4.5); description conventions informed by IEEE 1016
**Governing documents:** ASTROCTL-PRD-001 v1.5.0 (requirements), ASTROCTL-ADD-001 v1.1.0 (architecture)
**Change note (1.0.1):** Manual slew redesigned as a TTL-based dead-man's switch (§5.8.1, §5.4, T-SLW-1) — a lost link or stuck touch can no longer sustain motion.
**Change note (1.0.2):** Remote-link latency mitigations consolidated (§8.3): command staleness rejection, control/bulk connection separation (dedicated live-view socket), transfer pacing rule for Phase 2b, predictive position display and link-health surfacing in the PWA.

---

## 1. Introduction

### 1.1 Purpose

This document is the output of the Design Definition process for AstroCtl. It refines the architectural elements of ASTROCTL-ADD-001 into implementable design: Rust types and trait signatures, protocol encodings, state machines, task/channel topology, API schemas, storage formats, and test design. Where the ADD says *what* an element is responsible for, this document says *how* it is built.

### 1.2 Scope and increment plan

Per 12207's iterative application and the phase plan (PRD §9), this SDD is delivered in increments. **Version 1.0.0 provides full design for Phase 1 scope plus cross-phase foundations** that must be stable from the first commit (type system, error model, event schema, concurrency topology, config). Later elements are designed at the boundary level only and will be detailed in subsequent SDD versions:

| SDD increment | Scope | Sections |
|---------------|-------|----------|
| **v1.0.0 (this)** | Foundations + Phase 1: core types, HAL, Skywatcher driver, gPhoto2 driver, simulators, safety monitor, frame store, live view pipeline, field API gateway, config, testing | all |
| v1.1.x (Phase 2a) | Session FSM detail, control pipeline, solver adapters, planning (erfa), slew limits detail | §5.6 expand, new §5.10–5.12 |
| v1.2.x (Phase 2b) | Transfer agent, stack backbone, worker IPC detail, calibration library, ingest | new sections |
| v1.3.x (Phase 2c) | Post-chain executor, rebuild manager, LLM agent, confirmation service | new sections |
| v2.x (Phases 3–4) | Guiding, polar alignment, ML workers, adapters (INDI/Alpaca) | new sections |

### 1.3 Design constraints inherited from the ADD

- Rust backbone (tokio/axum), Python only in stacking-server workers (ADR-03, ADR-13)
- Modular monolith per node; crate boundaries and dependency rules of ADD §5.6
- HAL traits are the extension contract and semver-stable from Phase 1 (ADD §6.2)
- Safety enforcement below the API layer (ADR-11); e-stop priority lane (ADD §5.4.3)
- Write-ahead ordering: frame → disk (fsync) → metadata → everything else (ADD §9.2)

---

## 2. Design Conventions

- **Language level:** Rust 2021 edition, stable toolchain, MSRV pinned in workspace.
- **Async:** tokio multi-threaded runtime. No blocking calls on runtime threads; blocking C-library work goes to dedicated OS threads (camera) or `spawn_blocking`/rayon (decode, detection). Trait methods are async via `async_trait` until native async-in-traits covers dyn dispatch needs.
- **Errors:** `thiserror` enums per crate; no `anyhow` in library crates (binaries may use it at the top level). Every error carries enough context to render an operator-facing message (PRD §2 "transparency" principle).
- **Serialization:** `serde` throughout; all externally visible JSON schemas (API, events, IPC, metadata files) carry a `v` version field.
- **IDs:** session IDs `YYYY-MM-DD_<target-slug>`; frame IDs zero-padded sequence numbers per session (`light_00042`); job IDs monotonically increasing u64 per process run.
- **Time:** all persisted timestamps are UTC RFC 3339 with milliseconds. Local time appears only in UI rendering.
- **Units:** RA in hours, DEC in degrees, alt/az in degrees, exposure in seconds, temperatures in °C — carried in newtypes (§4.1) to make unit bugs unrepresentable.
- **Logging:** `tracing` with structured fields; every log line that corresponds to an operator-visible event also goes through the event bus (single source of truth, SES-07).

---

## 3. Design Overview

Phase 1 delivers one binary, `astroctl-field`, composed of the crates below. Arrows are compile-time dependencies (all also depend on `astroctl-core`):

```
                       astroctl-field (bin)
                     /      |       |      \
        astroctl-safety  astroctl- astroctl- astroctl-session
              |          pipeline  hal   ┌──────┘
              |             |       |    │ (frame store lives here)
              └────────► astroctl-drivers
                        (skywatcher, gphoto2, simulators)
```

Runtime task topology (Phase 1):

```
 axum server task ──► mount facade ──► [normal lane] ──┐
      │                    │                            ├─► serial task ─► /dev/ttyUSB*
      │                    └─────────► [priority lane] ─┘      │
      │                                                    heartbeat
      ├──► camera facade ──► command channel ──► camera thread ─► libgphoto2
      │                                             │
      ├──► live view pipeline ──► decode pool       │ (frames)
      │            ▲______________frames____________┘
      ├──► frame store (fsync writes, session dirs)
      ├──► WS hub ◄── event bus (tokio::sync::broadcast)
      └──► watchdog task (serial heartbeat, USB presence, disk, clock)
```

---

## 4. Foundation Design (`astroctl-core`)

### 4.1 Domain types

```rust
/// Right ascension in hours [0, 24). Constructor normalizes.
#[derive(Copy, Clone, Serialize, Deserialize, PartialEq)]
pub struct RaHours(f64);

/// Declination in degrees [-90, +90]. Constructor validates.
#[derive(Copy, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecDegrees(f64);

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct RaDec { pub ra: RaHours, pub dec: DecDegrees }

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct AltAz { pub alt_deg: f64, pub az_deg: f64 }

#[derive(Copy, Clone, Serialize, Deserialize, PartialEq)]
pub enum TrackingMode { Sidereal, Lunar, Solar }

#[derive(Copy, Clone, PartialEq)] pub enum Axis { Ra, Dec }
#[derive(Copy, Clone, PartialEq)] pub enum Direction { North, South, East, West }
#[derive(Copy, Clone, PartialEq)] pub enum SlewSpeed { Guide, Slow, Medium, Fast, Max }
```

Newtype constructors (`RaHours::new`, `DecDegrees::new`) are the only way to build coordinate values; out-of-range input is a `CoreError::InvalidCoordinate`, never a wrapped/clamped silent fix.

### 4.2 Error model

```rust
// astroctl-core
#[derive(thiserror::Error, Debug)]
pub enum DeviceError {
    #[error("device not connected")]          NotConnected,
    #[error("timeout after {0:?}")]           Timeout(Duration),
    #[error("protocol error: {0}")]           Protocol(String),
    #[error("device rejected command: {0}")]  Rejected(String),
    #[error("transport error: {0}")]          Transport(String),   // serial/USB layer
    #[error("unsupported by this device")]    Unsupported,         // capability mismatch
    #[error("busy: {0}")]                     Busy(&'static str),  // e.g. slew in progress
}
```

API error envelope (every non-2xx response):

```json
{ "v": 1, "code": "MOUNT_TIMEOUT", "message": "mount did not respond within 2.0s",
  "detail": {"axis": "ra", "command": "j"}, "retryable": true }
```

HTTP mapping: `NotConnected`/`Unsupported` → 409; `Timeout`/`Transport` → 502 (device side, `retryable: true`); `Rejected`/validation → 422; safety-limit rejection → 403 with `code: "LIMIT_ALTITUDE"`; auth failure → 401. Codes are a closed enum shared with the UI.

### 4.3 Event schema

All events flow through one bus (`tokio::sync::broadcast<Event>`); WS frames and the session log serialize the identical struct (ADD §6.2):

```rust
#[derive(Clone, Serialize)]
pub struct Event {
    pub v: u16,             // schema version, 1
    pub ts: DateTime<Utc>,
    pub topic: Topic,       // closed enum, serialized as "mount.position", …
    pub data: serde_json::Value,
}
```

Phase 1 topics and payloads:

| Topic | Payload | Cadence |
|-------|---------|---------|
| `mount.position` | `{ra, dec, alt, az, pier_side}` | 1 Hz (MNT-02) |
| `mount.status` | `{state, tracking, slewing, parked}` | on change |
| `camera.status` | `{connected, battery_pct, charging, storage_free_mb}` | on change + 60s |
| `capture.progress` | `{frame_id, state: exposing\|downloading\|saved, elapsed_s}` | on change |
| `frame.saved` | `{frame_id, path, size_bytes, sha256}` | per frame |
| `liveview.frame` | binary WS frame (JPEG), not JSON — carried on the dedicated `/ws/liveview` socket, never on `/ws` (§8.3) | ≤ camera rate |
| `alert` | `{severity, code, message}` | as needed |
| `system.health` | `{disk_free_gb, clock_synced, uptime_s}` | 60s |

The WS hub drops slow consumers (bounded per-client queue, close on overflow) rather than applying backpressure to the bus — a stalled phone must never stall capture.

### 4.4 Configuration

Config structs mirror PRD §8.1 exactly, `#[serde(deny_unknown_fields)]` on every level — a typo in the YAML is a startup error listing the offending key, not silent default behavior. Validation pass after parse: port existence deferred to connect, but ranges (baud, limits, thresholds) and cross-field rules (e.g. `mount.limits.min_altitude_degrees ∈ [0, 45]`) checked at load. The loaded, validated config is exposed as `Arc<FieldConfig>`; no component re-reads the file.

### 4.5 Auth (Phase 1 subset of SEC-02)

Bearer token middleware on every route including WS upgrade: constant-time comparison against the token from `auth_token_env`. Absent env var + `server.host` not a loopback/VPN address → startup refuses with an explanatory error (SEC-01 enforcement at the earliest possible point). Confirmation-token machinery (SEC-03) is Phase 2c; the route metadata slot for tiers (§8.2) exists from Phase 1 so routes are annotated once.

---

## 5. Element Designs

### 5.1 HAL (`astroctl-hal`)

Traits follow PRD §4.1 exactly; representative signature set:

```rust
#[async_trait]
pub trait MountDevice: Send + Sync {
    async fn connect(&self) -> Result<(), DeviceError>;
    async fn disconnect(&self) -> Result<(), DeviceError>;
    async fn position(&self) -> Result<RaDec, DeviceError>;
    async fn status(&self) -> Result<MountStatus, DeviceError>;
    /// Resolves when the slew completes or fails; cancel-safe (drop = no stop).
    async fn goto(&self, target: RaDec) -> Result<(), DeviceError>;
    async fn sync(&self, pos: RaDec) -> Result<(), DeviceError>;
    async fn start_tracking(&self, mode: TrackingMode) -> Result<(), DeviceError>;
    async fn stop_tracking(&self) -> Result<(), DeviceError>;
    async fn slew(&self, axis: Axis, dir: Direction, speed: SlewSpeed) -> Result<(), DeviceError>;
    async fn stop_slew(&self, axis: Axis) -> Result<(), DeviceError>;
    async fn guide_pulse(&self, axis: Axis, dir: Direction, duration_ms: u32) -> Result<(), DeviceError>;
    async fn park(&self) -> Result<(), DeviceError>;
    async fn unpark(&self) -> Result<(), DeviceError>;
    /// Must complete without awaiting normal-lane traffic. See §5.4.
    async fn emergency_stop(&self) -> Result<(), DeviceError>;
    fn capabilities(&self) -> MountCapabilities;
    fn device_info(&self) -> DeviceInfo;
}
```

`Camera` and `GuideCamera` follow the same pattern (signatures per PRD §4.1; `BatteryStatus { percent: u8, charging: bool }` per PRD). Capability structs are plain serde data:

```rust
pub struct MountCapabilities {
    pub has_pec: bool,
    pub has_pulse_guide: bool,
    pub tracking_rates: Vec<TrackingMode>,
    pub max_slew_speed_x_sidereal: u32,
    pub position_resolution_bits: u8,
}
```

**Registry** (HAL-07, HAL-08): `DriverRegistry` maps config names → factory closures. Registration is static (inventory of built-in drivers, feature-gated); `registry.create_mount("skywatcher", &cfg.mount)?` returns `Arc<dyn MountDevice>`. Auto-detection: each factory optionally implements `probe() -> Vec<DetectedDevice>` (serial port scan matching known USB VID/PIDs; gphoto2 autodetect list).

### 5.2 Skywatcher mount driver (`astroctl-drivers::skywatcher`)

#### 5.2.1 Layering

```
SkywatcherMount (impl MountDevice)          — semantics: coordinates, modes, goto logic
    └── MotorController                     — per-axis: counts, motion modes, ramping
          └── SyntaCodec + SerialTask       — framing, encoding, request/response, lanes
```

#### 5.2.2 Wire protocol (SyntaCodec)

Frame: `:` + command char + axis digit (`1`=RA, `2`=DEC, `3`=both where valid) + payload + `\r`. Response: `=` + payload + `\r` (success) or `!` + error digit + `\r`.

24-bit values are ASCII hex with **byte-swapped ordering**: value `0x123456` is transmitted `"563412"` (low byte first, per PRD §4.2 little-endian note). Codec functions `encode_u24/decode_u24` are pure and unit-tested against golden vectors captured from EQMOD traces.

Command set used in Phase 1 (**all opcodes to be verified against the EQMOD source before first powered test** — PRD §4.2 risk note):

| Cmd | Meaning | Used by |
|-----|---------|--------|
| `e` | Firmware version | connect handshake |
| `a` | Counts per revolution (CPR) | connect handshake → stored per axis |
| `b` | Timer interrupt frequency | connect handshake |
| `j` | Get position counter | 1 Hz poll, goto monitoring |
| `f` | Get axis status | status, slew-complete detection |
| `F` | Initialize axis | connect |
| `G` | Set motion mode (dir + speed class) | tracking, slew, goto |
| `S` | Set goto target (absolute counts) | goto |
| `I` | Set step period (speed) | tracking rates, slew speeds |
| `J` | Start motion | tracking, slew, goto |
| `K` | Stop motion (ramped) | stop_slew, stop_tracking |
| `L` | Instant stop | **emergency_stop only** |
| `P` | Set autoguide rate | guide_pulse setup |

#### 5.2.3 Position math

Per axis: `counts_home = 0x800000`. With CPR read at handshake:

```
ra_counts→hours:  ra_h  = ((counts - counts_home) / CPR) * 24.0   (mod 24, hemisphere-adjusted)
dec_counts→deg:   dec_d = ((counts - counts_home) / CPR) * 360.0
```

RA axis position is mechanical hour angle; conversion to/from RA requires LST — Phase 1 computes LST from system clock + site longitude (REL-14 warns when clock is unsynced; full erfa-based apparent-place pipeline arrives with `astroctl-planning` in Phase 2a, and this module keeps the conversion behind `fn mech_to_sky(&self, counts: AxisCounts, lst: Lst) -> RaDec` so the upgrade is internal). Pier-side handling: DEC counts beyond ±90° imply the flipped pier state; `pier_side` is derived, reported in `mount.position` events, and consumed by the meridian limit (§5.4).

Goto: absolute target counts computed from target RaDec + LST + chosen pier side; long slews use high-speed motion mode with the ramp handled by the motor controller; the driver polls `j`/`f` at 2 Hz during goto, declares completion when both axes report stopped within tolerance (default 10 counts), then restores tracking if it was active (SES-06).

#### 5.2.4 Serial task and lanes

One tokio task owns the `serialport` handle exclusively.

```rust
enum SerialRequest { Normal(Cmd, oneshot::Sender<Result<Resp>>),
                     Priority(Cmd, oneshot::Sender<Result<Resp>>) }
// two mpsc channels; the task select!s with bias: priority drained first,
// in-flight normal request completes (single request-response, ≤ ~50 ms) but
// no new normal request starts while priority queue is non-empty.
```

Per-request timeout 500 ms, one retry on timeout/garbled response, then `DeviceError::Timeout` and a `mount.status` degradation event. Heartbeat: the 1 Hz position poll doubles as the heartbeat; 3 consecutive failures → watchdog fires (§5.4). Emergency stop = `Priority(L axis1)` + `Priority(L axis2)`; measured budget from API handler to bytes-on-wire ≤ 20 ms (test T-SER-3, §9).

### 5.3 Canon gPhoto2 camera driver (`astroctl-drivers::gphoto2`)

#### 5.3.1 Thread model

libgphoto2 calls can block for seconds and the context is not thread-safe → **one dedicated OS thread** owns `gphoto2::Context` + `Camera` for the device's lifetime. Facade ↔ thread via `std::sync::mpsc` command channel with tokio `oneshot` replies:

```rust
enum CamCmd {
    Connect, Disconnect,
    GetSettings, SetSetting { key: CfgKey, value: String },
    Capture { reply: … },              // returns CaptureResult{camera_path}
    StartBulb { duration: Duration },  // thread manages timer + release
    AbortCapture,
    Download { camera_path, dest: PathBuf },  // streams to temp file + rename
    LiveViewStart, LiveViewStop,       // when active, thread pushes JPEGs to a watch channel
    GetBattery, GetStorage,
}
```

Every command has an operation-class timeout (config get/set 5 s; capture = exposure + 30 s; download 120 s). A timed-out thread is considered wedged: the facade drops the channel, the thread is abandoned (it cannot be safely killed mid-libgphoto2-call), a fresh thread + context is spawned, and a USB reset is attempted — this is the REL-03 recovery path, surfaced as a `camera.status` reconnecting event.

#### 5.3.2 Capture flow (CAM-03/04, REL-05)

```
capture request → set format/ISO/shutter if changed → trigger
  → wait event CAPTURE_DONE (or bulb timer expiry → release)
  → download to <session>/frames/.tmp_<id>.cr3   (streamed)
  → fsync file → rename to light_<id>.cr3 → fsync dir      ← frame is now durable
  → compute sha256 (blocking pool) → write frame meta JSON → emit frame.saved
  → hand path to live view pipeline (and, Phase 2b, transfer queue)
```

The rename-after-fsync makes a torn download invisible to every consumer (they only ever see completed frames). Bulb: R10 bulb is driven via PTP remote release config; if the binding lacks it, the fallback adapter (§5.3.3) handles bulb first — this is the highest-risk item in Phase 1 (ADD §10) and gets prototyped first.

#### 5.3.3 CLI fallback

`GPhoto2Cli` implements the same internal `CamOps` trait by shelling out to the `gphoto2` binary per operation (`--capture-image-and-download`, `--set-config`, `--wait-event`). The concrete driver is composed per-operation from a coverage table in config (`camera.ops_via_cli: ["bulb"]`), so a binding gap on one operation doesn't force the whole driver onto the CLI.

### 5.4 Safety monitor (`astroctl-safety`)

Sits between every caller and the mount driver — the mount facade the API/orchestrator sees **is** the safety wrapper (ADR-11):

```rust
pub struct SafeMount { inner: Arc<dyn MountDevice>, limits: Limits, site: Site, bus: EventBus }
impl SafeMount /* implements MountDevice */ {
    // goto/slew: compute target AltAz; alt < min_altitude → Err(LimitViolation::Altitude)
    // manual slew: TTL-governed (dead-man's switch, §5.8.1) — motion authorized per
    //   request for ttl_ms; TTL watcher stops the axis on expiry, renewal extends it
    // continuous slew: background limit check at 2 Hz while manual slew active; auto-stop + alert
    // meridian: hour-angle watch task; past limit → stop tracking + alert (MNT-16)
    // emergency_stop / estop lane: forwarded verbatim, never gated, never queued
}
```

Watchdogs (one task, 1 Hz tick): serial heartbeat freshness; camera thread liveness; disk free vs. thresholds (REL-12: warn → pause-after-frame); clock sync via `adjtimex` state (REL-14 warning). Watchdog actions publish `alert` events and, for serial loss during motion, issue priority-lane stop — a mount slewing on a dead link is the one scenario where the watchdog acts autonomously (REL-02/03).

### 5.5 Frame store & session layout (`astroctl-session`)

Directory layout exactly as PRD §5.9. Phase 1 writes:

```
sessions/<session_id>/session.json      # v, target?, equipment (from config), created_ts
                     frames/light_<id>.cr3
                     control/quality_<id>.json   # Phase 1: exposure params, sha256, size
```

`session.json` and per-frame metadata are written with the same tmp-fsync-rename discipline as frames. A `CURRENT` symlink identifies the active session. Disk monitor consults this store for REL-12. The store exposes `reserve_frame_id()` (atomic counter persisted in session.json on each grant) so a crash never reuses an ID (REL-04 groundwork).

### 5.6 Session orchestrator — Phase 1 skeleton

Phase 1 needs single-shot capture only; the FSM ships with three states so the API shape is final from the start:

```
Idle ──start_capture──► Capturing ──saved──► Idle
   └──connect/disconnect device management──┘        Faulted (from any state; operator ack → Idle)
```

The full sequence FSM (targets, dithering, solve-and-center, pause/resume — SES-01..06) is specified in SDD v1.1.x; its states will be a superset and the persistence format (`session.json: sequence_state`) is already reserved.

### 5.7 Live view pipeline (`astroctl-pipeline::liveview`)

Two sources, one output path (WS binary frames on `liveview.frame`):

1. **Camera live view stream** (CAM-05): JPEG frames from the camera thread's watch channel, forwarded as-is; rate-limited per client (default 5 fps LAN / adaptive down to 1 fps, USB-11 groundwork).
2. **Last-captured preview** (CAM-06, IPP-04): on `frame.saved`, a decode job goes to the blocking pool — libraw half-size decode → quarter-res RGB → asinh auto-stretch (fixed algorithm in Phase 1; the STF options come with the post-chain) → JPEG (quality 85) → cached as `<session>/preview/light_<id>.jpg` and pushed once on the bus.

Decode jobs are a queue of depth 1 with replace semantics: if frames arrive faster than decode, only the newest is previewed (previews are ephemeral; raw frames are what matters).

### 5.8 API gateway (field binary)

#### 5.8.1 Route table (Phase 1)

All routes under bearer auth (§4.5); tier annotations present from Phase 1 (enforced from Phase 2c):

| Route | Method | Tier | Body → Response |
|-------|--------|------|-----------------|
| `/api/system/health` | GET | read | → `{status, disk_free_gb, clock_synced, versions}` |
| `/api/system/info` | GET | read | → config summary, driver list, capabilities |
| `/api/mount/connect` | POST | low | `{port?}` → status |
| `/api/mount/disconnect` | POST | low | → status |
| `/api/mount/position` | GET | read | → `{ra, dec, alt, az, pier_side}` |
| `/api/mount/status` | GET | read | → MountStatus |
| `/api/mount/goto` | POST | medium | `{ra_hours, dec_degrees}` → 202 + progress via WS |
| `/api/mount/tracking` | POST | low | `{mode: "sidereal"\|"lunar"\|"solar"\|"off"}` |
| `/api/mount/slew` | POST | low | `{axis, direction, speed, ttl_ms?}` — dead-man's switch, see below |
| `/api/mount/slew/stop` | POST | low | `{axis?}` |
| `/api/mount/park` / `unpark` | POST | high | → 202 |
| `/api/mount/estop` | POST | *blocked-for-LLM* | → 200 always if delivered (dedicated handler, §5.8.2) |
| `/api/camera/connect` / `disconnect` | POST | low | |
| `/api/camera/settings` | GET/PUT | read/low | `{iso, shutter, aperture, format}` + available values |
| `/api/camera/capture` | POST | medium | `{}` or `{bulb_seconds}` → 202, `capture.progress` on WS |
| `/api/camera/capture/abort` | POST | low | |
| `/api/camera/battery`, `/storage` | GET | read | BatteryStatus / StorageInfo |
| `/api/session/current` | GET | read | session.json view + frame list |
| `/api/session/frames/{id}/preview.jpg` | GET | read | cached preview image |
| `/ws` | GET | read | WS upgrade — control/status events (JSON only); subscribe message selects topics |
| `/ws/liveview` | GET | read | WS upgrade — binary JPEG frames only (live view + previews); separate socket so a large frame can never head-of-line-block control traffic (§8.3) |

`202 + WS progress` is the pattern for every long-running action; the response includes the event topic and correlation ID to watch.

**Manual slew is a dead-man's switch.** Each `/api/mount/slew` call authorizes motion for `ttl_ms` only (default 500 ms, max 2000 ms, clamped server-side). While the operator holds the D-pad, the PWA re-sends the same request every `ttl_ms / 2`; a repeat with identical parameters extends the deadline without re-issuing motor commands. If no renewal arrives before the TTL expires — dropped VPN packet, stuck touch event, crashed browser tab — the SafeMount TTL watcher stops that axis and emits an `alert` (`code: "SLEW_TTL_EXPIRED"`). Release sends `/api/mount/slew/stop` for immediate stop; TTL expiry is the backstop, not the primary stop path. Goto is *not* TTL-governed — it is a bounded, position-targeted motion supervised by slew-complete detection (§5.2.3) and the safety limits (§5.4).

**Motion-initiating commands reject staleness.** Every state-changing request carries `issued_at` (client UTC) and a client-generated `command_id`. The server rejects motion-*initiating* commands (goto, slew start, tracking start, capture start) whose `issued_at` is older than `max_command_age_ms` (default 2000) with `code: "COMMAND_STALE"` — a request delayed by VPN retry storms must not start motion long after the operator's intent has passed. The asymmetry is deliberate: **stopping commands (slew/stop, tracking off, abort, e-stop) are never staleness-rejected** — a late stop is safe, a late start is not. Client clock skew is handled by echoing server time in every response; the PWA offsets `issued_at` by the measured skew, and skew beyond 30 s raises a UI warning (ties into REL-14 clock discipline). `command_id` makes retries idempotent: a re-sent request with a known id returns the original outcome instead of re-executing.

#### 5.8.2 E-stop handler

Registered before the normal middleware stack (auth only, no JSON parsing — empty body accepted), calls `SafeMount::emergency_stop()` directly. Handler + priority lane budget ≤ 20 ms; the PRF-12 end-to-end figure is then dominated by network RTT, as intended.

#### 5.8.3 WS hub

One task serving two endpoints per client (§8.3 separation): `/ws` for JSON control/status events, `/ws/liveview` for binary image frames. Per-client bounded queues: on `/ws`, 64 events with a latest-only slot for `mount.position` (high-rate telemetry coalesces, discrete events never dropped while under bound); on `/ws/liveview`, a depth-1 replace queue — only the newest frame is ever in flight. Client subscribe/unsubscribe messages filter topics server-side. Reconnect is client-driven (PWA auto-reconnect, REL-10); on `/ws` connect the hub sends a state snapshot (current status of every stateful topic) so the UI never renders from partial state. Every outbound event carries `ts`, and the hub answers `ping` frames immediately — the PWA derives link RTT and telemetry age from these (§8.3).

### 5.9 PWA (Phase 1 scope)

React + TypeScript, Vite build, output embedded in the binary via `include_dir!`. State: a thin store fed exclusively by WS events + snapshot (no REST polling); commands are REST calls that optimistically do nothing — UI state changes only when the corresponding event arrives (single source of truth). Two link-latency affordances (§8.3): **predictive position display** — between `mount.position` updates the UI dead-reckons the displayed coordinates from the last update and the known tracking/slew state (a tracking mount's motion is exactly predictable), rendering predicted values in a visually distinct "aging" style that resolves to confirmed on the next event; and **link-health surfacing** — header shows WS RTT and telemetry age, turning amber past 500 ms RTT / 3 s age and red on disconnect, so the operator always knows how stale their picture is before issuing commands. Phase 1 screens: connect panel, mount panel (coordinates, tracking, D-pad with press-and-hold slew — hold renews the slew TTL per §5.8.1, release sends stop, speed selector), camera panel (settings, capture, bulb countdown), live view/preview panel, header status bar (USB-04), e-stop button fixed in the header on every screen (USB-03, 44 px targets USB-12). Manifest + service worker per USB-09/10 (shell cached, data never cached).

---

## 6. Data Design (Phase 1)

| Artifact | Format | Written by | Schema highlights |
|----------|--------|-----------|-------------------|
| `field-node.yaml` | YAML | operator | PRD §8.1; deny-unknown-fields |
| `session.json` | JSON v1 | frame store | id, created, equipment snapshot, frame counter, sequence_state (reserved) |
| `frames/light_<id>.cr3` | CR3 | camera driver | immutable after rename (REL-11) |
| `control/quality_<id>.json` | JSON v1 | capture flow | ts, exposure, iso, format, sha256, size |
| `preview/light_<id>.jpg` | JPEG | live view pipeline | ephemeral, regenerable |
| session log | JSONL of Event | event bus sink | one file per session; rotation not needed (bounded by session) |

No SQLite on the field node in Phase 1 (first need is the transfer journal, Phase 2b).

---

## 7. Concurrency Design Summary

| Task/thread | Kind | Owns | Communicates via |
|-------------|------|------|------------------|
| axum server | tokio tasks | — | facades (Arc), bus |
| serial task | tokio task | serial port | 2× mpsc in (lanes), oneshot replies |
| camera thread | OS thread | gphoto2 context | std mpsc in, oneshot replies, watch out (live view) |
| decode pool | `spawn_blocking` (≤2) | — | job queue (depth 1, replace) |
| watchdog | tokio task | — | bus out, priority lane on serial-loss-while-moving |
| WS hub | tokio task | client sockets | broadcast in, per-client queues out |
| event bus | `broadcast` channel | — | capacity 256; lagging receiver ⇒ resync via snapshot |

Shutdown order (SIGTERM): stop accepting API → abort live view → if capturing, finish download (bounded 120 s) → stop tracking? **No** — tracking state is left as-is (an operator restart of the service mid-session must not stop the mount) → flush session log → exit. This asymmetry (finish camera, don't touch mount) is deliberate: the mount is safe while tracking; a half-downloaded frame is a lost frame.

---

## 8. Design of Cross-Cutting Mechanisms

### 8.1 Startup sequence

config load+validate → auth check (§4.5) → frame store open/create session → registry builds drivers (no connect) → safety wrapper → API up (health returns `starting`) → watchdogs on → health `ok`. Hardware connect is always an explicit operator action (or `--auto-connect` flag for fixed installations) — matching "startup to first capture < 60 s" (PRD §12) without surprise motion on boot.

### 8.2 Route metadata

Every route registers `RouteMeta { tier: Tier, audit: bool }` via a typed layer. Phase 1 uses it for audit logging only; Phase 2c's confirmation middleware and the LLM tool generator (ADD §6.1) consume the same declarations — the invariant "one declaration drives both" is established now.

### 8.3 Remote-link latency mitigations

Consolidated design position on operating over a slow/lossy VPN. The first two are architectural (ADD) and restated here for completeness; the rest are binding on the designs in this document:

1. **Edge autonomy** — deadline-bearing loops (guiding, watchdogs, limits, sequence execution) close on the field node; the link carries intents, not actuation (ADD §5.4.4, REL-09, PRF-03).
2. **Intent-based commands** — goto/track/sequence are goals executed under local supervision; nothing requires a sustained command stream to remain safe.
3. **Leases for continuous motion** — the slew dead-man's switch (§5.8.1): silence means stop.
4. **Staleness rejection + idempotency** — `issued_at`/`command_id` envelope (§5.8.1): late starts are refused, late stops are always honored, retries are idempotent.
5. **Connection separation** — safety/control traffic never shares a TCP stream with bulk image data: `/ws` (JSON events) and `/ws/liveview` (binary frames) are distinct sockets, and the e-stop POST uses the browser's separate HTTP connection pool with `keepalive`. A 500 KB JPEG retransmit can therefore never head-of-line-block a stop command or a position update.
6. **Coalescing telemetry** — latest-only delivery for self-superseding state (§5.8.3); the UI shows the present or a marked prediction, never a replayed past.
7. **Transfer pacing** *(binding rule for the Phase 2b transfer agent, SDD v1.2.x)* — the frame uploader must (a) enforce a configurable bandwidth cap, and (b) automatically yield: while any operator motion command is active or was issued within the last 10 s, uploads throttle to a configured interactive floor (default 20% of cap). Prevents self-inflicted bufferbloat where a 25 MB CR3 upload queues the operator's commands behind it in the tunnel.
8. **Predictive display + link-health surfacing** — the PWA dead-reckons between updates and displays RTT/telemetry age (§5.9); degradation is explicit, never silent (PRF-01, USB-11).

---

## 9. Verification Design (Phase 1)

| ID | Test design | Verifies |
|----|-------------|----------|
| T-COD-1 | Golden-vector unit tests for SyntaCodec (encode/decode u24, framing) incl. vectors captured from EQMOD logs | §5.2.2, protocol risk |
| T-POS-1 | Property tests: counts↔coordinates round-trip within 1 count; hemisphere/pier cases table-driven | §5.2.3 |
| T-SER-1 | Serial task against a mock port: timeout, retry, garbled response, lane priority under load | §5.2.4, REL-02 |
| T-SER-3 | E-stop latency: request injected during 50-cmd/s normal load; assert bytes-on-wire ≤ 20 ms | PRF-12 budget |
| T-SLW-1 | Slew TTL: start manual slew, silently drop renewals (simulated link loss); assert axis stop within ttl_ms + 100 ms and `SLEW_TTL_EXPIRED` alert emitted | §5.8.1 dead-man's switch |
| T-STALE-1 | Command staleness: goto with `issued_at` 5 s old → `COMMAND_STALE`, no motion; slew/stop with same age → executed; duplicate `command_id` → original outcome returned, no re-execution | §5.8.1 staleness/idempotency |
| T-HOL-1 | Connection separation: saturate `/ws/liveview` with frames over a shaped 1 Mbit link; assert `/ws` position events and e-stop POST latency unaffected (≤ 2× baseline) | §8.3(5) |
| T-CAM-1 | Camera thread against gphoto2 vusb/simulator: capture, settings, timeout-wedge recovery respawn | §5.3.1, REL-03 |
| T-E2E-1 | Full API-level session against simulator drivers: connect → goto → capture → preview → frame durable; assert event stream shape | Phase 1 exit criteria |
| T-DUR-1 | Kill -9 during download / during meta write; on restart assert no partial frame visible, no ID reuse | §5.3.2, §5.5, REL-04/05 |
| T-HIL-1 | Hardware-in-loop checklist (real HEQ5 + R10): handshake values vs. EQMOD reference, low-speed slews first, bulb prototype — **first powered milestone, gates Phase 1 completion** | §5.2, §5.3, ADD §10 risks |
| T-SOAK-1 | 8 h simulator soak: 1 Hz polling + capture every 60 s; assert memory flat ≤ 512 MB steady (PRF-05), no task death | PRF-05, robustness |

Simulators (HAL-11) are first-class: `SimulatorMount` implements realistic slew ramps, settle, and configurable fault injection (timeouts, garbled frames) — fault injection is a constructor parameter so T-SER/T-E2E tests express failure scenarios declaratively.

## 10. Requirements Traceability (Phase 1 elements)

| Requirement | Design element |
|-------------|----------------|
| HAL-01..07 | §5.1 traits + registry |
| HAL-08 | §5.1 probe design |
| HAL-11 | §9 simulators |
| MNT-01..08 | §5.2 driver; §5.4 wrapper; §5.8 routes |
| MNT-15/16 | §5.4 SafeMount |
| CAM-01..05 (05 basic), CAM-06, CAM-08 | §5.3, §5.7, §5.8 |
| IPP-04, IPP-09/10 (Phase 1 subset) | §5.7, §5.5, §6 |
| SES-07 (basic) | §4.3 bus → session log sink |
| ARC-01..05, ARC-07 | §2, §3, §4.4, §5.8.3 |
| REL-01..05, REL-11, REL-12, REL-14 | §5.2.4, §5.3.2, §5.4, §5.5 |
| PRF-01, PRF-04, PRF-05, PRF-12 | §5.2.4, §5.3.1, §7, §5.8.2, T-SOAK-1/T-SER-3 |
| SEC-01/02 (subset) | §4.5 |
| USB-03/04/09/10/12 | §5.9 |

Requirements of later phases trace at architecture level via ADD §11 and will be detailed in the SDD increments of §1.2.

---

*Verification note (12207 §6.4.5.3(c)): each design element above names its governing requirement IDs; the Phase 1 exit review walks this table against T-E2E-1/T-HIL-1 results.*
