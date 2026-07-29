# AstroCtl — Product Requirements Document

**Document ID:** ASTROCTL-PRD-001  
**Version:** 1.12.1  
**Author:** Artiom  
**Date:** 2026-07-28  
**Status:** Draft

**Change note (1.8.0):** §7 re-grounded on build evidence rather than crates.io metadata — every crate was resolved and `cargo check`ed in isolation (`docs/evidence/dependency-survey-2026-07-29.md`). Outcomes: toolchain pin moves to **1.97.1** because `rusqlite` 0.40 does not build on 1.94; **`erfars`** selected for ERFA (it vendors the C source, so `liberfa-dev` is not needed); **`rawler`** selected as the RAW decoder (pure Rust with R10 fixtures, so `libraw-dev` is not needed); **`libudev-dev`** added — `serialport` requires it and nobody had recorded that. System dependencies are now measured, and M2's list is down to `libgphoto2-dev` alone.

**Change note (1.7.0):** §7 dependency table corrected against crates.io as of 2026-07-29, with versions pinned where verified. Three claims did not survive checking: there is **no `sep` crate** (a first-party libsep FFI binding must be written — new risk-register entry); the crate named **`erfa` is a pure-Rust reimplementation, not liberfa bindings** (select `erfars`/`erfa-sys` instead); and **RAW-decode bindings are immature** (`libraw` 0.1.1), so decoder selection moves into the M2-T01 spike. System dependencies are now listed by the milestone that first needs them — M0 requires no C libraries.

**Change note (1.6.0):** Configuration schema completed — §8.1/§8.2 are normative for configuration and now carry every key the design documents reference (slew TTL, command staleness, per-operation CLI fallback, storage/session paths and disk thresholds, transfer pacing, device timeouts, worker interpreter). Processed-pipeline latency budgets disambiguated (IPP-06 vs PRF-08 vs §12). CAM-05/CAM-06 assigned to a single phase. Language-neutral GuideCamera frame type (residue of the pre-ADR-03 all-Python design). PRF-05 reconciled with the LLM-13 local STT fallback.

**Change note (1.9.0):** M2-T01 executed against the real Canon R10 ahead of schedule (`spikes/gphoto2-r10/FINDINGS.md`). The gPhoto2 support risk is **retired**: bulb, capture, CR3 download, settings and a 58.5 fps live-view stream all work through the bindings, so `camera.ops_via_cli` ships empty. Measured full-RAW frame size corrected to 32 MB.

**Change note (1.10.0):** `server.runtime_worker_threads` added to §8.1/§8.2. The tokio runtime is now sized deliberately per node rather than defaulting to one worker per core — on a 4-core field node that default competes with the camera OS thread and the decode pool for exactly the cores PRF-04 depends on (SDD §7).

**Change note (1.11.0):** §4.2 mount parameters verified against the operator's own HEQ5 by read-only handshake (`spikes/skywatcher-heq5/FINDINGS.md`). **Timer interrupt frequency corrected from ~460,800 Hz to 64,935 Hz** — wrong by a factor of 7.1. CPR (9,024,000) and counter home (`0x800000`) confirmed exactly. The high-speed ratio remains unverified. The protocol-documentation risk in §10 is marked as having *occurred and been contained*.

**Change note (1.11.1):** §4.2 high-speed ratio verified as 16× by read-only survey (`:g`, both axes) — the last unverified mount constant. All §4.2 parameters are now hardware-confirmed.

**Change note (1.12.0):** §4.2 timer frequency **confirmed under motion** — slewing at step period 620 measured 104.617 counts/s (0.999× sidereal), implying 64,862 against the corrected 64,935. The 460,800 figure would have predicted 743 c/s. Sidereal step period (620) and measured goto speeds recorded; goto speed is fixed per mode digit and not settable via the step period.

**Change note (1.12.1):** §4.2 motion behaviour corrected after optical confirmation — goto ramps trapezoidally to ~835× sidereal rather than running at one of two fixed speeds; short moves are ramp-limited. Physical rotation confirmed by camera observation at 7.1× the noise floor.

---

## 1. Problem Statement

Getting into astrophotography on Linux means confronting a fragmented ecosystem of specialized tools that don't work together seamlessly. A typical imaging session requires understanding and configuring some combination of: KStars for planetarium and planning, Ekos for session management, INDI for device communication, PHD2 for guiding, a plate solver (ASTAP or astrometry.net), Siril or PixInsight for stacking and post-processing — each with its own concepts, configuration, failure modes, and quirks. Building a working pipeline from these pieces is a nightmare: connections break silently, error messages are cryptic, configuration is scattered across multiple applications, and rarely does anything work on the first attempt.

The learning curve is brutal not because any individual tool is bad, but because the operator has to become a systems integrator. Understanding what each application does, how they talk to each other, which settings need to match across tools, and how to diagnose which link in the chain broke when something fails — this is the real barrier. The result is that most sessions involve more time fighting software than actually imaging.

Even when everything is working, the workflow is disjointed: capture in one tool, check framing in another, monitor guiding in a third, then transfer files to a fourth for processing. There is no single coherent application where you can plan a session, control the hardware, see results building up in real time, and process the output — all in one place, from any device.

## 2. Vision

AstroCtl is a single, self-hosted application that replaces the patchwork of astrophotography tools with one coherent system. Mount control, camera operation, plate solving, guiding, live stacking, calibration management, and post-processing — all in one place, accessible from a phone or tablet over VPN, with an LLM co-pilot that lets you control the rig by voice in the dark.

It should just work. Connect hardware, open the app, pick a target, and start imaging. No wiring together five different programs, no matching configuration across tools, no debugging which link in the chain broke.

The guiding principles:

- **One application, end to end**: planning through post-processing in a single UI, a single configuration, a single log. No context-switching between tools, no file-copying between steps.
- **Direct control with full transparency**: every command sent to the mount, every camera parameter change, every guide correction is visible, logged, and scriptable. When something goes wrong, it's clear what happened and where.
- **ML enhances signal extraction, never fabricates signal**: machine learning may improve noise reduction, star separation, and background modeling, but must never hallucinate detail or blend in external data. The output is always a faithful representation of captured photons.
- **Natural language as a first-class interface**: an LLM agent with full tool-use access to the system's APIs lets the operator control the rig, tune processing, and get diagnostics via text or voice — particularly valuable when operating in the cold and dark with gloves on, or when you'd rather describe what you want than navigate menus.

Plate solving provides astrometric ground truth for goto correction and polar alignment, while live stacking gives immediate visual feedback as signal accumulates — no waiting until post-processing to know if the session is working.

## 3. Target User

Single operator running a portable or semi-permanent astrophotography rig:

- HEQ5 Pro mount (Skywatcher/Synta motor controller, serial over USB)
- Canon EOS R10 camera (PTP/MTP over USB)
- Linux field node (Ubuntu, likely a laptop or Pi at the rig)
- High-performance Linux desktop PC as stacking server (at home or nearby):
  - CPU: AMD Ryzen 9 (16+ cores)
  - RAM: 128 GB
  - GPU: NVIDIA RTX 4090 (24 GB VRAM)
  - Storage: NVMe SSD for active sessions, bulk storage for calibration library and archive
- All nodes connected via VPN (e.g., NetBird/Tailscale) — field node, stacking server, and operator's tablet/phone may be on different physical networks
- Controls the rig from a tablet or phone over the VPN, potentially not co-located with the field node
- Values understanding what the software is doing over GUI convenience
- May want to extend the system with custom automation or integrate with other tools

## 4. Hardware Scope

### 4.1 Hardware Abstraction Layer

All hardware is accessed through abstract interfaces. Concrete drivers implement these interfaces for specific devices. The session orchestrator, LLM agent, REST API, and UI interact only with the abstract interface — never with device-specific code. Swapping hardware means registering a new driver; no other code changes.

**Abstract interfaces:**

```
┌─────────────────────────────────────────────────────────────────┐
│              Session Orchestrator / LLM Agent / UI              │
└──────────────┬──────────────┬───────────────┬───────────────────┘
               │              │               │
        ┌──────┴──────┐ ┌────┴─────┐ ┌───────┴────────┐
        │ MountDevice │ │ Camera   │ │ GuideCamera    │
        │ (abstract)  │ │(abstract)│ │ (abstract)     │
        └──────┬──────┘ └────┬─────┘ └───────┬────────┘
               │              │               │
       ┌───────┼────┐    ┌───┼────┐     ┌────┼────┐
       │       │    │    │   │    │     │    │    │
     Skyw.  INDI  ASCOM gPh2 INDI ASI  QHY  INDI
     Serial  (future)   PTP  (f.) SDK  SDK  (f.)
```

**MountDevice interface:**

Every mount driver must implement this async interface. The interface defines what the system can do with any equatorial mount — the driver handles how.

```
MountDevice (abstract)
├── connect() / disconnect()
├── get_position() → (ra_hours, dec_degrees)
├── get_status() → MountStatus (tracking, slewing, parked, etc.)
├── goto(ra_hours, dec_degrees) → awaitable
├── sync(ra_hours, dec_degrees)
├── start_tracking(mode: sidereal | lunar | solar)
├── stop_tracking()
├── slew(axis, direction, speed) / stop_slew(axis)
├── guide_pulse(axis, direction, duration_ms, rate)
├── park() / unpark()
├── emergency_stop()
├── get_capabilities() → MountCapabilities
│   (max_slew_speed, has_pec, has_pulse_guide,
│    has_tracking_rates, position_resolution, etc.)
└── get_device_info() → (name, model, firmware, protocol)
```

**Camera interface:**

```
Camera (abstract)
├── connect() / disconnect()
├── get_settings() → CameraSettings
├── set_iso(value) / set_shutter(value) / set_aperture(value)
├── set_image_format(format)
├── get_available_settings() → (isos[], shutters[], apertures[], formats[])
├── capture() → CaptureResult (path, metadata)
├── capture_bulb(duration_seconds) → CaptureResult
├── abort_capture()
├── get_live_view_frame() → bytes (JPEG)
├── live_view_stream() → async iterator of JPEG frames
├── get_battery_level() → BatteryStatus (percent, charging)
├── get_storage_info() → StorageInfo
├── get_capabilities() → CameraCapabilities
│   (has_bulb, has_live_view, has_mirror_lockup,
│    sensor_width_px, sensor_height_px, pixel_size_um,
│    supported_formats, max_iso, min_shutter, etc.)
├── get_sensor_temperature() → float | None
└── get_device_info() → (name, model, serial, protocol)
```

**GuideCamera interface:**

```
GuideCamera (abstract)
├── connect() / disconnect()
├── set_exposure(seconds)
├── set_gain(value)
├── set_binning(x, y)
├── capture_frame() → raw pixel buffer (16-bit mono or Bayer, with dimensions)
├── start_continuous() / stop_continuous()
├── continuous_stream() → async iterator of frames
├── get_capabilities() → GuideCameraCapabilities
│   (sensor_width_px, sensor_height_px, pixel_size_um,
│    max_gain, has_cooling, min_exposure, max_exposure, etc.)
├── set_cooling(enabled, target_temp) → (if supported)
├── get_temperature() → float
└── get_device_info() → (name, model, serial, protocol)
```

**FilterWheel interface (future):**

```
FilterWheel (abstract)
├── connect() / disconnect()
├── get_position() → int
├── set_position(slot) → awaitable
├── get_filter_names() → list[str]
├── get_slot_count() → int
└── get_device_info() → (name, model, serial)
```

**Focuser interface (future):**

```
Focuser (abstract)
├── connect() / disconnect()
├── get_position() → int (steps)
├── move_to(position) → awaitable
├── move_relative(steps) → awaitable
├── get_capabilities() → FocuserCapabilities
│   (max_position, has_temperature_compensation, step_size_um)
├── get_temperature() → float | None
└── get_device_info() → (name, model, serial)
```

| ID | Requirement | Priority |
|----|------------|----------|
| HAL-01 | All hardware access goes through abstract device interfaces — session orchestrator, LLM agent, REST API, and UI never reference device-specific driver code | Must |
| HAL-02 | MountDevice abstract interface with async methods for connect, position, goto, tracking, slewing, guiding, park, sync, emergency stop, and capability inquiry | Must |
| HAL-03 | Camera abstract interface with async methods for connect, settings, capture, bulb, live view, battery, storage, sensor temperature, and capability inquiry | Must |
| HAL-04 | GuideCamera abstract interface with async methods for connect, exposure, gain, binning, continuous capture, cooling, and capability inquiry | Must |
| HAL-05 | Each interface includes a `get_capabilities()` method returning a structured object describing what the specific hardware supports — the system adapts its behavior to available capabilities (e.g., no PEC UI if mount doesn't support PEC) | Must |
| HAL-06 | Each interface includes a `get_device_info()` method returning human-readable device identification for logging, UI display, and calibration profile tagging | Must |
| HAL-07 | Drivers are registered by name in configuration — the operator selects which driver to use for each device slot (mount, camera, guide camera) without code changes | Must |
| HAL-08 | Driver discovery: list available drivers and auto-detect connected devices where the protocol supports it (e.g., gPhoto2 camera auto-detect, serial port scan for mounts) | Should |
| HAL-09 | INDI driver adapter: an adapter that wraps any INDI device as a MountDevice, Camera, or GuideCamera — enabling access to the full INDI ecosystem for hardware not directly supported | Should |
| HAL-10 | ASCOM/Alpaca driver adapter: an adapter wrapping ASCOM Alpaca REST devices for cross-platform compatibility with Windows-native equipment | Could |
| HAL-11 | Simulator drivers for mount, camera, and guide camera — generate synthetic data for development, testing, and demo without physical hardware | Should |
| HAL-12 | FilterWheel abstract interface for automated filter changes (future narrowband support) | Could |
| HAL-13 | Focuser abstract interface for motorized focus control (future autofocus support) | Could |
| HAL-14 | Driver hot-swap: disconnect one device and connect another of the same type (e.g., switch cameras) without restarting the application — the session orchestrator handles the transition | Could |

### 4.2 Mount — Sky-Watcher HEQ5 Pro (reference implementation)

Driver: `SkywatcherMount` implementing `MountDevice`.

Communication: Skywatcher motor controller protocol over USB-serial adapter (Prolific PL2303, FTDI, or CH340), 9600 baud 8N1.

Protocol characteristics:
- Command format `:CXDDDDDD\r`, response `=DDDDDD\r` or `!\r`
- Two independent axes (RA, DEC), each reporting a 24-bit position counter — an open-loop stepper step count, not encoder feedback. The HEQ5 Pro has no position encoders: lost steps (wind gust, cable snag, imbalance) are not detected by the mount and must be caught by plate-solve verification (see risk register)
- Counter home position at `0x800000`
- Hex values transmitted as ASCII, little-endian byte order
- Supports: position inquiry, goto (absolute/incremental), constant-speed tracking, guide pulses, variable-speed slewing, emergency stop

Key parameters. **Read from the operator's own HEQ5 Pro on 2026-07-29** — see
`spikes/skywatcher-heq5/FINDINGS.md`. The driver reads these at handshake and never hardcodes
them (SDD §5.2.3); the values below are for test fixtures and hand-verification:
- Counts per revolution: **9,024,000** — verified, both axes
- Timer interrupt frequency: **64,935 Hz** — read at handshake and **confirmed under motion**:
  slewing at step period 620 measured 104.617 counts/s against a sidereal rate of 104.7304, giving
  an implied timer frequency of 64,862 (0.11% agreement). *Previously documented here as ~460,800 Hz,
  which was wrong by a factor of 7.1 and would have predicted 743 counts/s. Any fixture or
  hand-computed step period built on the old figure is invalid.*
- **Sidereal tracking step period: 620** (= timer frequency ÷ sidereal rate). Measured directly
- **Goto speeds are fixed and NOT settable via the step period** — see §4.2 note below
- Counter home position: **`0x800000`** — verified, both axes read exactly home
- High-speed ratio: **16×** — verified via `:g`, both axes. Note the encoding: it returns two hex
  characters (`10`), *not* a byte-swapped u24, so the codec must not apply the u24 rule to it

Capabilities reported: `has_pec=False, has_pulse_guide=True, has_tracking_rates=[sidereal, lunar, solar], max_slew_speed=800x_sidereal, position_resolution=24bit`

**Measured motion behaviour** (`spikes/skywatcher-heq5/FINDINGS.md`), confirmed optically with a
camera observing the mount. GOTO **ramps** through a trapezoidal profile rather than running at a
fixed speed: a 250,667-count (10°) move accelerated over ~1.6 s to a cruise of **87,486 counts/s =
835× sidereal**, corroborating the 800× maximum above. Short gotos are ramp-limited and never reach
cruise — a 1,000-count move peaked at only 5,350 counts/s. GOTO **ignores the step period** (a 10×
change left the profile identical); `I` governs SLEW and tracking only. Goto lands on target with
0 counts of error, and both `K` and `L` arrest motion with ~84 counts of overshoot, which at that
rate is one serial round trip of command latency rather than deceleration.

### 4.3 Camera — Canon EOS R10 (reference implementation)

Driver: `CanonGPhoto2Camera` implementing `Camera`.

Communication: PTP/MTP over USB, accessed through the libgphoto2 C library via Rust bindings (`gphoto2` crate), with a `gphoto2` CLI subprocess fallback for operations the bindings don't cover.

Capabilities via gPhoto2:
- ISO, shutter speed, aperture, white balance control
- Single capture and bulb mode
- Live view preview stream (JPEG frames)
- Image download (CR3 RAW, JPEG)
- Battery and storage status

Capabilities reported: `has_bulb=True, has_live_view=True, sensor_width_px=6000, sensor_height_px=4000, pixel_size_um=3.72, supported_formats=[CR3, JPEG], has_mirror_lockup=False`

Note: the R10 is mirrorless — there is no mirror mechanism. Pixel pitch is ~3.72 µm (22.3 mm APS-C sensor across 6000 px), giving ~0.77″/px at 1000 mm focal length.

### 4.4 Guide Camera (Phase 3)

Not in initial scope. Phase 3 will add support for a dedicated guide camera.

Planned drivers implementing `GuideCamera`:
- `ASICamera` — ZWO ASI cameras via the ASI SDK
- `QHYCamera` — QHY cameras via the QHY SDK
- `INDIGuideCamera` — any INDI-compatible guide camera via the INDI adapter (HAL-09)

### 4.5 Simulator Drivers (development / testing)

- `SimulatorMount` implementing `MountDevice` — simulates position, slewing with realistic settle times, tracking drift, periodic error
- `SimulatorCamera` implementing `Camera` — generates synthetic star field images based on current mount position using a catalog, with configurable noise, FWHM, and background
- `SimulatorGuideCamera` implementing `GuideCamera` — generates guide frames from the simulated star field with configurable guide star brightness and seeing

## 5. Functional Requirements

### 5.1 Mount Control

| ID | Requirement | Priority |
|----|------------|----------|
| MNT-01 | Connect/disconnect to mount via serial port auto-detection or manual port selection | Must |
| MNT-02 | Read and display current RA/DEC position in real time (1 Hz minimum) | Must |
| MNT-03 | Convert and display Alt/Az for the configured observing site | Must |
| MNT-04 | Start/stop sidereal, lunar, and solar tracking on the RA axis | Must |
| MNT-05 | Manual slew in N/S/E/W at selectable speeds (guide, slow, medium, fast, max) | Must |
| MNT-06 | Goto a target by RA/DEC coordinates (absolute position-counter targeting) | Must |
| MNT-07 | Wait for slew completion with timeout and status reporting | Must |
| MNT-08 | Emergency stop (immediate, no deceleration) accessible from all UI states | Must |
| MNT-09 | Park mount to a configured home position | Should |
| MNT-10 | Sync/align position after plate solving or manual star alignment (required by the solve-and-center loop, PLS-03) | Must |
| MNT-11 | Meridian flip detection and warning (hours until flip, auto-flip in sequences) | Should |
| MNT-12 | Send guide correction pulses (RA/DEC, configurable rate and duration) | Should |
| MNT-13 | Periodic error correction: record and playback PEC curve | Could |
| MNT-14 | Multi-star alignment routine (2-star, 3-star) | Could |
| MNT-15 | Slew limits: configurable minimum altitude (horizon limit) — goto/slew targets below the limit are rejected at the mount-control layer for all callers (UI, REST API, LLM agent) | Must |
| MNT-16 | Meridian/RA-axis limits to prevent pier or tripod collision: configurable track-past-meridian limit with automatic tracking stop when reached | Should |

### 5.2 Camera Control

| ID | Requirement | Priority |
|----|------------|----------|
| CAM-01 | Connect/disconnect with auto-detection of Canon R10 on USB | Must |
| CAM-02 | Read and set ISO, shutter speed, aperture, image format | Must |
| CAM-03 | Single-frame capture with image download to local storage | Must |
| CAM-04 | Bulb mode capture with configurable duration | Must |
| CAM-05 | Live view stream for framing and focusing (JPEG frames over WebSocket) | Must |
| CAM-06 | Display last captured frame with auto-stretch (screen transfer function) | Should |
| CAM-07 | Capture sequence: N frames × exposure, with configurable delay between frames | Should |
| CAM-08 | Battery level and storage card status monitoring | Should |
| CAM-09 | Histogram display for last captured or live view frame | Could |
| CAM-10 | Focus assistant with HFR (Half-Flux Radius) measurement from live view | Could |
| CAM-11 | Frame-and-focus mode with continuous short exposures and star detection overlay | Could |

### 5.3 Session Orchestration

| ID | Requirement | Priority |
|----|------------|----------|
| SES-01 | Define a capture sequence: target, exposure time, frame count, ISO, format | Must |
| SES-02 | Execute sequence: slew → settle → capture → repeat, with progress reporting | Must |
| SES-03 | Abort/pause/resume a running sequence | Must |
| SES-04 | Multi-target queue: list of targets executed in order | Should |
| SES-05 | Dithering between frames (random offset via mount guide pulses) | Should |
| SES-06 | Auto resume sidereal tracking after goto completes | Should |
| SES-07 | Session logging: all events, commands, and results to a structured log file | Should |
| SES-08 | Automatic meridian flip handling mid-sequence (required for unattended multi-hour sessions — Phase 3 exit criteria) | Should |
| SES-09 | Calibration frame automation: darks, flats, bias sequences | Could |
| SES-10 | Weather/abort triggers via external sensor integration (future) | Won't (Phase 4+) |

### 5.4 Session Planning

| ID | Requirement | Priority |
|----|------------|----------|
| PLN-01 | Configure observing site: latitude, longitude, elevation, timezone | Must |
| PLN-02 | Calculate and display Local Sidereal Time | Must |
| PLN-03 | Target catalog: Messier objects with RA/DEC, common name, type | Should |
| PLN-04 | Target altitude/azimuth calculation for current or future time | Should |
| PLN-05 | Altitude chart: plot target altitude over the night for session planning | Could |
| PLN-06 | NGC/IC catalog and custom target list support | Could |
| PLN-07 | Mosaic panel calculator for multi-panel imaging | Could |
| PLN-08 | Optimal target ordering by altitude and transit time | Could |

### 5.5 Guiding (Phase 3)

| ID | Requirement | Priority |
|----|------------|----------|
| GDE-01 | Star detection in guide camera frames using `sep` (Source Extractor) | Should |
| GDE-02 | Centroid tracking with sub-pixel accuracy | Should |
| GDE-03 | PI (proportional-integral) correction controller with configurable aggressiveness | Should |
| GDE-04 | Guide performance display: RMS error, correction history graph | Should |
| GDE-05 | Aggressive/conservative guide profiles | Could |

### 5.6 Plate Solving

Plate solving determines the exact sky coordinates of a captured image by matching star patterns against an index catalog. AstroCtl supports two solver backends behind a common interface, with all solving performed locally (no internet required).

**Solver backends:**

- **astrometry.net** (local `solve-field`): robust, handles wide fields and poor initial guesses, requires ~2-8 GB index files depending on field of view coverage, typical solve time 5-30s.
- **ASTAP** (CLI `astap`): faster (1-3s typical), smaller star database (~1 GB), requires a reasonable initial position hint, better suited for iterative re-solves during sequences.

The backend is selected via configuration. Both produce the same output: solved RA/DEC center, field rotation, pixel scale, and a WCS (World Coordinate System) header.

| ID | Requirement | Priority |
|----|------------|----------|
| PLS-01 | Common solver interface abstracting over astrometry.net and ASTAP backends | Must |
| PLS-02 | Solve a captured JPEG or FITS image and return center RA/DEC, rotation, pixel scale, and field of view | Must |
| PLS-03 | Goto correction: after a slew, capture a short exposure, solve, sync mount position, and optionally re-slew to center the target (solve-and-center loop) | Must |
| PLS-04 | Configurable solve parameters: search radius, downsample factor, timeout, depth limit | Must |
| PLS-05 | Continuous pointing verification: periodically solve during a capture sequence and report drift from target center | Should |
| PLS-06 | Automatic framing: solve current position, compute pixel offset to desired target center, issue corrective slew, re-solve to confirm | Should |
| PLS-07 | Polar alignment assistant: solve at three or more RA rotation positions (three-point plate-solve method, as in SharpCap/N.I.N.A.), compute polar alignment error in altitude and azimuth, display correction instructions | Should |
| PLS-08 | Solve result overlay: annotate the last captured/solved image with detected stars, solved grid lines, and DSO labels in the UI | Could |
| PLS-09 | Blind solve (no position hint) as fallback when mount position is unknown or unreliable | Could |
| PLS-10 | Solver performance logging: time-to-solve, stars matched, confidence, for tuning index file selection | Could |

**Solve-and-center loop (PLS-03 + PLS-06):**

```
1. Slew to target RA/DEC
2. Wait for settle
3. Capture short exposure (2-5s)
4. Solve image → actual RA/DEC
5. Compute offset from target
6. If offset > threshold (e.g., 1 arcmin):
     Sync mount to solved position
     Re-slew to target
     Go to step 2 (max 3 iterations)
7. Begin imaging sequence
```

**Polar alignment assistant (PLS-07):**

```
1. Point near celestial pole, solve → position A
2. Rotate RA by 90°, solve → position B
3. Rotate RA by another 90°, solve → position C
4. Fit circle through A, B, C
5. Circle center offset from pole = polar alignment error
6. Display: "adjust altitude by X arcmin, azimuth by Y arcmin"
```

### 5.7 Live Stacking

Live stacking aligns and combines incoming frames in real time, producing a progressively deeper image during the capture session. This serves both as immediate visual feedback (is the target framed? is the signal building up?) and as a usable output that can be exported.

**Distributed architecture:**

The stacking pipeline runs on a separate high-performance PC (the "stacking server" — Ryzen 9, 128 GB RAM, RTX 4090 24 GB VRAM), not on the field computer controlling the mount and camera. The field node captures frames, saves them to disk, and streams them over the network to the stacking server. This separation keeps the field node lightweight and responsive for hardware control, while the stacking server has the CPU, RAM, and GPU resources to process full-resolution frames without compromise.

With 128 GB RAM, the stacking server can hold 300+ full-resolution frames in memory simultaneously (24 MP × 3 channels × 32-bit float ≈ 288 MB per frame), enabling true per-pixel median and sigma-clip stacking without approximation or streaming. The RTX 4090 accelerates frame registration, pixel rejection, debayering, ML inference, and post-processing operations.

```
                         VPN Network (NetBird / Tailscale)
          ┌──────────────────────┼──────────────────────────┐
          │                      │                          │
Field Node (laptop/Pi)    Stacking Server (desktop PC)   Operator Device
┌─────────────────────┐   ┌──────────────────────────┐   (tablet/phone)
│ Mount driver        │   │ Stacking engine          │   ┌────────────┐
│ Camera driver       │   │   Star detection (sep)   │   │ Browser    │
│ Session orchestrator│──▶│   Registration           │   │ (Web UI)   │
│ Plate solver        │   │   Accumulation           │   │            │
│ Frame transfer agent│   │   Calibration (dark/flat) │   │ Controls   │
│                     │◀──│   Stretch + preview      │   │ both nodes │
│ Web UI backend ─────│───│── Export (FITS/TIFF)     │   │ via field  │
│  (proxies stacking  │   │                          │   │ node proxy │
│   server endpoints) │   │ Calibration library      │   └──────┬─────┘
└─────────────────────┘   └──────────────────────────┘          │
          ▲                          ▲                          │
          └──────────────────────────┴──────────────────────────┘
                     REST + WebSocket over VPN
```

Communication between nodes over VPN:

- **Frame transfer:** field node pushes each new frame to the stacking server over the VPN. Options: HTTP upload endpoint on the stacking server (simplest over VPN), or rsync over SSH. Frames are transferred after being saved locally (REL-05 is preserved — the field node never depends on successful transfer to save the raw frame). Transfer is queued and tolerant of VPN latency/drops.
- **Preview stream:** stacking server pushes the current stretched preview back to the operator's browser via WebSocket (either directly or proxied through the field node, depending on VPN topology).
- **Operator control:** the operator's tablet/phone connects to the field node's web UI over VPN. The field node proxies stacking server endpoints so the operator has a single URL. All mount control, camera control, sequence management, and stacking preview are accessible from this single interface.
- **Status:** stacking statistics (frame count, rejected count, integration time, SNR, FWHM) streamed back over WebSocket.
- **Latency tolerance:** mount control commands are latency-sensitive but small (< 1 KB); frame transfers are large but latency-tolerant (queued). The architecture separates these concerns — time-critical operations (emergency stop, guide corrections) execute on the field node without round-tripping through the VPN.

The stacking server can also be the same machine as the field node for development/testing — the architecture degrades gracefully to a single process with in-memory frame passing.

| ID | Requirement | Priority |
|----|------------|----------|
| STK-01 | Detect stars in each incoming frame for registration (using `sep` or similar) | Must |
| STK-02 | Register (align) each new frame to the reference frame using affine or projective transform derived from matched star positions | Must |
| STK-03 | Accumulate aligned frames into a running stack using selectable method: mean, median, sigma-clipped mean, or kappa-sigma rejection | Must |
| STK-04 | Apply a screen transfer function (auto-stretch: midtone transfer, histogram equalization, or asinh stretch) to the current stack for display | Must |
| STK-05 | Stream the current stacked + stretched preview to the browser UI via WebSocket, updated within 1s of each new frame's accumulation (see PRF-09) — an update-latency requirement, since the stack only changes once per captured frame | Must |
| STK-06 | Export the current stack as a 16-bit FITS or 16-bit TIFF at any point during or after the session | Must |
| STK-07 | Export the current stack as a stretched 8-bit JPEG/PNG for quick sharing | Should |
| STK-08 | Automatic reference frame selection: use the first successfully solved or best-SNR frame as the registration reference | Should |
| STK-09 | Frame rejection: automatically exclude frames with excessive trailing, poor star count, or high background (clouds) | Should |
| STK-10 | Display live stacking statistics in the UI: frame count, rejected count, total integration time, SNR estimate, FWHM trend | Should |
| STK-11 | Per-channel (RGB) stacking for color data with separate accumulation buffers | Should |
| STK-12 | Dark frame subtraction from calibration library before stacking (auto-matched by profile) | Must |
| STK-13 | Flat field correction from calibration library before stacking (auto-matched by profile) | Must |
| STK-14 | Bias/offset frame subtraction from calibration library | Should |
| STK-15 | Drizzle integration for sub-pixel resolution enhancement (2x drizzle) | Could |
| STK-16 | Live stacking operates independently from image saving — raw frames are always written to disk on the field node regardless of stacking state or network availability (inherits REL-05) | Must |
| STK-17 | Frame transfer agent on field node: push frames to stacking server over network with retry and queue (frames not lost if stacking server is temporarily unreachable) | Must |
| STK-18 | Stacking server exposes REST API + WebSocket for control, status, and preview | Must |
| STK-19 | Field node UI proxies stacking server endpoints — single browser interface for the operator | Should |
| STK-20 | Stacking server can run on the same machine as field node (single-process fallback for development/testing) | Should |
| STK-21 | Stacking pipeline processes full-resolution debayered frames (no JPEG downgrade) since compute is not constrained on the stacking server | Should |

**Stacking methods and configuration:**

The stacking method determines how pixel values from aligned frames are combined into the final stack. Each method has different tradeoffs between noise reduction, outlier rejection, and computational cost. All methods operate per-pixel across the frame stack.

| Method | Description | Parameters | Live-capable | Notes |
|--------|------------|-----------|:------------:|-------|
| **Mean (average)** | Arithmetic mean of all frames per pixel. Best SNR improvement (√N) but no outlier rejection — satellites, aircraft, cosmic rays all accumulate. | `weight_mode` | Yes | Running mean: O(1) memory per pixel. Fastest. Good starting point. |
| **Weighted mean** | Mean weighted by per-frame quality metric (inverse variance, SNR, FWHM). Better frames contribute more. | `weight_mode`, `weight_metric` | Yes | Running weighted sum: O(1) memory. |
| **Median** | Median value per pixel. Robust to outliers (50% breakdown point) but ~20% less SNR than mean. | — | Partial | Requires storing all frames or using running median approximation. Full median needs all frames in memory — triggers re-stack. |
| **Sigma-clipped mean** | Iteratively reject pixels beyond σ thresholds, then mean the survivors. Best balance of SNR and rejection. | `sigma_low`, `sigma_high`, `max_iterations` | Partial | Requires running statistics (mean + variance) per pixel, or all frames for full recomputation. Approximation possible for live; full accuracy on re-stack. |
| **Kappa-sigma rejection** | Similar to sigma-clip but uses median absolute deviation (MAD) as the robust scale estimator instead of standard deviation. More resistant to skewed outliers. | `kappa`, `max_iterations` | Partial | Same memory tradeoff as sigma-clip. |
| **Winsorized sigma-clip** | Sigma-clip but replaces rejected values with the threshold value instead of discarding. Preserves frame count contribution. | `sigma_low`, `sigma_high`, `max_iterations` | Partial | Useful when few frames available. |
| **Min/Max clip** | Discard the N highest and N lowest values per pixel, then mean the rest. Simple, effective for small stacks. | `clip_low`, `clip_high` (frame count) | No | Requires all frames. Good for < 20 frames. |
| **Linear fit rejection** | Fit a line to pixel values over time, reject outliers from the fit. Handles gradual changes (sky brightness drift). | `sigma`, `max_iterations` | No | Requires all frames + timestamps. Computationally expensive. |

| ID | Requirement | Priority |
|----|------------|----------|
| STK-22 | Core stacking methods selectable per session: mean, weighted mean, median, sigma-clipped mean, kappa-sigma (extended methods are STK-34) | Must |
| STK-23 | Sigma-clip parameters configurable: `sigma_low` (default 2.5), `sigma_high` (default 3.0), `max_iterations` (default 5) | Must |
| STK-24 | Frame weighting mode selectable: equal (unweighted), by SNR, by FWHM (inverse — sharper frames weighted higher), by background level (inverse — darker sky weighted higher), custom per-frame weight | Must |
| STK-25 | Frame normalization mode selectable: none, by mean (additive), by mean (multiplicative), by median, by background region — applied before stacking to equalize brightness across frames taken under varying conditions | Must |
| STK-26 | Per-frame manual include/exclude: operator can toggle individual frames in/out of the stack from the UI, with immediate re-stack | Should |
| STK-27 | Automatic outlier frame rejection: reject entire frames (not just pixels) based on configurable thresholds — FWHM > X, star count < N, background > Y, eccentricity > E (trailing detection) | Should |
| STK-28 | Sub-frame quality metrics computed and displayed per frame: FWHM (arcsec), eccentricity, star count, background ADU, noise estimate, SNR, weight assigned | Should |
| STK-29 | Registration method selectable: affine (translation + rotation + scale), projective (full 8-parameter homography), or translation-only (for well-aligned data) | Should |
| STK-30 | Registration star matching: minimum star count threshold, maximum residual threshold, configurable detection sensitivity (sep threshold parameter) | Should |
| STK-31 | Reference frame selection: automatic (best quality), manual (operator picks), or explicit frame number | Should |
| STK-32 | Live stacking approximation mode: for methods that require all frames (median, sigma-clip), use a running approximation during live capture, then offer full-accuracy re-stack on demand or at session end | Should |
| STK-33 | Debayer method selectable: bilinear, VNG, AHD, DCB — controls how the Bayer-pattern RAW sensor data is interpolated to RGB | Should |
| STK-34 | Extended stacking methods: winsorized sigma-clip, min/max clip, linear fit rejection (not live-capable, computationally expensive — see method table) | Could |

**Stacking pipeline per frame (on stacking server):**

```
1. Frame arrives over network from field node
2. Look up matching calibration profile (§5.8) by equipment + exposure metadata
3. Subtract master bias (if available)
4. Subtract master dark (matched by exposure, ISO, temperature)
5. Divide by master flat (matched by telescope, camera, filter)
6. Detect stars using sep (STK-01)
7. If first frame or reference: set as registration reference
8. Match stars against reference, compute transform (STK-02)
9. If match fails or quality check fails → reject frame (STK-09)
10. Warp frame to align with reference
11. Add to accumulator using selected method (STK-03)
12. Apply stretch to current stack (STK-04)
13. Push stretched preview to UI via WebSocket (STK-05)
```

### 5.8 Calibration Library

Calibration frames (darks, flats, biases) are reusable across sessions when the equipment and conditions match. AstroCtl maintains a structured calibration library on the stacking server, indexed by equipment profile and exposure parameters, so master calibration frames are built once and automatically applied to matching light frames.

**Equipment profile:**

A calibration profile is defined by the combination of:
- **Telescope/optic** (e.g., "Skywatcher 200PDS f/5", "Samyang 135mm f/2")
- **Camera** (e.g., "Canon R10")
- **Filter** (e.g., "L", "Ha", "none" for unfiltered)

This triplet identifies which flat and bias frames are valid. Darks additionally depend on:
- **Exposure time** (e.g., 120s, 300s)
- **ISO / gain** (e.g., ISO 1600)
- **Sensor temperature** (within a tolerance band, e.g., ±2°C)

**Master frame generation:**

Individual calibration sub-frames (e.g., 30 dark frames at 120s ISO 1600) are stacked using median or sigma-clipped mean to produce a master frame. The library stores both the master and metadata about how it was produced (sub-frame count, date, method).

| ID | Requirement | Priority |
|----|------------|----------|
| CAL-01 | Define equipment profiles: telescope + camera + filter combinations, stored as named configurations | Must |
| CAL-02 | Store master dark frames indexed by profile + exposure time + ISO + sensor temperature (±tolerance) | Must |
| CAL-03 | Store master flat frames indexed by profile (telescope + camera + filter) | Must |
| CAL-04 | Store master bias/offset frames indexed by profile + ISO | Should |
| CAL-05 | Automatic profile matching: given a light frame's EXIF/FITS metadata, find the best matching master dark, flat, and bias from the library | Must |
| CAL-06 | Temperature tolerance for dark matching: configurable (default ±2°C), with warning if no dark within tolerance exists; when sensor temperature is unavailable in frame metadata (the Canon R10 EXIF may not carry it), fall back to matching by session-date proximity or manual annotation | Should |
| CAL-07 | Master frame generation: stack N calibration sub-frames into a master using median or sigma-clipped mean | Must |
| CAL-08 | UI for managing the calibration library: list profiles, view masters, upload sub-frames, generate masters, delete obsolete entries | Should |
| CAL-09 | Capture calibration sub-frames from the camera directly (dark sequence, flat sequence) with metadata auto-populated from current equipment profile | Should |
| CAL-10 | Library storage on the stacking server filesystem with a metadata index (JSON or SQLite) | Must |
| CAL-11 | Import existing master frames (FITS) into the library with manual metadata tagging | Should |
| CAL-12 | Library reports: for each profile, show available calibration coverage (which exposure/ISO/temp combinations have masters, which are missing) | Could |
| CAL-13 | Expiry/staleness tracking: flag master darks older than a configurable age (default 6 months) for re-acquisition | Could |

**Calibration library structure (on stacking server):**

```
/data/astro/calibration/
├── library.json                     # metadata index
├── profiles/
│   ├── sw200pds-r10-none/           # profile: telescope-camera-filter
│   │   ├── profile.json             # equipment details
│   │   ├── darks/
│   │   │   ├── 120s_iso1600_-5c/    # exposure_iso_temp
│   │   │   │   ├── master_dark.fits
│   │   │   │   └── meta.json        # sub-frame count, date, method
│   │   │   └── 300s_iso1600_-5c/
│   │   │       ├── master_dark.fits
│   │   │       └── meta.json
│   │   ├── flats/
│   │   │   ├── master_flat.fits
│   │   │   └── meta.json
│   │   └── bias/
│   │       ├── master_bias.fits
│   │       └── meta.json
│   └── samyang135-r10-none/
│       └── ...
```

**Profile matching algorithm (CAL-05):**

```
Given a light frame with metadata:
  telescope = "SW 200PDS"
  camera = "Canon R10"
  filter = "none"
  exposure = 120s
  iso = 1600
  sensor_temp = -4.2°C

1. Find profile matching telescope + camera + filter
2. Find master flat for that profile → apply
3. Find master bias for that profile + ISO → apply
4. Find master dark for that profile + exposure + ISO
   where |dark_temp - frame_temp| ≤ tolerance
   → if multiple, pick closest temperature
   → if none within tolerance, warn and skip (or scale nearest)
5. Return matched calibration set
```

### 5.9 Image Processing Pipelines

Every captured frame flows through up to three independent processing pipelines, each with a different priority. The pipelines are decoupled — they consume the same source frames but operate at different speeds, quality levels, and on different nodes. The raw frames are always preserved on disk for reprocessing.

**The three pipelines:**

```
                          ┌─────────────────────────────────────┐
                          │        Raw Frame (CR3 / FITS)       │
                          │    Always saved to disk first (REL-05)│
                          └──────┬──────────┬──────────┬────────┘
                                 │          │          │
                    ┌────────────┴┐   ┌─────┴────┐  ┌─┴──────────────┐
                    │  CONTROL    │   │  LIVE    │  │  PROCESSED     │
                    │  PIPELINE   │   │  VIEW    │  │  PIPELINE      │
                    │             │   │          │  │                │
                    │ Plate solve │   │ Debayer  │  │ Calibrate      │
                    │ Star detect │   │ Stretch  │  │ Register       │
                    │ FWHM/HFR   │   │ Annotate │  │ Stack          │
                    │ Centroid    │   │          │  │ Stretch        │
                    │             │   │          │  │ (configurable) │
                    │ FIELD NODE  │   │ FIELD    │  │ STACKING       │
                    │ < 3s        │   │ NODE     │  │ SERVER         │
                    │             │   │ < 1s     │  │ < 10s          │
                    └─────────────┘   └──────────┘  └────────────────┘
                    Drives mount        Operator      Builds final
                    corrections         feedback      result
```

**1. Control pipeline** (field node, latency-critical)

Purpose: extract astrometric and quality data from frames to drive automated decisions — plate solving for goto correction, star detection for guiding, FWHM measurement for focus, frame quality assessment for rejection. This pipeline must be as fast as possible because the session orchestrator blocks on its output (e.g., solve-and-center cannot proceed until the solve completes).

Optimization: operates on downsampled or JPEG-preview data where full resolution is not needed. Plate solving uses 2-4x downsampled images. Star detection for guiding uses binned sub-frames. FWHM can be measured from a central crop. The control pipeline never processes the full-resolution debayered frame unless accuracy requires it.

**2. Live view pipeline** (field node, near-real-time)

Purpose: provide the operator with a current visual of what the camera sees — for framing, focusing, and monitoring. This is the live view stream from the camera (CAM-05) plus a quick stretch/annotate pass on the most recent captured frame. Not accumulated — each frame is displayed independently, then replaced by the next.

Optimization: uses the camera's own JPEG preview stream for live view. For captured frame preview, debayers and stretches at reduced resolution (e.g., 1/4 scale) for speed. Can optionally overlay detected stars, solved grid, and crosshairs.

**3. Processed pipeline** (stacking server, quality-critical)

Purpose: produce the best possible stacked image from the session's frames. Full calibration (dark, flat, bias from the calibration library), full-resolution debayer, precise registration, configurable accumulation and stretch. This is the pipeline described in §5.7 (live stacking), running on the stacking server.

Key property: **the processed pipeline is configurable and re-runnable — both during and after a session.** The operator can adjust stacking method, sigma rejection thresholds, stretch parameters, calibration frame selection, and reference frame choice at any time. During a live session, changing parameters triggers a re-stack of all frames accumulated so far, with new frames continuing to flow in under the updated settings. After a session, the entire frame set can be reprocessed from scratch. This means:

- All raw frames from a session are stored permanently (not just the stack result)
- Session metadata (frame order, timestamps, solve results, guide corrections, rejected frame list) is stored alongside
- The processed pipeline can be re-invoked at any point — mid-session or post-session — with different parameters
- Mid-session parameter changes take effect immediately: the accumulator is rebuilt from stored frames using the new settings, then resumes ingesting new frames
- Multiple processed outputs can coexist for the same session (e.g., different stretch settings, different rejection thresholds)

| ID | Requirement | Priority |
|----|------------|----------|
| IPP-01 | Three independent image processing pipelines: control (latency-critical), live view (near-real-time), and processed (quality-critical) — decoupled, consuming the same source frames | Must |
| IPP-02 | Control pipeline runs on the field node; star detection, FWHM/HFR, and quality scoring complete in < 3s per frame using downsampled/preview data. Plate solving is budgeted separately (PRF-06: ≤ 5s with ASTAP) and only gates the sequence during solve-and-center | Must |
| IPP-03 | Control pipeline outputs: plate solve result (RA/DEC/rotation/scale), detected star list with centroids, FWHM/HFR measurement, frame quality score | Must |
| IPP-04 | Live view pipeline runs on the field node with target latency < 1s per frame; uses camera JPEG preview stream for live view and reduced-resolution debayer for captured frame preview | Must |
| IPP-05 | Live view pipeline outputs: stretched JPEG for display, optional star overlay and solved grid annotation | Should |
| IPP-06 | Processed pipeline runs on the stacking server with full-resolution calibrated frames, non-blocking (never gates capture). **End-to-end budget ≤ 10s per frame**, measured from download completion on the field node to the updated preview being visible in the operator's browser — this envelope contains transfer (PRF-07), ingest, the compute step (PRF-08), and the preview push (PRF-09) | Must |
| IPP-07 | Processed pipeline is configurable: stacking method, rejection parameters, stretch function, calibration selection, reference frame — all adjustable at any time, including during a live capture session | Must |
| IPP-08 | Processed pipeline is re-runnable both mid-session and post-session: changing parameters mid-session rebuilds the accumulator from all frames captured so far under the new settings, then resumes ingesting new frames; post-session reprocessing re-stacks all frames from scratch | Must |
| IPP-09 | All raw frames from every session are stored permanently; the stacking server archive is authoritative — the field-node copy may be reclaimed only after checksum-verified transfer, per the retention policy (REL-13). Frames are never deleted by the processing pipelines | Must |
| IPP-10 | Session metadata stored alongside raw frames: frame sequence, timestamps, per-frame solve results, guide corrections, quality scores, rejection decisions, equipment profile, capture parameters | Must |
| IPP-11 | Multiple processed outputs can coexist for the same session (different parameter sets produce different stacks, all retained) | Should |
| IPP-12 | Processing pipeline configuration is saved as a named preset (YAML/JSON) that can be applied to future sessions | Should |
| IPP-13 | Reprocessing UI: adjust pipeline parameters and re-run on the current live session or any completed session, preview result, compare with previous output, export — accessible from the operator's device | Should |
| IPP-14 | Control and live view pipelines do not depend on the stacking server — they run entirely on the field node even if the stacking server is unreachable | Must |
| IPP-15 | Processed pipeline tolerates late-arriving frames (e.g., if VPN was down during capture, frames arrive after session ends) — frames are incorporated into the stack in arrival order or re-sorted by timestamp | Should |
| IPP-16 | Mid-session parameter change triggers an asynchronous re-stack of existing frames — the rebuild runs in the background on the stacking server while capture continues uninterrupted on the field node; new frames arriving during the rebuild are queued and applied once the rebuild completes | Must |
| IPP-17 | Post-processing and stretch parameter changes (PPR-*) apply to the current accumulator without a full re-stack — only changes to stacking method, rejection thresholds, calibration, or reference frame require a rebuild; the post-processing chain caches intermediate results (PPR-30) so late-step changes are near-instant | Should |

**Post-processing tools (processed pipeline):**

The processed pipeline is not just "stack and stretch" — it includes a configurable chain of post-processing operations that run on the stacked image. These are the standard tools astrophotographers use in dedicated software (Siril, PixInsight, APP), brought into the AstroCtl pipeline so results are reproducible, presetable, and adjustable mid-session or post-session without exporting to a separate tool.

All post-processing operations are non-destructive: they operate on the stacked image in memory, are defined as an ordered list of steps in the pipeline configuration, and can be reordered, added, removed, or re-parameterized at any time. The raw stack (pre-post-processing) is always preserved.

```
Raw frames → Calibrate → Register → Stack → [ Post-processing chain ] → Export
                                                     │
                                          ┌──────────┴──────────────┐
                                          │ Ordered list of steps:  │
                                          │ 1. Background extract   │
                                          │ 2. Color calibrate      │
                                          │ 3. Stretch              │
                                          │ 4. Noise reduce (lum)   │
                                          │ 5. Curves (per-channel) │
                                          │ 6. Saturation boost     │
                                          │ 7. Star reduce          │
                                          │ 8. Sharpen              │
                                          │ 9. Crop/rotate          │
                                          │ (any order, any subset) │
                                          └─────────────────────────┘
```

| ID | Requirement | Priority |
|----|------------|----------|
| **Stretch & tone mapping** | | |
| PPR-01 | Multiple stretch functions: asinh, midtone transfer (STF), histogram transformation, logarithmic, gamma, generalized hyperbolic stretch (GHS) — each with adjustable parameters | Must |
| PPR-02 | Curves adjustment: master (luminance) and per-channel (R, G, B) curves with arbitrary control points | Must |
| PPR-03 | Levels adjustment: black point, midtone, white point — master and per-channel | Must |
| PPR-04 | Histogram display with per-channel overlays, updated live as parameters change | Must |
| **Color & channel operations** | | |
| PPR-05 | RGB channel separation: view and adjust R, G, B channels independently | Must |
| PPR-06 | Color calibration: photometric color calibration using plate-solved star field and catalog star colors (Gaia, APASS) | Should |
| PPR-07 | White balance adjustment: manual color temperature / tint sliders, eyedropper on background region, auto-neutral background | Must |
| PPR-08 | Channel mixing / RGB combination: adjust channel weights, useful for blending luminance with color data | Should |
| PPR-09 | Saturation and vibrance controls (global and per-hue selective saturation) | Should |
| PPR-10 | SCNR (Subtractive Chromatic Noise Reduction): remove green/magenta cast common in OSC (one-shot color) astrophotography | Should |
| PPR-11 | Narrowband palette mapping: for future narrowband filter support — assign SII/Ha/OIII channels to RGB (SHO Hubble palette, HOO, custom mappings) | Could |
| **Background & gradient** | | |
| PPR-12 | Background extraction / gradient removal: model and subtract sky gradients caused by light pollution, moon, vignetting residual (polynomial or RBF model with sample point selection) | Must |
| PPR-13 | Background neutralization: set a target neutral background value after gradient removal | Should |
| **Noise reduction** | | |
| PPR-14 | Luminance noise reduction: wavelet-based or non-local means denoising on the luminance channel with adjustable strength and detail preservation | Should |
| PPR-15 | Chrominance noise reduction: separate color noise reduction with adjustable strength | Should |
| PPR-16 | Multiscale noise reduction: target noise at specific spatial scales while preserving structure at others | Could |
| **Sharpening & detail** | | |
| PPR-17 | Unsharp mask with adjustable radius, strength, and threshold | Should |
| PPR-18 | Wavelet sharpening: enhance detail at selectable spatial scales (small-scale for stars, large-scale for nebula structure) | Could |
| PPR-19 | Deconvolution: Richardson-Lucy or Wiener deconvolution with estimated PSF for resolution recovery | Could |
| **Star operations** | | |
| PPR-20 | Star-starless separation: decompose the image into a star layer and a starless layer using star detection and morphological operations or neural network model | Should |
| PPR-21 | Star reduction: shrink star sizes by a configurable factor (morphological erosion or transfer function) | Should |
| PPR-22 | Starless processing: apply stretch/color/noise operations to the starless layer independently, then recombine with the star layer | Could |
| PPR-23 | Star color preservation: maintain natural star colors through aggressive stretching | Should |
| **Geometry** | | |
| PPR-24 | Crop to remove stacking borders (auto-detect or manual) | Must |
| PPR-25 | Rotation correction: level the image by field rotation or manual angle | Should |
| PPR-26 | Resample / downsample for export at different resolutions | Should |
| **Annotation & export** | | |
| PPR-27 | Annotation overlay: label DSOs, stars, constellation lines, field of view — using WCS from plate solve | Could |
| PPR-28 | Export in multiple formats from the same pipeline: 16-bit FITS, 16-bit TIFF, 8-bit JPEG/PNG — each with optional format-specific settings (JPEG quality, TIFF compression) | Must |
| **Pipeline mechanics** | | |
| PPR-29 | Post-processing steps are an ordered list in the pipeline config — steps can be added, removed, reordered, enabled/disabled, and re-parameterized without re-stacking | Must |
| PPR-30 | Each step in the chain operates on the output of the previous step; intermediate results are cached so changing a late step doesn't recompute earlier steps | Should |
| PPR-31 | Before/after comparison: toggle any step on/off and see the result instantly, or compare two pipeline configurations side by side | Should |
| PPR-32 | Undo/redo for parameter changes in the post-processing chain | Should |
| PPR-33 | All post-processing parameters are included in pipeline presets (IPP-12) — a preset captures the full chain: stacking settings + post-processing steps + parameters | Must |

**Session storage structure:**

```
/data/astro/sessions/
├── 2026-03-15_m42/
│   ├── session.json              # metadata: target, equipment, parameters, timeline
│   ├── frames/
│   │   ├── light_001.cr3         # raw frames, never modified
│   │   ├── light_002.cr3
│   │   └── ...
│   ├── control/
│   │   ├── solve_001.json        # per-frame solve results
│   │   ├── quality_001.json      # FWHM, star count, background
│   │   └── ...
│   ├── processed/
│   │   ├── default/
│   │   │   ├── pipeline.yaml     # pipeline configuration used
│   │   │   ├── stack_16bit.fits  # output stack
│   │   │   ├── stack_preview.jpg # stretched preview
│   │   │   └── processing.log
│   │   └── reprocess_v2/         # reprocessed with different params
│   │       ├── pipeline.yaml
│   │       ├── stack_16bit.fits
│   │       └── ...
│   └── guide/                    # guide corrections log (Phase 3)
│       └── corrections.csv
```

### 5.10 ML-Enhanced Processing

**Core principle: ML enhances signal extraction, never fabricates signal.**

Every ML model in the pipeline operates on data that was captured by the sensor. ML can suppress noise, separate overlapping structures (stars vs. nebula), model and remove gradients, and sharpen within the information content of the stack. ML must never hallucinate detail, invent structures, or blend in data from external images. The output must be a faithful representation of the photons the camera collected — processed more effectively than traditional algorithms, but never synthetically augmented.

This principle is enforced architecturally: ML models are post-processing steps in the pipeline chain (§5.9, PPR-*) operating on the stacked image. They replace or augment traditional algorithms at specific steps, not bypass the pipeline. The raw stack (pre-ML) is always preserved, and before/after comparison (PPR-31) lets the operator verify that ML is enhancing, not inventing.

**ML processing tools:**

These replace or offer alternatives to the traditional algorithms in the corresponding PPR requirements. The operator chooses between traditional and ML-based implementations per step.

| ID | Requirement | Priority |
|----|------------|----------|
| MLR-01 | ML-based noise reduction: deep learning denoiser (trained on astro data) as an alternative to wavelet/non-local means (PPR-14, PPR-15); adjustable strength with detail preservation slider | Should |
| MLR-02 | ML-based star-starless separation: neural network model (e.g., StarNet++ architecture) as an alternative to morphological methods (PPR-20); produces a clean star mask and starless image | Should |
| MLR-03 | ML-based background extraction: learned sky model that handles complex gradients (IFN, light pollution domes, moon glow) better than polynomial/RBF fitting (PPR-12) | Could |
| MLR-04 | ML-based sharpening / deconvolution: learned PSF deconvolution that adapts to spatially varying PSF across the field (coma, tilt) — alternative to Richardson-Lucy (PPR-19) | Could |
| MLR-05 | ML model management: download, update, and select models from a local model library on the stacking server; models are versioned and pinned per pipeline preset | Should |
| MLR-06 | All ML steps include a "traditional fallback" — if the ML model is unavailable or produces artifacts, the operator can switch to the equivalent traditional algorithm without re-stacking | Must |
| MLR-07 | ML processing is always optional and explicitly opt-in — no ML runs by default; the operator adds ML steps to the pipeline chain or enables them in a preset | Must |
| MLR-08 | ML step outputs are visually distinguishable in the before/after comparison UI — the operator can always see exactly what the ML changed | Should |

**Reference-guided parameter tuning:**

Instead of manually tweaking dozens of pipeline parameters, the operator can provide a reference image of the same (or similar) target — a well-processed example from a public gallery, a previous session's output, or a standard reference. The system analyzes the reference and suggests pipeline settings that would produce a tonally and chromatically similar result from the current stack.

This is *parameter suggestion*, not image blending. The reference image is never composited into the output. The system extracts tonal curves, color balance targets, saturation profiles, and stretch characteristics from the reference, then maps them to pipeline parameters. The operator reviews the suggestions and accepts, modifies, or rejects them.

| ID | Requirement | Priority |
|----|------------|----------|
| MLR-09 | Reference image input: operator provides a JPEG/TIFF/FITS reference image for the current target or a similar object type | Should |
| MLR-10 | Reference analysis: extract tonal profile (histogram shape, stretch curve), color balance (RGB channel ratios, white point), saturation distribution, and dynamic range characteristics from the reference image | Should |
| MLR-11 | Parameter suggestion: map the extracted reference profile to concrete pipeline settings — stretch function + parameters, curves control points, white balance, saturation, background target level — presented as a proposed pipeline configuration | Should |
| MLR-12 | Suggestion is non-destructive: presented as a "suggested preset" that the operator can preview, accept, modify, or reject; never auto-applied | Must |
| MLR-13 | Reference image library: store reference images per target (M42, M31, etc.) on the stacking server for reuse across sessions; optionally fetch from a curated public catalog | Could |
| MLR-14 | Style transfer mode: given a reference, match the overall aesthetic (color palette, contrast, saturation style) while preserving the spatial content of the operator's stack — implemented as a learned tone-mapping curve, not pixel-level style transfer | Could |
| MLR-15 | Provenance tracking: every pipeline output records whether ML models or reference-guided suggestions were used, which models/references, and what parameters were applied — full reproducibility | Must |

### 5.11 LLM Control Layer

AstroCtl exposes a natural language control interface powered by an LLM (Claude or GPT) with full agentic tool-use access to the system's REST APIs. The operator can control the rig, adjust settings, query status, and get diagnostic advice via text or voice — from the PWA on their tablet or phone.

**Architecture:**

```
Operator                    Field Node                    Stacking Server
  │                              │                              │
  │  "Switch to M31,            │                              │
  │   180s at ISO 3200"         │                              │
  │ ────────────────────►       │                              │
  │                      ┌──────┴──────┐                       │
  │                      │ LLM Agent   │                       │
  │                      │             │                       │
  │                      │ Understands:│                       │
  │                      │ • System state (mount pos,         │
  │                      │   camera settings, sequence        │
  │                      │   progress, guide RMS,             │
  │                      │   stacking stats)                  │
  │                      │ • Equipment capabilities           │
  │                      │ • Astronomy domain knowledge       │
  │                      │                                    │
  │                      │ Has tools for:                     │
  │                      │ • Mount API (goto, track, park)    │
  │                      │ • Camera API (settings, capture)   │
  │                      │ • Session API (start, stop, queue) │
  │                      │ • Solver API (solve, sync)         │
  │                      │ • Stacking API ─────────────────►  │
  │                      │ • Pipeline API ─────────────────►  │
  │                      │ • Calibration API ───────────────► │
  │                      └──────┬──────┘                       │
  │                             │                              │
  │  "Slewing to M31.          │                              │
  │   Changed to 180s          │                              │
  │   ISO 3200. Shall I        │                              │
  │   start the sequence?"     │                              │
  │ ◄────────────────────      │                              │
```

The LLM agent runs on the field node (or optionally on the stacking server if the field node is resource-constrained). It receives the operator's natural language input, reads system state via the same APIs the UI uses, formulates a plan, executes API calls, and reports the result back in natural language.

**Safety model:**

Not all commands carry equal risk. The LLM control layer uses a tiered execution model:

| Tier | Risk | Behavior | Examples |
|------|------|----------|---------|
| **Read** | None | Execute immediately, no confirmation | "What's the current RA/DEC?", "How many frames so far?", "What's the guide RMS?" |
| **Low** | Reversible | Execute immediately, report result | "Set ISO to 3200", "Change stretch to asinh", "Add SCNR step to pipeline" |
| **Medium** | Significant | Explain plan, wait for operator confirmation | "Switch target to M31" (stops current sequence, slews), "Start a 50-frame sequence", "Reprocess with different rejection" |
| **High** | Irreversible / safety | Always require explicit confirmation, show warning | "Park the mount" (ends session), "Delete calibration profile" |
| **Blocked** | Safety-critical | LLM cannot execute — hardware button only | Emergency stop is always available as a physical UI button; the LLM can suggest it but never auto-execute |

| ID | Requirement | Priority |
|----|------------|----------|
| **Core** | | |
| LLM-01 | LLM agent with tool-use access to all AstroCtl REST API endpoints on both field node and stacking server | Must |
| LLM-02 | Natural language input via text chat in the PWA UI | Must |
| LLM-03 | LLM receives full system state as context before each interaction: mount position, tracking mode, camera settings, sequence progress, guide status, stacking stats, recent frame quality, calibration profile, active alerts/warnings | Must |
| LLM-04 | LLM tools defined as structured function schemas matching the REST API — the LLM reasons about which API calls to make and in what order | Must |
| LLM-05 | Tiered execution model: read (immediate), low (immediate), medium (confirm), high (confirm + warning), blocked (hardware only) — tier assigned per API endpoint and enforced server-side in the API layer (SEC-03), so confirmation cannot be bypassed by the agent | Must |
| LLM-06 | LLM explains what it's about to do before executing medium/high tier actions, and reports results after execution | Must |
| **Domain intelligence** | | |
| LLM-07 | LLM has astronomy domain knowledge: can answer questions about targets (what is M42, when does it transit, is it visible tonight), suggest optimal imaging parameters for a target, explain why guide RMS is high, interpret FWHM trends | Should |
| LLM-08 | LLM can interpret frame quality data and provide diagnostics: "your last 5 frames have rising FWHM — likely dew or defocus", "star eccentricity is high — check polar alignment or wind", "background is elevated — moon is 23° away" | Should |
| LLM-09 | LLM can suggest pipeline parameter changes based on the current stack and session context: "you have 30 frames now, switching from mean to sigma-clip would improve satellite rejection", "FWHM is 3.2 arcsec — I'd suggest enabling deconvolution" | Should |
| **Multi-step workflows** | | |
| LLM-10 | LLM can execute multi-step workflows as a single natural language command: "image M42 for 2 hours at 120s ISO 1600 with dithering" → set camera, create sequence, goto with solve-and-center, start sequence | Should |
| LLM-11 | LLM can plan an entire evening session from a natural language brief: "tonight I want to image M42 and M31, prioritize whichever is higher first, 90 minutes each, standard settings" → build target queue with timing, ordered by altitude | Could |
| LLM-12 | LLM can perform comparative analysis: "compare the current stack with the version from last week's session" → load both, display side by side, summarize differences in integration time, SNR, FWHM | Could |
| **Voice & accessibility** | | |
| LLM-13 | Voice input: browser speech-to-text where available, with a local STT fallback on the field node (e.g., whisper.cpp) — Chrome's Web Speech API is cloud-backed and requires operator-device internet, which conflicts with the VPN-only model (LLM-17); iOS support is partial. The local fallback is invoked as a per-utterance subprocess so no model stays resident in the backbone (PRF-05); model choice is configurable and the feature is off by default on memory-constrained field nodes | Should |
| LLM-14 | Voice output (text-to-speech) for LLM responses — operator can hear status updates and confirmations without looking at the screen | Could |
| LLM-15 | Voice commands work with gloves / in the dark — the interaction model doesn't require precise tapping or typing | Should |
| **Provider & deployment** | | |
| LLM-16 | LLM provider is configurable: Anthropic Claude API, OpenAI API, or a local model (e.g., ollama) — the tool-use schema is provider-agnostic | Should |
| LLM-17 | LLM API calls go through the field node's network (VPN) — the operator's device doesn't need direct internet access, only VPN to the field node | Should |
| LLM-18 | Conversation history maintained per session — the LLM remembers earlier commands and context within the current imaging session | Should |
| LLM-19 | LLM interactions are logged alongside session metadata — every command, tool call, and result is recorded for reproducibility and debugging | Must |
| LLM-20 | Graceful degradation: if the LLM API is unavailable (no internet, API down), all manual controls remain fully functional — the LLM layer is an enhancement, not a dependency | Must |

### 5.12 GPU-Accelerated Compute

The stacking server (Ryzen 9, 128 GB RAM, RTX 4090 24 GB VRAM) is the sole compute node for all processing workloads. The RTX 4090 with 24 GB VRAM enables GPU-accelerated processing for the heaviest operations in the pipeline. The system uses CUDA (via CuPy, PyTorch, or custom kernels) with CPU fallback for all operations:

- **Frame registration**: star matching and affine/projective warp on GPU — 10-50x faster than CPU
- **Accumulation with rejection**: sigma-clip and kappa-sigma per-pixel operations parallelized across the full frame
- **ML inference**: noise reduction, star separation, deconvolution models run natively on the 4090
- **Debayering**: GPU-accelerated VNG/AHD debayer for full-resolution frames
- **Post-processing chain**: stretch, curves, color operations, gradient modeling — all GPU-acceleratable

With 128 GB RAM, the system can hold an entire session's frames in memory (300+ frames at 24 MP × 3 channels × 32-bit float = ~288 MB per frame → ~84 GB for 300 frames), enabling true median and sigma-clip without streaming or approximation.

| ID | Requirement | Priority |
|----|------------|----------|
| CMP-01 | GPU-accelerated frame registration (star detection, matching, warp) via CUDA on the stacking server | Should |
| CMP-02 | GPU-accelerated pixel rejection (sigma-clip, kappa-sigma) and accumulation | Should |
| CMP-03 | GPU-accelerated debayering (VNG, AHD) | Should |
| CMP-04 | GPU-accelerated post-processing operations (stretch, curves, color, gradient modeling) | Should |
| CMP-05 | GPU-accelerated ML inference using the local RTX 4090 (PyTorch CUDA or ONNX Runtime CUDA) | Should |
| CMP-06 | CPU fallback for all GPU-accelerated operations — system runs correctly (but slower) if no GPU is available or CUDA is not installed | Must |
| CMP-07 | GPU memory management: operations work within 24 GB VRAM; large frames processed in tiles if necessary; VRAM usage monitored and reported | Must |

## 6. Non-Functional Requirements

Non-functional requirements are **Must** unless marked otherwise inline.

### 6.1 Architecture

| ID | Requirement |
|----|------------|
| ARC-01 | Rust backbone on both nodes (tokio async runtime, axum for REST + WebSocket): drivers, orchestration, safety, pipeline control flow, APIs. Python is confined to supervised worker processes where its ecosystem is necessary — GPU array compute and ML inference on the stacking server. The field node contains no Python runtime |
| ARC-02 | React frontend built and bundled with the field node backend — the compiled bundle is served as static files, so deployment is a single service (a build step exists at development time, not at deploy time) |
| ARC-03 | All hardware communication is async (tokio) to avoid blocking the runtime; blocking C-library calls (libgphoto2, libraw) are confined to dedicated tasks or a bounded thread pool |
| ARC-04 | Mount and camera drivers are independent modules with no cross-dependencies |
| ARC-05 | Configuration via a single YAML file per node (field node config, stacking server config) |
| ARC-06 | Field node is fully self-contained and runs offline — mount control, camera control, capture, plate solving all work without the stacking server |
| ARC-07 | WebSocket for real-time status broadcasting (mount position, guide corrections, capture progress, stacking preview) |
| ARC-08 | Two-node distributed architecture: field node (hardware control + capture) and stacking server (calibration, registration, accumulation, export) communicate over VPN (e.g., NetBird, Tailscale) — nodes may be on different physical networks |
| ARC-09 | Stacking server is optional — the field node captures and saves frames regardless; stacking can be performed later or skipped entirely |
| ARC-10 | Both nodes expose REST APIs; the field node UI aggregates both into a single operator interface |
| ARC-11 | Frame transfer from field node to stacking server is resilient: local queue with retry, frames never lost if stacking server is temporarily unreachable or VPN tunnel drops |
| ARC-12 | Calibration library lives on the stacking server and is managed independently of capture sessions |
| ARC-13 | The operator's device (tablet, phone, or laptop) connects to the field node's web UI over VPN — the operator does not need to be physically co-located with the field rig |
| ARC-14 | Web UI is delivered as a Progressive Web App (PWA) — installable to home screen on iOS and Android, launches full-screen without browser chrome, caches UI shell for offline availability. Any device with a modern browser and VPN access can operate the full system |
| ARC-15 | All inter-node communication (field node ↔ stacking server, browser ↔ field node, browser ↔ stacking server) assumes routed IP connectivity via VPN, not local network discovery |
| ARC-16 | Three independent image processing pipelines (§5.9): control and live view run on the field node, processed pipeline runs on the stacking server — pipelines are decoupled and never block each other |
| ARC-17 | Raw frames are the immutable source of truth; all processing pipelines read from stored frames, never modify them — reprocessing is a first-class operation, not an afterthought |
| ARC-18 | ML models are pipeline steps, not a separate system — they plug into the same post-processing chain (PPR-*) as traditional algorithms, controlled by the same presets, and subject to the same before/after comparison |
| ARC-19 | ML models run on the stacking server (GPU-accelerated where available); the field node never requires ML inference capability |
| ARC-20 | LLM agent runs as a service on the field node (or stacking server), consuming the same REST APIs as the web UI — no privileged access, no backdoor commands |
| ARC-21 | LLM is an enhancement layer, not a dependency — the system is fully functional without internet or LLM API access (LLM-20); all controls accessible via manual UI |
| ARC-22 | Stacking server leverages GPU (NVIDIA CUDA) for compute-intensive operations *(Should — mirrors CMP-01–05)*; all GPU code paths have CPU fallbacks (CMP-06, Must) |
| ARC-23 | Two-tier compute architecture: field node (latency-critical, lightweight) and stacking server (primary compute, GPU-accelerated) — the stacking server is self-sufficient for all processing workloads |

### 6.2 Performance

| ID | Requirement |
|----|------------|
| PRF-01 | Mount position update latency ≤ 200ms from position-counter read to UI display (on LAN/fast VPN); graceful degradation to 1s on high-latency links |
| PRF-02 | Live view frame rate ≥ 5 fps on LAN; adaptive quality/frame rate reduction over VPN to maintain responsiveness |
| PRF-03 | Guide correction loop latency ≤ 500ms (exposure + detection + correction command) — runs entirely on the field node, unaffected by VPN latency |
| PRF-04 | Image download must not block mount tracking or UI responsiveness |
| PRF-05 | Field node steady-state memory usage ≤ 512 MB (Rust backbone, no Python runtime, no stacking in memory on the field node); transient RAW-decode buffers during CR3 preview generation (~150–300 MB per decode) are excluded from the steady-state budget. The local speech-to-text model (LLM-13, Phase 3) is likewise excluded: it runs as a subprocess spawned per utterance, not as a resident model in the backbone, and a deployment that enables it must budget separately for the chosen model size (`base.en` ≈ 150 MB, larger models more) |
| PRF-06 | Plate solve completion ≤ 5s with ASTAP (with position hint), ≤ 30s with astrometry.net blind solve |
| PRF-07 | Frame transfer from field node to stacking server: ≤ 5s per frame over gigabit LAN, tolerant of higher latency and lower bandwidth over VPN (queued, non-blocking) |
| PRF-08 | Live stacking compute step on the stacking server: new frame calibrated, registered, and accumulated within 3s of **receipt** (frame durable on the stack node's local disk → accumulator updated; GPU-accelerated registration and accumulation). This is the innermost of the three nested budgets — PRF-08 ⊂ PRF-09 ⊂ IPP-06 |
| PRF-09 | Live stacking preview update pushed to UI within 1s of accumulation completing |
| PRF-10 | Stacking server memory: with 128 GB RAM, hold up to 300+ full-resolution 24 MP frames in memory for true median and sigma-clip stacking (no approximation needed for typical sessions); running accumulator + statistics buffers ≤ 8 GB |
| PRF-11 | Calibration profile lookup (CAL-05) ≤ 100ms per frame |
| PRF-12 | Emergency stop command delivery ≤ 500ms end-to-end from UI tap to mount halt on links with RTT ≤ 150ms; on worse links the stop command takes absolute priority over all queued traffic. Independently of the remote path, the field node's own watchdogs (REL-02, REL-03) can halt the mount locally without an operator round-trip |
| PRF-13 | Mid-session re-stack (full rebuild from stored frames): ≤ 3s per frame on the stacking server (GPU-accelerated); a 100-frame session rebuilds in ≤ 5 minutes; post-processing-only changes (PPR-*) apply in ≤ 1s for the full chain; individual step parameter tweaks apply in ≤ 200ms when intermediate results are cached |
| PRF-14 | GPU utilization: frame registration, accumulation with rejection, ML inference (noise reduction, star separation), and deconvolution leverage the RTX 4090 via CUDA; CPU fallback for all GPU-accelerated operations *(Should)* |
| PRF-15 | ML inference on stacking server: noise reduction model ≤ 2s per full-resolution frame on RTX 4090; star separation ≤ 3s; batch inference for reprocessing at near-100% GPU utilization *(Should — Phase 4)* |

### 6.3 Reliability

| ID | Requirement |
|----|------------|
| REL-01 | Emergency stop must work regardless of application state (dedicated endpoint, no queuing) |
| REL-02 | Serial communication timeout and retry with error reporting (no silent hangs) |
| REL-03 | USB disconnect detection and graceful recovery for both mount and camera |
| REL-04 | Sequence state persisted so a crash during capture doesn't lose progress metadata |
| REL-05 | All captured images saved to disk on the field node before any transfer or processing — never lose a frame |
| REL-06 | Frame transfer queue persists across field node restarts — unsent frames are retransmitted on reconnection |
| REL-07 | Stacking server crash or VPN disconnection does not affect field node capture — the session continues, frames queue locally, stacking resumes when connectivity recovers |
| REL-08 | Calibration library metadata backed by a durable index (JSON or SQLite) that survives stacking server restarts |
| REL-09 | VPN tunnel drop between operator device and field node does not abort a running sequence — the sequence continues autonomously, operator reconnects to current state |
| REL-10 | WebSocket connections auto-reconnect on VPN tunnel recovery without requiring a page reload |
| REL-11 | Raw frames are immutable — no processing pipeline or reprocessing run ever modifies captured raw frames; removal only via explicit operator action or the verified-transfer retention policy (REL-13) |
| REL-12 | Disk-space monitoring on both nodes with warning and critical thresholds; capture pauses gracefully (after the in-flight frame completes) before disk exhaustion instead of failing mid-write |
| REL-13 | Field-node retention policy: raw frames may be reclaimed from the field node only after checksum-verified transfer to the stacking server, and only under an explicit operator-configured policy — the stacking server archive is authoritative |
| REL-14 | Field node system clock disciplined via NTP (or a GPS time source when offline); the UI warns when the clock is unsynchronized — LST, goto, and visibility calculations depend on accurate time, and a Pi has no battery-backed RTC |

### 6.4 Usability

| ID | Requirement |
|----|------------|
| USB-01 | UI must be usable on a 10" tablet and phone screens in outdoor/dark conditions (dark theme, large touch targets, responsive layout) |
| USB-02 | Red/dim UI mode option to preserve dark adaptation |
| USB-03 | All critical actions (stop, park, abort) accessible with one tap from any screen, including on phone-sized viewports |
| USB-04 | Connection status for mount, camera, and stacking server always visible in the header |
| USB-05 | Coordinates displayed in standard astronomical notation (HH:MM:SS / ±DD°MM'SS") |
| USB-06 | Stacking server status shows: connected/disconnected, queue depth, current stack frame count, last preview timestamp |
| USB-07 | UI is fully functional over VPN from a device not co-located with the rig — all controls, previews, and status work remotely with no degradation beyond network latency |
| USB-08 | Responsive layout: single-column on phone, multi-panel on tablet/desktop — all functionality accessible on both form factors |
| USB-09 | PWA manifest with app name, icon set, theme color, and display: standalone — "Add to Home Screen" installs a proper app icon on iOS and Android |
| USB-10 | Service worker caches the UI shell (HTML, JS, CSS, fonts, icons) so the app opens instantly even if the VPN tunnel is still connecting — data loads when connectivity is established |
| USB-11 | Live view and stacking preview degrade gracefully over lower-bandwidth VPN links (adaptive JPEG quality, reduced frame rate) |
| USB-12 | Touch-optimized controls: D-pad buttons, sliders, and capture button sized for finger interaction (minimum 44×44px tap targets) |

### 6.5 Extensibility

| ID | Requirement |
|----|------------|
| EXT-01 | Mount drivers implement the MountDevice abstract interface (§4.1) — adding support for a new mount means writing a single driver class, no changes to orchestrator, API, UI, or LLM tools |
| EXT-02 | Camera drivers implement the Camera abstract interface (§4.1) — same pattern; guide camera and future devices (filter wheel, focuser) follow the same abstraction |
| EXT-03 | REST API is the primary integration surface; any external tool can drive the system via HTTP |
| EXT-04 | Session sequences are defined as data (YAML/JSON), not code — editable and shareable |
| EXT-05 | All events emitted on WebSocket for external monitoring/automation |
| EXT-06 | PWA UI is structured for future Capacitor/Ionic wrapping — no browser-only APIs that would prevent packaging as a native app |

### 6.6 Security

The VPN is the trust boundary: AstroCtl services are never exposed to the public internet, and VPN provisioning is out of scope (§11). The authentication requirements below are defense in depth for the case where a VPN ACL is broader than intended or a port is accidentally exposed.

| ID | Requirement | Priority |
|----|------------|----------|
| SEC-01 | VPN as explicit trust boundary: services bind to VPN/LAN interfaces only, no public exposure or port forwarding; this assumption is stated in the setup documentation | Must |
| SEC-02 | Token-based authentication on all REST and WebSocket endpoints on both nodes (shared token per node, supplied via environment variable) | Must |
| SEC-03 | LLM tier enforcement is server-side: medium/high-tier endpoints require a confirmation token issued on explicit operator approval; the API rejects unconfirmed calls regardless of caller — tiers cannot be bypassed by the agent, a prompt injection, or a buggy client | Must |
| SEC-04 | Secrets (LLM API keys, auth tokens) are supplied via environment variables, never stored in the main config file or written to logs | Must |
| SEC-05 | Optional TLS on browser and inter-node connections for transports the operator doesn't fully trust | Could |

## 7. Technical Dependencies

Language split per ARC-01: Rust backbone on both nodes; Python confined to supervised workers on the stacking server.

**Rust backbone (field node and stacking server):**

| Dependency | Purpose | Notes |
|-----------|---------|-------|
| `tokio` **1.53**, `axum` **0.8** | Async runtime, REST + WebSocket backend | |
| `serialport` **4.9** | Mount serial communication | Pulls `libudev-sys`, so it needs **`libudev-dev`** — measured, and previously undocumented. Building with `default-features = false` drops udev but also drops enumeration by USB VID/PID, which MNT-01 auto-detection depends on |
| `gphoto2` **3.4.1** | Camera control via libgphoto2 bindings | Requires `libgphoto2` system library. **Coverage verified against a real R10** (`spikes/gphoto2-r10/FINDINGS.md`, 2026-07-29): bulb, timed capture, CR3 download, settings read/write, live view at 58.5 fps, battery/storage — all work through the bindings. **No CLI fallback is needed; `camera.ops_via_cli` ships as `[]`.** The fallback mechanism stays designed (SDD §5.3.3) for future bodies |
| `rawler` **0.7.2** | CR3 decoding for preview generation | **Selected on build evidence** (`docs/evidence/dependency-survey-2026-07-29.md`): pure Rust, no system library, dedicated CR3/CRX decoder, an R10 camera profile, and R10 regression fixtures across RAW/CRAW/BURST. `rawloader` has no CR3 support at all; `libraw`/`libraw-sys` are at 0.1.1 and would add a `libraw_r` system dependency for nothing. M2-T01 still confirms decode timing and peak RSS on a real file |
| **First-party FFI over libsep** (no crate exists) | Star detection for guiding, registration, and frame quality assessment | The C Source Extractor library the Python `sep` package wraps. **There is no `sep` crate** — the name on crates.io belongs to unrelated projects and `sxr` is an empty placeholder. A thin in-tree `sep-sys` binding must be written; libsep's API surface is small (background estimation, extraction, aperture photometry). This lands in Phase 2a with the control pipeline and needs its own spike |
| `erfars` **0.2.0** | Coordinate transforms, sidereal time, precession/nutation | **Selected on build evidence:** it vendors the ERFA C source (251 `.c` files, built with `cc`) rather than linking a system copy — so it is genuinely the library astropy wraps *and* needs no `liberfa-dev`. CI parity tests against astropy still required. **Do not select the crate named `erfa` (0.2.1)** — despite the name it is a pure-Rust *reimplementation* (0 C files), which would turn the parity suite into a test of someone else's port rather than of our usage. `erfa-sys` is unnecessary now that `erfars` vendors |
| `fitsio` **0.21.10** | FITS read/write for plate solving I/O, simulator frames, and metadata | Wraps cfitsio; needed from M1 because the simulator camera writes 16-bit FITS |
| image / ndarray / rayon | Image ops, array math, CPU parallelism (debayer, stretch, PI controller) | |
| `rusqlite` **0.40** | Durable indexes: transfer journal, calibration library, session index | Use the `bundled` feature — SQLite compiles in, so neither node needs a system SQLite |
| reqwest | LLM provider HTTP client (Anthropic/OpenAI/ollama, LLM-16), inter-node HTTP | No official Rust LLM SDKs assumed; providers spoken via their HTTP APIs |
| serde / serde_json | Config, metadata, event schema, worker IPC protocol | |
| whisper.cpp | Local speech-to-text for voice input (LLM-13) | C++ library, called from Rust or as subprocess; avoids Web Speech API's cloud dependency |

**Python worker environment (stacking server only, supervised by the Rust backbone):**

| Dependency | Purpose | Notes |
|-----------|---------|-------|
| numpy / CuPy | GPU-accelerated array compute: registration, accumulation, rejection, debayer | CUDA backend; numpy-compatible API; numpy fallback on CPU (CMP-06) |
| PyTorch / ONNX Runtime | ML model inference for noise reduction, star separation, background extraction | GPU-accelerated; ONNX for portability |
| scikit-image / OpenCV | Affine/projective transforms, background modeling, deconvolution, morphology | Worker-side only |
| PyWavelets | Wavelet-based noise reduction and multiscale sharpening | |
| astropy / photutils | WCS handling in the post-chain, photometric color calibration (PPR-06, Phase 4) | Worker-side only — never on the field node |

Plate solving backends (at least one required):
- **astrometry.net** (`solve-field` CLI): `apt install astrometry.net` + index files from `astrometry.net/doc/readme.html`
- **ASTAP** (`astap` CLI): download from `astap-program.org` + star database (G17 or H17)

System-level, listed by the milestone that first needs it — nothing below is required to start M0:

Every entry below was confirmed by isolated build (`docs/evidence/dependency-survey-2026-07-29.md`),
not inferred from documentation:

| Needed from | Package | For |
|-------------|---------|-----|
| M0 | rustup toolchain **1.97.1**, Node ≥ 20, npm, a C compiler | backbone + PWA build; no astronomy C libraries at all. `cc` is needed because `rusqlite` compiles bundled SQLite |
| M1 | `libcfitsio-dev` | `fitsio` — the simulator camera writes 16-bit FITS and the preview decoder reads it |
| M2 | `libgphoto2-dev` | camera USB access — M2's **only** system dependency; `rawler` needs none |
| M3 | `libudev-dev` | `serialport` port enumeration by USB VID/PID (MNT-01 auto-detect) |
| M3 | USB serial driver | usually built into the kernel for PL2303/FTDI/CH340 |
| Phase 2a | `libsep` + headers | the first-party FFI binding for star detection |
| Phase 2a | `astrometry.net` and/or `astap` | plate solving (see above) |
| Phase 2b | CUDA toolkit + Python worker venv | stacking server only |

No `liberfa-dev` (erfars vendors ERFA) and no `libraw-dev` (rawler is pure Rust).

Python on the stacking server: the M1 stub worker needs only numpy and a FITS reader. Pin the
worker venv's interpreter deliberately (config `workers.python_interpreter`) — CuPy and PyTorch
wheel availability lags new CPython releases, and that bites at Phase 2b, not now.

## 8. Configuration

### 8.1 Field Node Configuration

```yaml
# field-node.yaml
site:
  latitude: 59.9139      # Oslo, Norway
  longitude: 10.7522
  elevation: 25
  timezone: Europe/Oslo

mount:
  driver: skywatcher        # "skywatcher", "indi", "ascom_alpaca", "simulator"
  port: auto                # "auto" for detection, or "/dev/ttyUSB0"
  baud: 9600
  park_position:
    ra_hours: 0.0
    dec_degrees: 90.0
  settle_time_seconds: 3    # pause after slew before capture
  serial:
    request_timeout_ms: 500   # per request/response exchange
    request_retries: 1        # retries before DeviceError::Timeout
    heartbeat_misses: 3       # consecutive poll failures before watchdog fires (REL-02)
    poll_hz: 1                # position poll rate, minimum 1 (MNT-02)
  limits:
    min_altitude_degrees: 15    # reject goto/slew targets below this altitude (MNT-15)
    meridian_limit_minutes: 15  # stop tracking this long past the meridian (MNT-16)
    slew_ttl_default_ms: 500    # manual-slew dead-man's switch: default authorization window
    slew_ttl_max_ms: 2000       # server-side clamp on a client-requested TTL
  # indi_device: "EQMod Mount"    # if driver=indi
  # ascom_host: "http://..."      # if driver=ascom_alpaca

camera:
  driver: gphoto2            # "gphoto2", "indi", "ascom_alpaca", "simulator"
  default_iso: "1600"
  default_shutter: "30"
  default_format: "RAW+JPEG"
  ops_via_cli: []            # operations routed through the `gphoto2` binary instead of the
                             #   crate bindings, e.g. ["bulb"] — populated from the M2 spike
  timeouts:                  # operation-class timeouts; a breach declares the thread wedged (REL-03)
    config_seconds: 5        # get/set a setting
    capture_extra_seconds: 30  # added to the exposure duration
    download_seconds: 120
  # indi_device: "Canon DSLR"     # if driver=indi

# Where sessions, frames and logs live on the field node (PRD §5.9 layout)
storage:
  sessions_dir: /data/astro/sessions
  disk_warn_free_gb: 20      # warning alert threshold (REL-12)
  disk_critical_free_gb: 5   # capture pauses after the in-flight frame (REL-12)

guide_camera:
  driver: null               # "asi", "qhy", "indi", "simulator", or null (disabled)
  # asi_index: 0             # if driver=asi
  # qhy_id: "QHY5III178M"   # if driver=qhy
  # indi_device: "ASI 120MM" # if driver=indi

guider:
  dither_pixels: 5        # dither offset in guide pixels
  dither_settle: 5        # seconds to settle after dither
  # Phase 3:
  # exposure: 2.0
  # aggressiveness_ra: 0.7
  # aggressiveness_dec: 0.5

solver:
  backend: astap           # "astap" or "astrometry"
  astap_path: /usr/local/bin/astap
  astap_database: /opt/astap/g17
  astrometry_config: /etc/astrometry.cfg
  search_radius: 15        # degrees from hint position
  downsample: 2            # reduce image resolution for faster solve
  timeout: 30              # seconds
  center_threshold: 60     # arcsec — re-slew if offset exceeds this
  center_max_iterations: 3

# Equipment profile for this session (used to tag frames for calibration matching)
equipment:
  telescope: "SW 200PDS f/5"
  camera: "Canon R10"
  filter: "none"

stacking_server:
  enabled: true
  host: 192.168.1.100      # stacking server IP on LAN
  port: 8471
  transfer_method: http     # "http" (upload endpoint) or "rsync"
  retry_interval: 10        # seconds between retries if server unreachable (backoff base)
  queue_dir: /data/astro/transfer_queue  # local spool for unsent frames
  pacing:                   # keep bulk uploads from queueing operator commands behind them
    bandwidth_cap_mbps: null   # null = uncapped; set on constrained links
    interactive_floor_pct: 20  # % of cap allowed while the operator is actively commanding
    interactive_window_seconds: 10  # motion command within this window triggers the floor

llm:
  enabled: true
  provider: anthropic       # "anthropic", "openai", or "ollama"
  model: claude-sonnet-5    # example — pin to a current model at deploy time
  api_key_env: ANTHROPIC_API_KEY  # read key from environment variable (SEC-04)
  # ollama_host: http://localhost:11434  # for local models
  confirmation_tiers:
    read: auto              # execute immediately
    low: auto               # execute immediately
    medium: confirm         # require operator confirmation
    high: confirm_warn      # confirm with warning
  voice_input: true         # enable Web Speech API voice commands
  voice_output: false       # text-to-speech for responses
  session_history: true     # maintain conversation context per session

server:
  host: 0.0.0.0             # bind to the VPN interface IP in production (SEC-01)
  port: 8470
  auth_token_env: ASTROCTL_TOKEN  # shared token for REST/WebSocket auth (SEC-02)
  max_command_age_ms: 2000  # motion-initiating commands older than this are rejected
                            #   COMMAND_STALE; stopping commands are never age-rejected
  runtime_worker_threads: null  # tokio async workers; null = min(2, cores-2), floor 1.
                            #   Deliberately NOT one-per-core: the camera OS thread and the
                            #   decode pool need cores reserved on a 4-core Pi (SDD §7)
  log_level: INFO
  log_dir: /data/astro/logs
```

### 8.2 Stacking Server Configuration

```yaml
# stacking-server.yaml
stacking:
  # --- Stacking method ---
  method: sigma_clip       # mean, weighted_mean, median, sigma_clip,
                           # kappa_sigma, winsorized_sigma_clip,
                           # min_max_clip, linear_fit
  sigma_low: 2.5           # sigma-clip / winsorized: lower rejection threshold
  sigma_high: 3.0          # sigma-clip / winsorized: upper rejection threshold
  kappa: 3.0               # kappa-sigma: rejection threshold (MAD-based)
  max_iterations: 5        # iterative rejection: max passes
  clip_low: 1              # min/max clip: discard N lowest per pixel
  clip_high: 1             # min/max clip: discard N highest per pixel
  live_approximation: true # use running approximation for live; full re-stack on demand

  # --- Frame weighting ---
  weight_mode: snr         # equal, snr, fwhm, background, custom
  # custom weights are set per-frame in the UI

  # --- Frame normalization ---
  normalization: multiplicative_mean  # none, additive_mean, multiplicative_mean,
                                      # median, background_region

  # --- Registration ---
  registration_method: affine  # affine, projective, translation_only
  min_star_count: 15           # minimum detected stars to attempt registration
  max_residual: 2.0            # pixels — reject registration if RMS residual exceeds this
  detection_threshold: 5.0     # sep detection threshold (sigma above background)

  # --- Reference frame ---
  reference_mode: auto         # auto (best quality), manual, first
  reference_frame: null        # frame number if reference_mode is manual

  # --- Debayer ---
  debayer_method: VNG          # bilinear, VNG, AHD, DCB

  # --- Frame rejection (whole-frame) ---
  reject_fwhm_max: 8.0        # arcsec — reject frames with FWHM above this
  reject_star_count_min: 10   # reject frames with fewer detected stars
  reject_eccentricity_max: 0.6 # reject frames with star eccentricity above this (trailing)
  reject_background_max: null  # ADU — reject frames with background above this (clouds)

  # --- Output ---
  export_dir: /data/astro/stacks

# Mirrored session archive — this is the authoritative copy (IPP-09, REL-13)
storage:
  sessions_dir: /data/astro/sessions
  disk_warn_free_gb: 100
  disk_critical_free_gb: 20   # ingest rejects new frames below this (REL-12)

# Supervised Python compute/ML workers (ADR-13)
workers:
  python_interpreter: /data/astro/venv/bin/python  # venv the workers run in
  compute_worker: workers/compute_worker.py
  ml_worker: workers/ml_worker.py
  health_ping_seconds: 5      # missed × 3 → kill and restart
  restart_backoff_seconds: 2  # capped exponential
  job_timeout_seconds: 300

calibration:
  library_dir: /data/astro/calibration
  index_file: library.json  # or library.sqlite
  dark_temp_tolerance: 2.0  # °C — match darks within this temperature range
  dark_max_age_days: 180    # flag masters older than this for re-acquisition
  default_master_method: sigma_clip  # method for generating master frames
  default_master_sub_count: 30       # recommended minimum sub-frames per master

ml:
  models_dir: /data/astro/ml_models
  device: auto              # "auto" (GPU if available), "cuda", "cpu"
  noise_reduction:
    model: astro_denoise_v1  # model name in models_dir
    enabled: false           # opt-in (MLR-07)
  star_separation:
    model: starnet_v2
    enabled: false
  background_extraction:
    model: astro_bgmodel_v1
    enabled: false
  reference_library_dir: /data/astro/references  # reference images per target

gpu:
  enabled: true
  device: auto              # "auto", "cuda:0", or "cpu" (fallback)
  vram_budget_gb: 20        # reserve ~4 GB for OS/display; use up to 20 GB for processing
  accelerate:
    registration: true      # GPU-accelerated star detection + warp
    accumulation: true      # GPU-accelerated sigma-clip / rejection
    debayer: true            # GPU-accelerated VNG/AHD
    post_processing: true   # GPU-accelerated stretch, curves, color ops
    ml_inference: true       # ML models on GPU (PyTorch CUDA / ONNX CUDA)

server:
  host: 0.0.0.0             # bind to the VPN interface IP in production (SEC-01)
  port: 8471
  auth_token_env: ASTROCTL_TOKEN  # shared token for REST/WebSocket auth (SEC-02)
  runtime_worker_threads: null  # null = one per core; heavy compute is in child processes,
                            #   so there is nothing to reserve against here (SDD §7)
  log_level: INFO
  log_dir: /data/astro/logs
```

### 8.3 Equipment Profiles (in calibration library)

```json
{
  "id": "sw200pds-r10-none",
  "telescope": "SW 200PDS f/5",
  "camera": "Canon R10",
  "filter": "none",
  "sensor_width_px": 6000,
  "sensor_height_px": 4000,
  "pixel_size_um": 3.72,
  "focal_length_mm": 1000,
  "arcsec_per_pixel": 0.77,
  "notes": "Primary deep-sky rig"
}
```

## 9. Development Phases

### Phase 1 — Foundation (Mount + Camera + Basic UI)

Core hardware control and a functional web interface on the field node.

Deliverables:
- **Hardware abstraction layer**: MountDevice, Camera, and GuideCamera abstract interfaces with capability inquiry (HAL-01 through HAL-06)
- **Driver registration**: config-driven driver selection, device auto-detection where supported (HAL-07, HAL-08)
- **Simulator drivers**: mount, camera, and guide camera simulators for development/testing without physical hardware (HAL-11)
- Skywatcher protocol driver implementing MountDevice (MNT-01 through MNT-08)
- Canon gPhoto2 driver implementing Camera (CAM-01 through CAM-04)
- Rust backend (axum) with REST endpoints and WebSocket status broadcast
- Web UI: mount control panel (coordinates, tracking, D-pad), camera panel (settings, capture), connection status — UI adapts to device capabilities reported by HAL
- **PWA packaging**: manifest, service worker, responsive layout — installable on tablet/phone home screen (USB-09, USB-10, USB-08, USB-12)
- **Live view pipeline** (basic): camera preview stream to browser, last-captured frame with quick stretch (CAM-05, CAM-06, IPP-04, IPP-14) — CAM-05/CAM-06 are delivered here, not in Phase 2a; Phase 2a only adds the annotation overlays (IPP-05)
- **Session raw frame storage**: all captured frames saved with session metadata structure (IPP-09, IPP-10)
- Configuration file loading (field node config with driver selection)
- Basic session logging

Exit criteria: can connect to mount and camera (real or simulated), slew to a target, set exposure, capture a frame, see it in live view, and download it — all from the PWA installed on a tablet over VPN. Simulator drivers allow full end-to-end testing without hardware. Raw frames stored in session directory structure.

### Phase 2a — Imaging Sessions + Plate Solving

Automated capture sequences with astrometric precision, entirely on the field node — no stacking server required.

Deliverables:
- Session orchestrator state machine (SES-01 through SES-06)
- Multi-target queue (SES-04)
- Dithering between frames via mount guide pulses (SES-05)
- Target catalog (Messier) and altitude plotting (PLN-03, PLN-04, PLN-05)
- Park and sync operations (MNT-09, MNT-10)
- Slew limits and meridian protection (MNT-15, MNT-16)
- Session log files (SES-07)
- **Control pipeline** on field node: plate solving, star detection, FWHM, quality scoring — latency-critical (IPP-02, IPP-03)
- **Live view pipeline** on field node: debayer, stretch, annotation overlays — near-real-time (IPP-04, IPP-05)
- **Plate solver interface** with astrometry.net and ASTAP backends, feeding the control pipeline (PLS-01, PLS-02, PLS-04)
- **Solve-and-center loop** for goto correction (PLS-03)
- **Automatic framing** via solve → offset → re-slew (PLS-06)
- **Continuous pointing verification** during sequences (PLS-05)
- Clock discipline and disk monitoring on the field node (REL-12, REL-14)

Exit criteria: can plan a multi-target session, execute it unattended with dithering and solve-and-center achieving < 1 arcmin pointing accuracy — entirely on the field node. Live view shows the current frame on the operator's tablet. Raw frames and session metadata stored per §5.9. Zero lost frames.

### Phase 2b — Distributed Stacking + Calibration Library

Network-offloaded stacking and reusable calibration frames.

Deliverables:
- **Stacking server** as a separate process/host: REST API, WebSocket preview, frame ingestion endpoint (STK-17, STK-18, STK-20)
- **Frame transfer agent** on field node: queue, retry, checksum-verified resilient delivery (STK-17, ARC-11, REL-13)
- **Three-pipeline architecture completed** with the **processed pipeline** on the stacking server: full-resolution calibration, registration, accumulation, configurable stretch (IPP-01, IPP-06, IPP-07)
- **Stacking pipeline**: star detection, registration, accumulation, auto-stretch, WebSocket preview (STK-01 through STK-05, STK-08, STK-09, STK-10, STK-16, STK-21)
- **Stack export** as 16-bit FITS/TIFF and stretched JPEG (STK-06, STK-07)
- **Calibration library** on stacking server: equipment profiles, master dark/flat/bias storage, automatic profile matching (CAL-01 through CAL-07, CAL-10)
- **Master frame import** (CAL-11) — the Phase 2 pathway for populating the library, until in-app calibration capture arrives in Phase 3 (CAL-09)
- **Calibration-aware stacking**: dark subtraction, flat correction, bias subtraction from library (STK-12, STK-13, STK-14)
- **Field node UI proxying** stacking server status and preview (STK-19)
- **API authentication and trust boundary** on both nodes (SEC-01, SEC-02, SEC-04)
- **GPU-accelerated stacking**: registration, accumulation with rejection, debayer with CPU fallback (CMP-01, CMP-02, CMP-03, CMP-06, CMP-07)

Exit criteria: frames stream to the stacking server over VPN, are calibrated against imported masters and stacked in real time; live stack preview visible in the browser; exportable as 16-bit FITS/TIFF and 8-bit JPEG. Calibration library contains at least one profile with imported master darks and flats automatically applied. Zero lost frames across a simulated VPN outage.

### Phase 2c — Post-Processing + LLM Control

Configurable, re-runnable processing and the natural-language control layer.

Deliverables:
- **Processed pipeline is re-runnable**: reprocess mid-session (rebuild accumulator from existing frames with new settings while capture continues) or post-session from stored raw frames (IPP-08, IPP-11, IPP-16, IPP-17)
- **Reprocessing UI**: adjust pipeline settings mid-session or post-session, trigger re-stack, compare outputs (IPP-13)
- **Pipeline presets**: save and apply named processing configurations across sessions (IPP-12)
- **Post-processing chain** with ordered, non-destructive steps on the stacked image (PPR-29, PPR-30):
  - Stretch functions: asinh, STF, histogram, GHS, gamma (PPR-01)
  - Curves and levels: master + per-channel R/G/B with arbitrary control points (PPR-02, PPR-03)
  - Histogram display with per-channel overlays, live-updating (PPR-04)
  - RGB channel separation: view and adjust independently (PPR-05)
  - White balance: manual sliders, eyedropper, auto-neutral (PPR-07)
  - Saturation and vibrance (PPR-09)
  - SCNR green/magenta cast removal (PPR-10)
  - Background extraction / gradient removal (PPR-12, PPR-13)
  - Crop and rotation (PPR-24, PPR-25)
  - Multi-format export: FITS, TIFF, JPEG/PNG (PPR-28)
  - Before/after comparison and undo/redo (PPR-31, PPR-32)
  - Pipeline presets include full post-processing chain (PPR-33)
- **GPU-accelerated post-processing** operations (CMP-04)
- **LLM control layer (basic)**: text chat in PWA, tool-use access to mount/camera/session/solver/stacking/pipeline APIs, tiered execution model with server-side confirmation enforcement (LLM-01 through LLM-06, LLM-19, LLM-20, SEC-03)

Exit criteria: can fiddle with curves, stretch, color balance, gradient removal mid-session and see the result within seconds on the stacked image. Can reprocess a completed session with different parameters and produce a new output alongside the original. Can type "switch to M31 at 180s ISO 3200" and have the LLM execute it with confirmation enforced by the API layer.

### Phase 3 — Guiding + Polar Alignment

Closed-loop autoguiding with a dedicated guide camera, plus astrometric polar alignment.

Deliverables:
- **ASI and QHY guide camera drivers** implementing GuideCamera interface (HAL-04)
- Star detection and centroid tracking (GDE-01, GDE-02)
- PI correction loop (GDE-03)
- Guide performance graphs in UI (GDE-04)
- Meridian flip handling (MNT-11, SES-08)
- Focus assistant (CAM-10)
- **Polar alignment assistant** via plate solving at multiple positions (PLS-07)
- **Calibration capture workflow**: shoot darks/flats from the camera panel, auto-populate metadata, transfer to stacking server library (CAL-09)
- **LLM diagnostics and intelligence**: frame quality interpretation, FWHM trend analysis, pipeline parameter suggestions based on session context (LLM-07, LLM-08, LLM-09)
- **LLM multi-step workflows**: natural language session planning and execution — "image M42 for 2 hours at 120s with dithering" (LLM-10, LLM-11)
- **Voice control**: speech-to-text input for hands-free operation in the field (LLM-13, LLM-15)

Exit criteria: can autoguide with sub-arcsecond RMS, automatically flip at the meridian, correct polar alignment to < 2 arcmin error, and run unattended multi-hour sessions. Can capture calibration frames from the UI and have them ingested into the library. Can speak "image M42 for 2 hours with standard settings" and have the LLM plan and execute the full workflow with confirmation.

### Phase 4 — ML Processing + Advanced Features

- **ML-enhanced processing tools** (stacking server, GPU-accelerated):
  - ML noise reduction as alternative to wavelet/NLM (MLR-01)
  - ML star-starless separation as alternative to morphological methods (MLR-02)
  - ML background extraction (MLR-03)
  - ML deconvolution with spatially varying PSF (MLR-04)
  - Model management: download, version, pin per preset (MLR-05)
  - Traditional fallback for all ML steps (MLR-06)
  - ML always opt-in, never default (MLR-07)
  - ML diff visibility in before/after UI (MLR-08)
- **Reference-guided parameter tuning**:
  - Reference image input and analysis (MLR-09, MLR-10)
  - Parameter suggestion from reference profile (MLR-11, MLR-12)
  - Reference image library per target (MLR-13)
  - Style transfer tone mapping (MLR-14)
  - Provenance tracking for ML and reference usage (MLR-15)
- **Advanced post-processing tools**:
  - Photometric color calibration via catalog star colors (PPR-06)
  - Channel mixing / RGB combination (PPR-08)
  - Luminance noise reduction: wavelet-based / non-local means (PPR-14)
  - Chrominance noise reduction (PPR-15)
  - Multiscale noise reduction (PPR-16)
  - Unsharp mask sharpening (PPR-17)
  - Wavelet sharpening at selectable scales (PPR-18)
  - Deconvolution: Richardson-Lucy / Wiener (PPR-19)
  - Star-starless separation and independent processing (PPR-20, PPR-21, PPR-22, PPR-23)
  - Narrowband palette mapping SHO/HOO (PPR-11)
  - Annotation overlay with DSO labels and grid (PPR-27)
  - Resample / downsample for export (PPR-26)
- **LLM advanced features**:
  - Comparative session analysis (LLM-12)
  - Voice output / text-to-speech (LLM-14)
  - Local model support via ollama (LLM-16)
  - Session conversation history (LLM-18)
- Calibration library management UI (CAL-08, CAL-12, CAL-13)
- Solve result overlay with annotated stars and grid (PLS-08)
- Blind solve fallback (PLS-09)
- Drizzle integration (STK-15)
- Per-channel RGB stacking (STK-11)
- Mosaic planner (PLN-07)
- Calibration frame automation (SES-09)
- PEC recording/playback (MNT-13)
- Multi-star alignment (MNT-14)
- Red/night mode UI (USB-02)
- Plugin/hook system for custom automation
- **INDI driver adapter**: wrap any INDI device as MountDevice, Camera, or GuideCamera (HAL-09)
- **ASCOM/Alpaca adapter**: wrap ASCOM Alpaca REST devices (HAL-10)
- **FilterWheel interface and drivers** for automated filter changes (HAL-12)
- **Focuser interface and drivers** for motorized focus control (HAL-13)
- **Driver hot-swap**: switch devices without restart (HAL-14)

## 10. Risks and Mitigations

| Risk | Impact | Likelihood | Mitigation |
|------|--------|-----------|------------|
| Skywatcher protocol documentation is incomplete/inaccurate — **this risk fired.** The timer frequency in §4.2 was wrong by 7.1× (2026-07-29 read-only handshake) | Mount misbehaves, gear damage possible | **Occurred, contained** | Caught before any motion by the read-only handshake, and its blast radius was already limited by the design decision to read CPR/timer-freq from the mount at handshake rather than hardcoding them (SDD §5.2.3). All §4.2 constants are now verified against the hardware (the high-speed ratio by a follow-up read-only survey). What remains unverified is behaviour under motion, not parameters. Continue to cross-reference EQMOD source; verify positions before high-speed slews |
| Open-loop steppers lose steps (wind gust, cable snag, imbalance) — the position counter silently diverges from true pointing (the HEQ5 Pro has no encoders) | Target drifts out of frame; goto accuracy degrades through the night | Medium | Continuous pointing verification via plate solving (PLS-05); sync after solve (MNT-10); solve-and-center before each sequence (PLS-03) |
| ~~Canon R10 gPhoto2 support gaps (CR3 bulb quirks, live view latency)~~ — **RETIRED 2026-07-29** | — | — | Measured on the hardware: bulb via `eosremoterelease` works, CR3 downloads at 32 MB, live view runs at 58.5 fps against a 5 fps requirement. Evidence: `spikes/gphoto2-r10/FINDINGS.md` |
| USB disconnect during long exposure | Lost frame, mount continues uncontrolled | Medium | Watchdog timer on serial heartbeat, auto-stop on camera disconnect, sequence state persistence |
| Rust binding maturity gaps. Surveyed against crates.io: `gphoto2` 3.4.1 is healthy (the residual risk is R10 bulb/CR3 *coverage*, not the binding); liberfa bindings exist (`erfars`, `erfa-sys`); **RAW-decode bindings are immature** (`libraw` 0.1.1) | Camera features unavailable; preview pipeline blocked; coordinate errors | Low-Medium | Prototype the camera driver first (M2-T01), which also selects the RAW decoder on evidence from a real CR3; per-operation fallback to `gphoto2` CLI subprocess; CI parity tests of erfa transforms against astropy — binding the same C library astropy uses, never the `erfa` reimplementation crate |
| **No Rust binding exists for libsep** — star detection has no off-the-shelf crate, only an empty placeholder | Control pipeline (FWHM, quality scoring), registration, and guiding all depend on it; discovered late it would stall Phase 2a | Medium | Write a thin in-tree `sep-sys` FFI binding — libsep's API is small (background, extract, aperture photometry). Spike it at the *start* of Phase 2a, on the M2-T01 pattern: prove the binding against a real frame before designing the pipeline around it |
| Rust↔Python worker IPC adds protocol and lifecycle complexity on the stacking server | Worker crash or protocol drift breaks the processed pipeline | Low-Medium | Reuse the proven rifflab backbone/worker pattern: supervised long-running workers, versioned JSON protocol, automatic restart; capture is unaffected (the field node has no Python) |
| Pi 4/5 USB bandwidth under load (mount serial + camera PTP + guide camera) | Dropped frames or serial timeouts | Low | Separate USB buses for devices, use hub with per-port power |
| Plate solve failure in poor conditions (few stars, clouds, dew) | Solve-and-center loop hangs, sequence stalls | Medium | Configurable timeout and max retries; fall back to mount coordinates if solve fails; frame quality gate before attempting solve |
| ASTAP/astrometry.net index file mismatch with camera field of view | Solve never succeeds or takes minutes | Medium | Document required index file ranges for Canon R10 + common focal lengths; validate index coverage at startup |
| LAN connectivity loss between field node and stacking server | No live stacking preview, frames queue on field node | Medium | Resilient transfer queue with retry (ARC-11); field node continues capturing regardless (ARC-09); stacking server catches up on reconnection (REL-07) |
| VPN tunnel instability (cellular field connectivity, ISP issues) | Operator loses control of rig, frame transfer interrupted | Medium | Sequences run autonomously once started (REL-09); WebSocket auto-reconnect (REL-10); emergency stop has priority delivery (PRF-12); frame queue survives disconnection (REL-06) |
| High VPN latency degrades live view and stacking preview | Operator perceives UI as sluggish, live view stutters | Medium | Adaptive JPEG quality and frame rate for live view (USB-11, PRF-02); stacking preview is inherently low-frequency (one update per capture); mount status updates degrade gracefully (PRF-01) |
| LAN throughput insufficient for full-resolution CR3 frames over VPN | Frame transfer falls behind capture rate, growing backlog | Low-Medium | CR3 frames measure **32 MB** for a lit full-RAW frame (measured; heavily compressible — a dark frame came in at 1.7 MB); VPN throughput varies — on fast links this is fine, on cellular it may lag; transfer is queued and non-blocking; optionally transfer JPEG for stacking with RAW synced later |
| Frame registration failure accumulates (bad reference frame, field rotation) | Stack degrades over time, stars trail | Low | Periodic reference frame refresh; reject frames with poor match quality; log registration residuals for diagnostics |
| Mid-session re-stack competes with live stacking for CPU/memory on stacking server | New frames queue while rebuild runs, preview stalls temporarily | Medium | Rebuild runs on a separate thread/process; new frames queued and applied after rebuild completes (IPP-16); post-processing chain changes bypass the rebuild entirely (IPP-17, PPR-30); UI shows rebuild progress so operator knows it's working |
| ML model produces artifacts (ringing, hallucinated structure, over-smoothing) that operator doesn't notice | False detail in final image; scientific/artistic integrity compromised | Medium | ML is opt-in only (MLR-07); before/after comparison always available (MLR-08, PPR-31); provenance tracking records all ML usage (MLR-15); traditional fallback for every ML step (MLR-06); core principle documented: "ML enhances signal, never fabricates" |
| ML model trained on different sensor/optic characteristics than the operator's equipment | Noise model mismatch causes under/over-processing, color shifts, pattern artifacts | Medium | Model management allows selecting models appropriate for OSC/mono, specific sensor sizes (MLR-05); operator can A/B test ML vs. traditional on their data; model metadata includes training data characteristics |
| Reference-guided parameter tuning pushes all outputs toward a homogeneous "look" | Loss of artistic individuality; operator doesn't develop processing skills | Low | Reference suggestions are always presented as a starting point, never auto-applied (MLR-12); operator must explicitly accept; UI encourages further tweaking after applying suggestion |
| LLM misinterprets a command and executes an unintended action (wrong target, wrong settings) | Wasted imaging time, possible data loss if sequence interrupted incorrectly | Medium | Tiered execution model (LLM-05): medium/high risk actions require explicit confirmation; LLM explains its plan before acting (LLM-06); emergency stop is hardware-only, never LLM-triggered; all LLM actions logged (LLM-19) |
| LLM API unavailable in the field (no cellular signal, API outage) | Operator loses natural language control | Medium | LLM is an enhancement layer, not a dependency (LLM-20, ARC-21); all controls remain fully functional via manual UI; field node works offline for all hardware and capture operations |
| LLM API costs accumulate over long sessions (many tool-use calls with full system state context) | Unexpected expense, especially with verbose context on every interaction | Low | Provider-configurable (LLM-16) — use local ollama model for zero cost; context can be summarized rather than sent in full; API usage tracked and displayed in session logs |
| LLM hallucinates astronomy knowledge (wrong transit times, incorrect object coordinates, bad parameter advice) | Operator trusts incorrect advice, wastes session time or captures wrong target | Medium | All LLM goto/slew commands verified against catalog data and plate solving; LLM advice is always advisory alongside the operator's own judgment; obvious errors (object below horizon) caught by validation before execution |
| Calibration library mismatches (wrong dark applied, stale flats) | Artifacts in stacked image, calibration residuals | Medium | Strict metadata matching with warnings for edge cases; staleness tracking (CAL-13); temperature tolerance clearly communicated; preview shows calibration result so issues are visible immediately |
| Sensor temperature not available from Canon R10 EXIF | Cannot temperature-match darks automatically | Medium | Fall back to ambient temperature estimate or manual annotation; if Canon doesn't embed sensor temp, match darks by session date proximity as secondary key |

## 11. Out of Scope

- Windows or macOS support (Linux-only, USB serial and libgphoto2 paths assumed; ASCOM/Alpaca adapter in Phase 4 enables limited cross-platform device access)
- VPN provisioning and management (assumes an existing VPN such as NetBird or Tailscale is configured and operational)
- Planetarium/sky map in the UI (use Stellarium separately for visual planning)
- Dedicated drivers for hardware beyond the reference implementations (Skywatcher mount, Canon gPhoto2, ASI/QHY guide cameras) — additional hardware is supported via INDI and ASCOM/Alpaca adapters (Phase 4), not bespoke drivers
- Third-party driver development — the HAL enables community-contributed drivers but AstroCtl does not ship or maintain them
- Online plate solving services (nova.astrometry.net) — all solving is local/offline
- Native app store distribution (Capacitor/Ionic wrapper is a future option if PWA limitations arise on iOS, but not in initial scope)
- Generative AI image enhancement — no GAN/diffusion-based "super-resolution" or detail synthesis; ML is limited to signal extraction and traditional-algorithm replacement (core principle: "ML enhances signal, never fabricates signal")
- Training custom ML models — AstroCtl uses pre-trained models; training pipelines and datasets are out of scope
- LLM fine-tuning — the system uses general-purpose LLMs with tool-use; no domain-specific fine-tuning is required or planned
- Autonomous unattended operation without operator oversight — the LLM assists and executes confirmed commands, but is not designed to run an entire session without any human interaction

## 12. Success Metrics

- Phase 1 complete: full imaging session (slew + capture + download) through the PWA on a tablet over VPN
- Phase 2a complete: unattended 2-hour multi-target session with dithering, solve-and-center achieving < 1 arcmin pointing accuracy, zero lost frames — field node alone
- Phase 2b complete: frames streaming to stacking server, calibrated live stack visible in browser, exportable as 16-bit FITS, calibration library contains at least one complete profile (populated via master import), zero lost frames across a simulated VPN outage
- Phase 2c complete: post-processing adjustable mid-session with results visible in seconds, completed sessions reprocessable with different parameters, LLM text commands controlling mount/camera/pipeline with server-enforced confirmation
- Phase 3 complete: guided session with < 1" RMS total, automatic meridian flip, polar alignment error measured and correctable to < 2 arcmin, 4+ hours unattended, calibration frames captured from UI, voice-controlled session planning and execution
- Phase 4 complete: ML noise reduction and star separation producing visibly better results than traditional algorithms on the same data (A/B test), reference-guided parameter tuning generating usable presets from exemplar images, local ollama model operational as LLM backend
- Startup to first capture: < 60 seconds on field node (connect, configure, shoot)
- UI latency: position updates visible within 200ms of position-counter read
- Plate solve time: < 5s with ASTAP (with hint), < 30s astrometry.net blind solve
- Frame delivery: field node to stacking server < 5s per frame on gigabit LAN
- Live stacking: calibrated + stacked preview visible in browser within 10s of each new frame completing download (the IPP-06 end-to-end envelope)
- Post-processing: parameter changes applied to stack within 3s; full chain re-render within 1s on cached intermediates
- Calibration reuse: same master darks/flats applied across multiple sessions without re-acquisition
- LLM command success rate: > 90% of natural language commands executed without requiring operator re-issue or manual correction, measured from the LLM interaction logs (LLM-19)
- ML provenance: 100% of pipeline outputs that used ML or reference guidance have full provenance metadata recorded
