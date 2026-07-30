# AstroCtl — Architecture Design Document

**Document ID:** ASTROCTL-ADD-001
**Version:** 1.5.1
**Author:** Artiom
**Date:** 2026-07-29
**Status:** Draft
**Conformance:** ISO/IEC/IEEE 12207:2017 (Architecture Definition process, §6.4.4); architecture description per ISO/IEC/IEEE 42010:2022
**Governing requirements:** ASTROCTL-PRD-001 v1.18.0
**Change note (1.1.0):** Implementation technology revised — Rust backbone with Python confined to stacking-server workers (ADR-03 reversed, ADR-13 added), aligning with the rifflab architecture.
**Change note (1.2.0):** Guiding given an explicit architectural home (§5.2.1 Guiding Service, `astroctl-guiding` crate in §5.6) — GDE-* previously had no element. E-stop latency budget split into its three distinct measurement points (§9.1, §10). Repository-directory vs. artifact-name convention stated in §5.6. EXT-04 traced (§11).
**Change note (1.2.1):** Governing-requirements pin advanced (dependency survey). §10's erfa risk restated: the mitigation depends on binding the *same C library astropy uses*, which the crate survey showed is not what the similarly-named `erfa` crate provides.
**Change note (1.2.2):** Pin advanced to PRD v1.8.0. The erfa risk is now largely retired by build evidence — `erfars` vendors the ERFA C source, so the parity suite tests our usage of the same library astropy wraps, and no system liberfa is involved.

**Change note (1.2.3):** §10's top risk — gphoto2 coverage for the R10 — retired on hardware evidence.

**Change note (1.2.4):** Pin advanced to PRD v1.10.0 (runtime sizing config).

**Change note (1.2.5):** Pin advanced to PRD v1.11.0 (mount parameters verified on hardware).

**Change note (1.2.6):** Pin advanced to PRD v1.11.1.

**Change note (1.3.0):** §5.6 dependency rules 1 and 6 disambiguated during M0-T01, when they turned out not to be mechanically enforceable as written. Rule 1's "except the registry" had no literal implementation — `DriverRegistry` is a HAL type, so `astroctl-hal` cannot depend on the drivers it registers without a cycle; the rule now states that only the two binaries may name concrete drivers. Rule 6 conflicted with rule 5 over `astroctl-ipc`, which is worker-*related* but is inert protocol definitions; rule 6 now excludes GPU/ML runtimes and worker process management specifically, not the protocol crate.

**Change note (1.3.1):** §5.5 gains the development/CI topology — two containers on separate network namespaces, which is how a single workstation exercises the two-node shape honestly and how the §9.1 latency and head-of-line-blocking requirements become automatically testable. Distinct from the STK-20 degenerate single-host case, which proves nothing about the deployment.

**Change note (1.4.0):** ADR-12 extended — Android/Chrome is the supported PWA target and the iOS-compatible-subset discipline is recorded as rejected, with reasons. Paying its cost would have bought an option nobody holds while giving up Screen Wake Lock, which matters when the operator watches a live view for minutes at a time.

**Change note (1.4.1):** §5.6 records a watch item — rules 5 and the core/axum boundary together leave the two binaries with no shared home for HTTP-layer code, so ~700 lines are duplicated. Fine at M0 size; if M1 grows it, an `astroctl-api` crate between core and the binaries closes the drift risk without violating any rule.

**Change note (1.5.0):** §4's context diagram already showed `HTTPS/WSS` on the operator link but never said where TLS terminates or why it was not optional. Both are now recorded: termination is **in `astroctl-field`**, because a cloud proxy would put a WAN round trip in front of every live-view frame on the cellular link the two-node split exists to work around (and contradicts ARC-06), while a sidecar adds a second process to supervise on a Pi already running the mount and camera. The reason it is mandatory is browser secure-context gating of wake lock, service workers and installability — not confidentiality, which the VPN already provides. Aligns with PRD 1.16.0's SEC-05/06/07/08.

**Change note (1.5.1):** §5.5 said the container topology exercises "two independent tokens". It does not, and neither does any deployment: PRD §8.1 and §8.2 both name `ASTROCTL_TOKEN`, SDD §4.5 calls it the shared token, and ADR-07 has the field node forward the operator's credential when it proxies — there is no key anywhere for a second one. Corrected during M0-T08, which is the task that had to build against it.

---

## 1. Introduction

### 1.1 Purpose

This document is the output of the Architecture Definition process (ISO/IEC/IEEE 12207:2017 §6.4.4) for AstroCtl. It records the stakeholder concerns the architecture addresses, the viewpoints and views used to describe it, the selected architecture and its rationale, the candidate architectures that were evaluated and rejected, and traceability from architectural elements to the system requirements in ASTROCTL-PRD-001.

It is the reference for the subsequent Design Definition process (§6.4.5): detailed design of each element identified here must conform to the element boundaries, interfaces, and constraints defined in this document.

### 1.2 Scope

The architecture covers the complete AstroCtl system as specified in the PRD: field node, stacking server, operator PWA, and the communication fabric between them. Hardware itself (mount, camera), the VPN, and LLM provider services are external systems at the context boundary (§5.1).

### 1.3 References

| Reference | Title |
|-----------|-------|
| ASTROCTL-PRD-001 v1.9.0 | AstroCtl Product Requirements Document (governing requirements; all `XXX-nn` IDs cited below refer to it) |
| ISO/IEC/IEEE 12207:2017 | Systems and software engineering — Software life cycle processes |
| ISO/IEC/IEEE 42010:2022 | Software, systems and enterprise — Architecture description |

### 1.4 Definitions and abbreviations

| Term | Meaning |
|------|---------|
| HAL | Hardware Abstraction Layer (PRD §4.1) |
| Field node | Linux computer at the rig controlling mount/camera (laptop or Pi) |
| Stacking server | High-performance Linux PC running the processed pipeline |
| Control / live view / processed pipeline | The three image pipelines of PRD §5.9 |
| ADR | Architecture decision record (§7 of this document) |
| E-stop | Emergency stop (MNT-08, REL-01) |
| WS | WebSocket |

---

## 2. Stakeholders and Concerns

Per 12207 §6.4.4.3(a), stakeholder concerns drive viewpoint selection.

| Stakeholder | Concerns | Addressed in |
|------------|----------|--------------|
| Operator (single user, remote via VPN) | Rig controllable and observable from tablet/phone; nothing lost when links drop; safety of the mount; responsive UI in the field | Context (§5.1), Deployment (§5.5), Runtime (§5.4), Quality (§9) |
| Operator as maintainer/extender | Can add drivers, script the system via REST, understand what the software did | Development view (§5.6), Interfaces (§6), HAL rules |
| Future driver contributors | Stable abstract interfaces, no coupling to orchestrator internals | Functional view (§5.2), ADR-02 |
| Data integrity (the operator's photons) | Raw frames immutable, calibration correct, ML provenance, reproducible processing | Information view (§5.3), ADR-08, ADR-10 |
| Safety of equipment | E-stop always works, slew limits enforced for every caller, open-loop mount drift detected | Runtime view (§5.4), §9.3 |

---

## 3. Architecture Viewpoints

Per 12207 §6.4.4.3(b)/42010, the following viewpoints are used. Each view in §5 conforms to one viewpoint.

| Viewpoint | Concerns framed | Model kinds | View |
|-----------|-----------------|-------------|------|
| Context | System boundary, external actors and systems | Context diagram, external interface list | §5.1 |
| Functional | Decomposition into components, responsibilities, dependencies | Component diagrams, responsibility tables | §5.2 |
| Information | Data entities, ownership, storage layout, lifecycle | Data model, directory/schema layouts | §5.3 |
| Runtime (concurrency) | Processes, tasks, queues, latency paths, failure behavior | Process/task diagrams, scenario walkthroughs | §5.4 |
| Deployment | Mapping of components to nodes and networks | Deployment diagram, port map | §5.5 |
| Development | Code organization, dependency rules, build | Package layout, allowed-dependency matrix | §5.6 |

---

## 4. Architecture Overview

AstroCtl is **two self-contained Rust services plus a browser PWA**, connected only by authenticated REST + WebSocket over routed IP (VPN). Python exists only as supervised worker processes on the stacking server, where its ecosystem is necessary (GPU array compute, ML inference) — the same backbone/worker split proven in rifflab (ADR-03, ADR-13):

- **`astroctl-field`** — the field node service, **pure Rust, one static binary**. Owns all hardware, the session orchestrator, the control and live view pipelines, the frame archive of record during capture, the transfer agent, the LLM agent, and the PWA (which it serves and for which it proxies the stacking server). No Python runtime on the field node.
- **`astroctl-stack`** — the stacking server service: a Rust backbone owning frame ingestion, the calibration library index, job control, preview streaming, and the authoritative long-term archive, supervising **Python compute/ML workers** that execute the numeric pipeline (calibrate, debayer, register, accumulate, post-chain, ML) on the GPU.

Each service is a **modular monolith**: one deployable backbone process per node, with strict internal module boundaries (§5.6) rather than intra-node microservices (rationale: ADR-01); the Python workers are the only additional OS processes, and they are children of the backbone. The field node is fully functional without the stacking server (ARC-06, ARC-09), without the LLM (ARC-21), and without the operator connected (REL-09).

```
Operator PWA ──HTTPS/WSS (VPN)──► astroctl-field ──HTTP/WS (VPN)──► astroctl-stack
                                     │  │                             │        │
                                  serial gphoto2                   Python    CUDA GPU
                                  (mount) (camera)                 workers  /data archive
                                                                   (IPC, supervised)
```

**TLS terminates in `astroctl-field`, not in front of it** (SEC-05). The obvious alternatives both
break something the architecture depends on: a cloud reverse proxy puts a WAN round trip in front
of every live-view frame and every event, on the cellular link the two-node split exists to work
around, and it contradicts ARC-06's standalone operation; a local proxy sidecar adds a second
process to supervise on a Pi that already runs the mount and camera. The field node is the only
process that must be up for the system to function, so it is the right place for the certificate.

The reason this is `HTTPS` rather than an option is not confidentiality — the VPN already provides
that. It is that browsers gate the Screen Wake Lock API, service-worker registration and
`beforeinstallprompt` behind a *secure context*, and a tunnelled `http://` origin does not qualify.
USB-09 and USB-10 are therefore unreachable without it. The field↔stack hop stays plain HTTP
(SEC-09): no browser is involved, so nothing is gated.

---

## 5. Architecture Views

### 5.1 Context View

```
                              ┌─────────────────────────────┐
        (USB serial)          │            AstroCtl          │        (HTTPS)
 HEQ5 Pro mount ◄────────────►│                             │◄──────────────► LLM provider
                              │  ┌──────────┐ ┌──────────┐  │            (Anthropic/OpenAI/ollama)
 Canon R10 ◄─────────────────►│  │ field    │ │ stacking │  │
        (USB PTP)             │  │ node     │ │ server   │  │        (CLI subprocess)
                              │  └──────────┘ └──────────┘  │◄──────────────► ASTAP /
 Guide camera (Ph.3) ◄───────►│                             │                 astrometry.net
        (USB SDK)             └──────┬───────────────┬──────┘
                                     │ (VPN, WSS)    │ (filesystem)
                              Operator device   /data volumes
                              (browser PWA)    (sessions, calibration, models)
```

External interfaces:

| External entity | Interface | Owned by | PRD |
|-----------------|-----------|----------|-----|
| Mount | Skywatcher serial protocol, 9600 8N1 | `SkywatcherMount` driver | §4.2 |
| Camera | PTP/MTP via libgphoto2 | `CanonGPhoto2Camera` driver | §4.3 |
| Guide camera (Phase 3) | ASI/QHY SDK, INDI | GuideCamera drivers | §4.4 |
| Plate solvers | `astap` / `solve-field` CLI subprocess | Solver adapter | PLS-01 |
| LLM provider | HTTPS API (provider-agnostic tool schema) | LLM agent service | LLM-16 |
| Operator device | HTTPS/WSS to field node only (single URL) | API gateway + PWA | ARC-13, STK-19 |
| VPN | Assumed routed IP; out of scope | — | §11 (PRD), SEC-01 |

### 5.2 Functional View

#### 5.2.1 Field node components

```
┌────────────────────────── astroctl-field ───────────────────────────────┐
│                                                                          │
│  ┌────────────── API Gateway (axum) ────────────────────────────┐        │
│  │ REST routers │ WS hub │ PWA static │ stack proxy │ auth m/w  │        │
│  └──────┬───────────▲──────────────────────────▲────────────────┘        │
│         │           │ events                   │                         │
│  ┌──────▼──────┐ ┌──┴──────────┐  ┌────────────┴─┐  ┌────────────────┐   │
│  │ Session     │ │ Event Bus   │  │ Confirmation │  │ LLM Agent      │   │
│  │ Orchestrator│ │ (async      │  │ Service      │  │ Service        │   │
│  │ (FSM)       │ │  pub/sub)   │  │ (SEC-03)     │  │ (tools = API)  │   │
│  └──┬───┬───┬──┘ └─────────────┘  └──────────────┘  └────────────────┘   │
│     │   │   │                                                            │
│  ┌──▼─┐ │ ┌─▼──────────┐  ┌──────────────┐  ┌───────────────────────┐   │
│  │HAL │ │ │ Control    │  │ Live View    │  │ Planning Service      │   │
│  │Reg.│ │ │ Pipeline   │  │ Pipeline     │  │ (erfa coordinates,    │   │
│  └─┬──┘ │ │ (solve,    │  │ (decode,     │  │  catalog, LST, site)  │   │
│    │    │ │  metrics)  │  │  stretch)    │  └───────────────────────┘   │
│ drivers │ └────────────┘  └──────────────┘                               │
│ ┌──────┴───────┐  ┌───────────────┐  ┌──────────────┐  ┌─────────────┐  │
│ │ Safety       │  │ Frame Store   │  │ Transfer     │  │ Config +    │  │
│ │ Monitor      │  │ (sessions on  │  │ Agent        │  │ Logging     │  │
│ │ (limits,     │  │  disk, meta)  │  │ (queue,      │  │             │  │
│ │  watchdogs,  │  └───────────────┘  │  checksum,   │  └─────────────┘  │
│ │  e-stop)     │                     │  retry)      │                    │
│ └──────────────┘                     └──────────────┘                    │
└──────────────────────────────────────────────────────────────────────────┘
```

| Component | Responsibility | Key PRD requirements |
|-----------|----------------|----------------------|
| **API Gateway** | REST routers per domain (mount, camera, session, solver, planning, llm, system); WS hub broadcasting event-bus topics; serves PWA bundle; reverse-proxies `/stack/*` to the stacking server; auth middleware validates tokens | ARC-01, ARC-07, ARC-10, STK-19, SEC-02, ARC-14 |
| **Auth middleware + Confirmation Service** | Token check on every endpoint; issues single-use confirmation tokens on operator approval; medium/high-tier endpoints reject calls without one — server-side, caller-agnostic | SEC-02, SEC-03, LLM-05 |
| **HAL Registry + drivers** | Abstract `MountDevice`/`Camera`/`GuideCamera` interfaces; driver registration by config name; auto-detection; simulators. Drivers are leaf modules with no dependencies on anything above the HAL | HAL-01..14, ARC-04, EXT-01, EXT-02 |
| **Safety Monitor** | Enforces altitude/meridian limits on every motion request regardless of caller; serial heartbeat and USB-disconnect watchdogs; owns the priority e-stop path (§5.4.3) | MNT-08, MNT-15, MNT-16, REL-01..03, PRF-12 |
| **Session Orchestrator** | Finite state machine executing sequences (slew → settle → solve-and-center → capture → dither → repeat); multi-target queue; pause/resume/abort; state persisted to disk each transition | SES-01..09, REL-04, PLS-03 |
| **Control Pipeline** | Per-frame astrometric/quality analysis: solver adapter (ASTAP/astrometry.net subprocess), star detection (sep), FWHM/HFR, quality score. Feeds orchestrator decisions | IPP-01..03, PLS-01..06, STK-27 inputs |
| **Live View Pipeline** | Camera JPEG preview relay; reduced-resolution debayer + stretch of last captured frame; optional overlays | IPP-04, IPP-05, CAM-05, CAM-06 |
| **Guiding Service** (Phase 3) | Closed-loop autoguiding: guide-camera frame acquisition, star detection and sub-pixel centroid tracking (libsep), PI correction controller, guide-pulse emission through the mount facade, RMS/correction history for the UI. Runs wholly inside the field process so the loop never crosses the VPN (PRF-03); dithering (SES-05) is driven from the orchestrator through this service | GDE-01..05, PRF-03, SES-05, MNT-12 |
| **Planning Service** | Site config, LST, target catalog, alt/az and visibility computation — erfa-based (liberfa FFI), CI-verified against astropy reference values | PLN-01..08 |
| **Frame Store** | Session directory structure (PRD §5.9), write-once raw frames, per-frame metadata JSON, disk monitoring | IPP-09, IPP-10, REL-05, REL-11, REL-12 |
| **Transfer Agent** | Durable on-disk queue of frames to push to the stacking server; SHA-256 per frame; retry with backoff; reclaim eligibility marking after verified transfer | STK-17, ARC-11, REL-06, REL-13, PRF-07 |
| **LLM Agent Service** | Provider adapters (Anthropic/OpenAI/ollama); tool schemas generated from the gateway's OpenAPI spec; system-state context assembly; conversation history; interaction logging. Calls the same authenticated REST API as the UI — no in-process shortcuts | LLM-01..20, ARC-20 |
| **Event Bus** | In-process async pub/sub; single source for WS broadcasts and the structured session log | ARC-07, EXT-05, SES-07 |

#### 5.2.2 Stacking server components

```
┌────────────────────────── astroctl-stack ────────────────────────────────┐
│  ┌───────────── API (axum) ─────────────────┐                            │
│  │ ingest │ control/config │ WS preview │ auth │                         │
│  └───┬──────────────▲─────────────▲──────────┘                           │
│  ┌───▼─────────┐    │             │                                      │
│  │ Ingest      │  ┌─┴─────────────┴──┐   ┌───────────────────────────┐   │
│  │ Service     │  │ Session Manager   │   │ Calibration Library       │   │
│  │ (verify,    │──► (archive of      │   │ (SQLite index, matcher,   │   │
│  │  dedup)     │  │  record, meta)   │   │  master generation)       │   │
│  └─────────────┘  └───────┬──────────┘   └────────────┬──────────────┘   │
│                           ▼                           │                   │
│  ┌────────────────── Processed Pipeline ──────────────▼──────────────┐   │
│  │ calibrate → debayer → detect → register → accumulate → post-chain │   │
│  │  (Python compute workers: CuPy/PyTorch CUDA, numpy/CPU fallback,  │   │
│  │   supervised children of the Rust backbone over IPC — ADR-13)     │   │
│  └───────────┬───────────────────────────────────────────┬───────────┘   │
│  ┌───────────▼───────────┐  ┌──────────────┐  ┌──────────▼───────────┐   │
│  │ Rebuild Manager       │  │ ML Runtime + │  │ Preview + Export     │   │
│  │ (re-stack jobs, queue │  │ Model Manager│  │ Service (WS push,    │   │
│  │  new frames during)   │  │ (opt-in)     │  │  FITS/TIFF/JPEG)     │   │
│  └───────────────────────┘  └──────────────┘  └──────────────────────┘   │
└──────────────────────────────────────────────────────────────────────────┘
```

| Component | Responsibility | Key PRD requirements |
|-----------|----------------|----------------------|
| **Ingest Service** | Receives frame uploads, verifies checksum, deduplicates, acknowledges (ack ⇒ field node may mark reclaim-eligible); tolerates late arrivals | STK-17, REL-13, IPP-15 |
| **Session Manager** | Mirrors session structure; authoritative archive; multiple processed outputs per session; disk monitoring | IPP-09, IPP-11, REL-12 |
| **Calibration Library** | SQLite index over master frames; profile matching (temperature tolerance, date-proximity fallback); master generation; import | CAL-01..13 |
| **Processed Pipeline** | The quality pipeline: calibration, full-res debayer, registration, selectable accumulation, ordered post-processing chain with per-step caching | STK-01..34, PPR-01..33, IPP-06..08 |
| **Compute Workers (Python)** | Supervised child processes executing the numeric pipeline and ML: CUDA (CuPy/PyTorch) with automatic CPU (numpy) fallback; VRAM budget and tiling; versioned IPC protocol, frames passed by filesystem path (same host); auto-restart on crash | CMP-01..07, ARC-22, ADR-13 |
| **Rebuild Manager** | Background full re-stacks on parameter change; queues incoming frames during rebuild; swaps accumulator atomically on completion | IPP-08, IPP-16, PRF-13 |
| **ML Runtime + Model Manager** | ONNX/PyTorch inference as post-chain steps; model versioning and preset pinning; traditional fallback per step; provenance recording | MLR-01..15, ARC-18, ARC-19 |
| **Preview + Export Service** | Stretch current stack, push via WS within 1s of accumulation; export FITS/TIFF/JPEG | STK-04..07, PRF-09, PPR-28 |

### 5.3 Information View

#### 5.3.1 Principal data entities

| Entity | Owner | Storage | Mutability |
|--------|-------|---------|-----------|
| Raw frame (CR3/FITS) | Frame Store (field) → Session Manager (stack, authoritative) | Session `frames/` dir | **Immutable** (REL-11); field copy reclaim-eligible only after verified transfer (REL-13) |
| Session metadata | Orchestrator | `session.json` + per-frame JSON in `control/` | Append-only during session |
| Transfer queue entry | Transfer Agent | Spool dir + SQLite journal | Removed on ack |
| Calibration master | Calibration Library | `calibration/profiles/...` + SQLite index | Replace-only via library operations |
| Pipeline config / preset | Processed Pipeline | YAML per processed output; named presets | Versioned copies, never edited in place |
| Processed output | Processed Pipeline | `processed/<name>/` per parameter set | New name per parameter set (IPP-11) |
| Provenance record | ML Runtime / post-chain | Inside `pipeline.yaml` + `processing.log` | Append-only (MLR-15) |
| LLM interaction log | LLM Agent Service | Session `llm/` JSONL | Append-only (LLM-19) |
| Equipment profile | Calibration Library | `profile.json` | Edited via library API |

Directory layouts follow PRD §5.8 and §5.9 verbatim; this document does not redefine them.

#### 5.3.2 Data flow (one light frame, happy path)

```
Camera ─► Frame Store (write CR3 + meta; fsync)          REL-05 — before anything else
              ├─► Control Pipeline (downsampled)  ─► solve/quality → orchestrator, event bus
              ├─► Live View Pipeline (1/4-res)    ─► stretched JPEG → WS
              └─► Transfer Agent (enqueue + SHA-256)
                        └─► Ingest (verify) ─► Session Manager ─► Processed Pipeline
                                                                     └─► accumulate → preview WS
                                                                     └─► post-chain → export on demand
```

#### 5.3.3 Index technology

SQLite is used for every durable index (calibration library, transfer journal, session index); plain JSON/YAML for human-facing metadata that travels with the data (session.json, pipeline.yaml, profile.json). Rationale: ADR-06. This satisfies CAL-10/REL-08 ("JSON or SQLite" — resolved to SQLite for indexes).

### 5.4 Runtime (Concurrency) View

#### 5.4.1 Field node process model

One OS process (pure Rust, tokio runtime), structured as:

- **Tokio async runtime** — API gateway, WS hub, orchestrator FSM, event bus, transfer agent, LLM agent. All hardware I/O is async (ARC-03).
- **Serial I/O task** — sole owner of the mount serial port. Commands arrive on a **two-lane queue**: normal lane (position polls, gotos) and priority lane (e-stop, limit stops) which preempts the normal lane. Position polling at 1 Hz min (MNT-02) with heartbeat watchdog (REL-02).
- **Camera I/O task** — sole owner of the gphoto2 context (libgphoto2 is not thread-safe); capture, download, live view frames; blocking libgphoto2 calls run on this dedicated thread, never on the async runtime.
- **Bounded blocking thread pool (rayon / `spawn_blocking`, 2–3 threads)** — CPU-bound work: CR3 decode (libraw) for previews, sep star detection, FWHM. Keeps the runtime responsive (PRF-04) and confines RAW-decode memory spikes to short-lived buffers (PRF-05).
- **Solver subprocess** — ASTAP/`solve-field` invoked per solve with timeout kill (PLS-04, risk: solve hang).

#### 5.4.2 Stacking server process model

- **Rust backbone (tokio)** — API, ingest, checksum verification, calibration matching, WS preview streaming, job control, worker supervision.
- **Compute worker (Python, supervised child)** — owns the accumulator and GPU context; consumes a frame job queue over IPC (ADR-13); one frame at a time through calibrate→register→accumulate (PRF-08). Crash ⇒ backbone restarts it and replays the accumulator from session state; capture on the field node is never affected.
- **Rebuild worker (second compute-worker instance)** — launched by the Rebuild Manager for full re-stacks; works on a shadow accumulator; the live worker queues (does not process) new frames during rebuild; on completion the shadow accumulator is swapped in and the queue drains (IPP-16). Live preview continues serving the pre-rebuild stack until the swap.
- **Post-chain execution** — inside the compute worker; per-step output caching so late-step parameter changes re-run only downstream steps (PPR-30, IPP-17, PRF-13). ML inference runs in the same worker (or a dedicated ML worker instance) so a model fault cannot take down the backbone.

#### 5.4.3 Safety-critical path: emergency stop

```
UI tap ──WSS──► gateway /api/mount/estop (no auth-tier confirmation, token only)
                   └─► Safety Monitor ─► serial priority lane ─► :K/:L stop commands
Independent local triggers: serial heartbeat loss, camera USB disconnect,
meridian limit reached ─► same priority lane (no operator round-trip)
```

Design rules: the e-stop endpoint has a dedicated route with no queuing or middleware beyond token auth (REL-01); it is never exposed as an LLM tool (Blocked tier); the Safety Monitor evaluates altitude/meridian limits **inside the mount facade**, below the API layer, so every caller — UI, REST, LLM, orchestrator — passes through them (MNT-15).

#### 5.4.4 Degraded-mode behavior

| Failure | Behavior | PRD |
|---------|----------|-----|
| Stacking server / VPN down | Capture continues; frames spool in transfer queue; stack catches up on reconnect | REL-06, REL-07, ARC-09 |
| Operator device disconnects | Sequence continues autonomously; WS auto-reconnect resumes state | REL-09, REL-10 |
| LLM API unreachable | Manual UI unaffected; agent reports unavailability | LLM-20, ARC-21 |
| Solver failure/timeout | Bounded retries, then fall back to mount coordinates; sequence proceeds with warning | PRD risk table |
| GPU/CUDA absent | Compute Backend transparently selects CPU path | CMP-06 |
| Disk near-full | Warning, then graceful capture pause after in-flight frame | REL-12 |

### 5.5 Deployment View

```
        Operator device                Field node (Ubuntu)            Stacking server (Ubuntu)
        (any modern browser)           laptop / Raspberry Pi           Ryzen 9 / 128 GB / RTX 4090
 ┌──────────────────────┐   VPN   ┌──────────────────────────┐  VPN  ┌──────────────────────────┐
 │ PWA (installed,      │◄───────►│ astroctl-field :8470     │◄─────►│ astroctl-stack :8471     │
 │  service worker,     │  :8470  │  systemd service         │ :8471 │  systemd service         │
 │  offline shell)      │  only   │  /data/astro (SSD)       │       │  /data/astro (NVMe+bulk) │
 └──────────────────────┘         │  USB: serial, PTP        │       │  CUDA 12.x, models dir   │
                                  └──────────────────────────┘       └──────────────────────────┘
```

- The operator device talks **only** to the field node (`:8470`); `/stack/*` is proxied (STK-19, ARC-13). Direct browser→stack connection is a permitted optimization when VPN topology allows (PRD §5.7) but is not required.
- Both services bind the VPN interface (SEC-01) and require the shared token (SEC-02); each node has one YAML config file (ARC-05).
- Single-machine deployment: both services on one host with in-memory transfer shortcut — same code paths, loopback transport (STK-20, ARC-08 degenerate case).
- **Development and CI topology: two containers on separate network namespaces** (M0-T08), addressed by service name over a bridge network. This is not the degenerate case above — it exercises real TCP between nodes, the proxy host-to-host, and the **one shared token** both nodes name (SEC-02), on a single workstation. It also permits `tc` shaping of the inter-node link, which is how the latency and head-of-line-blocking requirements of §9.1 and SDD §8.3 get tested automatically rather than on a bench. What it does not cover is the VPN itself (MTU, NAT traversal, reconnection) or Pi hardware; those remain field-deployment gates.
- Time discipline on the field node via chrony/NTP or GPS (REL-14) is a deployment prerequisite checked at startup.

### 5.6 Development View

Monorepo, structured as a Cargo workspace plus Python workers — mirroring the rifflab layout.

**Naming convention:** the repository *directory* is `astrocli/` for historical reasons; every
artifact identifier — product name, both binaries, all crates, all document IDs — is
`astroctl`. Do not "correct" one to match the other in either direction; the directory name is
not an artifact name.

```
astrocli/                       # repo directory (see naming convention above)
├── Cargo.toml                  # workspace
├── crates/
│   ├── astroctl-core/          # shared types (serde models), events, config schema,
│   │                           #   auth, units/coordinate helpers
│   ├── astroctl-hal/           # MountDevice/Camera/GuideCamera async traits,
│   │                           #   capabilities, driver registry
│   ├── astroctl-drivers/       # skywatcher, gphoto2, simulators (feature-gated;
│   │                           #   indi/, alpaca/ adapters in Phase 4)
│   ├── astroctl-safety/        # limits, watchdogs, e-stop priority path
│   ├── astroctl-session/       # orchestrator FSM, sequence model, persistence
│   ├── astroctl-pipeline/      # control + live view pipelines (field-side)
│   ├── astroctl-solver/        # ASTAP / astrometry.net subprocess adapters
│   ├── astroctl-planning/      # erfa coordinates, LST, catalog, visibility
│   ├── astroctl-guiding/       # star centroids, PI controller, guide loop (Phase 3)
│   ├── astroctl-transfer/      # durable queue, checksums, retry
│   ├── astroctl-llm/           # provider adapters (HTTP), tool registry, tiers
│   ├── astroctl-ipc/           # worker protocol: versioned messages, supervision
│   ├── astroctl-field/         # field binary: axum app, WS hub, proxy, PWA serving
│   └── astroctl-stack/         # stack binary: ingest, calibration index, job
│                               #   control, rebuild manager, preview, export
├── workers/                    # Python (stacking server only)
│   ├── compute_worker.py       # calibrate/debayer/register/accumulate/post-chain
│   │                           #   (CuPy CUDA, numpy fallback)
│   ├── ml_worker.py            # model inference (PyTorch/ONNX Runtime)
│   └── requirements.txt
├── frontend/                   # React PWA → built bundle embedded in astroctl-field
└── docs/
```

**Dependency rules** (enforced by workspace structure and CI):

1. `astroctl-drivers` depends only on `astroctl-hal` + `astroctl-core`. Nothing above the HAL depends on a concrete driver: crates above it hold `Arc<dyn MountDevice>` and friends, never a driver type (HAL-01, ARC-04). **Only the two binaries may depend on `astroctl-drivers`** — the `DriverRegistry` is a HAL type (SDD §5.1), so `astroctl-hal` cannot itself depend on the drivers it registers without a dependency cycle, and the deployable that assembles the system is therefore what supplies the concrete driver set.
2. API layers (in the two binaries) depend on domain crates; domain crates never depend on the binaries — they emit events instead.
3. `astroctl-llm` reaches the system only through HTTP calls to the local API (ARC-20); it must not depend on `astroctl-session` or `astroctl-hal`.
4. Python workers communicate exclusively via the `astroctl-ipc` protocol; they never import backbone state and hold no sockets other than the IPC channel. All CuPy/PyTorch usage lives in `workers/` (CMP-06 CPU fallback is the workers' numpy path).
5. `astroctl-field` and `astroctl-stack` never depend on each other; they share only `astroctl-core`/`astroctl-ipc` and the HTTP contract.
**Watch item (raised by M0-T05):** rule 5 forbids the two binaries depending on each other and
SDD §4.2 keeps axum out of `astroctl-core`, so HTTP-layer concerns common to both nodes — auth
middleware, route metadata, telemetry setup, vitals, watchdog scaffolding, CLI — currently exist
as near-identical copies in each binary, on the order of 700 lines kept in step by review alone.
That is acceptable at M0 size and genuinely cheaper than a premature abstraction. **If M1 grows
it further, add an `astroctl-api` crate** that both binaries depend on: it sits above `core` and
below the binaries, so it violates no rule and closes the drift risk. Revisit when M1-T03 lands
the WS hub, which is the next substantial shared surface.

6. The field binary must build without any GPU or ML runtime, and without the compute workers — `workers/` is packaged with the stack service only. This does **not** exclude `astroctl-ipc`: that crate is protocol *definitions* (message types, framing), which are inert and cheap, and rule 5 explicitly permits both binaries to share it. What rule 6 forbids in the field binary is a dependency on a CUDA or ML runtime (`cudarc`, `cust`, `tch`, `ort`, and the like) or on worker process management.

---

## 6. Interface Definitions

### 6.1 External API surface (contract-first)

Both nodes expose OpenAPI-documented REST plus WS topics (EXT-03, EXT-05). Router groups and the tier annotation (SEC-03/LLM-05) — every endpoint declares its tier in the route metadata; the confirmation middleware and the LLM tool generator both consume the same declaration:

| Router (field :8470) | Representative endpoints | Tiers present |
|----------------------|--------------------------|---------------|
| `/api/mount` | position, goto, tracking, slew, park, sync, `POST /estop` | read → high; estop: blocked-for-LLM |
| `/api/camera` | settings, capture, bulb, liveview WS | read, low, medium |
| `/api/session` | sequences CRUD, start/pause/abort, queue | medium |
| `/api/solver` | solve, solve-and-center, status | low, medium |
| `/api/planning` | site, lst, catalog, visibility | read |
| `/api/llm` | chat, confirm/{token}, history | — |
| `/api/system` | health, config, disk, clock-sync status | read |
| `/stack/*` | reverse proxy to :8471 | pass-through |
| WS `/ws` | topics: mount.position, session.progress, frame.quality, liveview, alerts | — |

| Router (stack :8471) | Representative endpoints |
|----------------------|--------------------------|
| `/api/ingest` | frame upload (multipart + sha256), late-arrival ingest |
| `/api/stacking` | method/params get/set, rebuild trigger + progress, stats |
| `/api/postchain` | step list CRUD/reorder, per-step params, before/after, undo/redo |
| `/api/calibration` | profiles, masters, matching preview, import |
| `/api/ml` | models list/select/pin, enable per step |
| `/api/export` | FITS/TIFF/JPEG export jobs |
| WS `/ws` | topics: preview (binary JPEG), stack.stats, rebuild.progress |

### 6.2 Internal interfaces

- **HAL interfaces** — exactly as specified in PRD §4.1 (`MountDevice`, `Camera`, `GuideCamera`, future `FilterWheel`, `Focuser`); async Rust traits in `astroctl-hal`. These are the extension contract (EXT-01/02) and are semver-stable from Phase 1 on.
- **Worker IPC protocol** (`astroctl-ipc`, ADR-13) — versioned JSON messages over stdio between the stack backbone and the Python workers it spawns as child processes. This channel is strictly stacking-server-internal and never crosses the network — the field node delivers frames via HTTP (ADR-05), and only after a frame is on the stack's local disk does a worker get its path. Frames and large arrays are passed by filesystem path (parent and child share the same host filesystem), never serialized through the channel. Message families: job submit/progress/result, accumulator state save/load, health ping, capability report (GPU present, VRAM). Protocol version negotiated at worker startup; mismatch fails loudly.
- **Post-chain step contract** — `Step(config) → apply(image, ctx) → image`, executed worker-side, pure with respect to its input; declares a cache key from its params; ML steps additionally declare model+version into provenance (MLR-15, PPR-29/30). Step definitions and parameters live in `pipeline.yaml`, owned by the backbone.
- **Event schema** — versioned serde models in `astroctl-core`; WS payloads and session-log lines are the same objects (SES-07, EXT-05).

---

## 7. Architecture Decisions and Rationale

Per 12207 §6.4.4.3(d) — decisions, with alternatives considered and the reason for selection.

| ID | Decision | Alternatives considered | Rationale |
|----|----------|------------------------|-----------|
| ADR-01 | Modular monolith per node (one process each), not intra-node microservices | Per-component services; celery workers | Single operator, two hosts; process boundaries only where a real fault/latency boundary exists (the VPN, the GPU worker, solver subprocess). Fewer failure modes in the field; one systemd unit per node |
| ADR-02 | Custom HAL with thin drivers; INDI/ASCOM as *adapters behind* the HAL, not as the foundation | Build on INDI directly (Ekos model) | PRD's core complaint is opaque multi-process integration; direct drivers give full transparency and testability. INDI remains reachable (HAL-09) without inheriting its server topology |
| ADR-03 | Rust backbone (tokio/axum) on both nodes; Python confined to supervised stacking-server workers (GPU compute, ML inference) | All-Python asyncio/FastAPI (selected in ADD v1.0.0, reversed); all-Rust including ML | Field node becomes one static binary: ~10× lower memory than a Python stack (PRF-05), no GIL, no interpreter startup, robust multi-day processes on a Pi. The astronomy C libraries (libgphoto2, libsep, liberfa, libraw, cfitsio) bind to Rust as well as to Python. Python remains where its ecosystem is genuinely necessary — CuPy/PyTorch GPU compute and ML — but only in crash-isolated workers. Mirrors the proven rifflab backbone/worker architecture. All-Rust ML rejected: CUDA/ML ecosystem in Rust is immature |
| ADR-04 | REST + WebSocket only; no message broker | MQTT/Redis/NATS between nodes | Two nodes, one operator: a broker adds an always-on dependency and another failure mode; durable delivery needs are met by the on-disk transfer queue (ARC-11), which must exist anyway to survive restarts (REL-06) |
| ADR-05 | Frame transfer = HTTP multipart upload to stack ingest + SHA-256 ack | rsync-over-SSH (PRD option) | Single auth scheme (SEC-02), checksum ack integrates with reclaim policy (REL-13), works anywhere HTTP works; rsync remains a config option for bulk backfill |
| ADR-06 | SQLite for all durable indexes; JSON/YAML for data-adjacent metadata | JSON index files (PRD allowed either) | Atomic transactions for queue journal and calibration index under crash (REL-08); human-readable metadata still travels with the frames |
| ADR-07 | Operator single-URL topology: field node proxies the stack | Browser connects to both nodes | One URL/PWA origin, one token prompt, works in asymmetric VPN topologies (STK-19); direct connection kept as optimization |
| ADR-08 | Accumulator design: running statistics for live (mean/weighted/σ-approx), full in-RAM frame stack for exact median/σ-clip and rebuilds | Streaming-only; disk-backed mmap stack | 128 GB RAM makes the exact path feasible (PRF-10); live approximation + on-demand exact rebuild matches STK-32/IPP-16 |
| ADR-09 | All GPU work (CuPy for array compute, PyTorch/ONNX for ML) lives in the Python compute workers, never in the backbone | GPU calls from the backbone (Rust CUDA via cudarc) | Fault isolation: a CUDA OOM/driver fault kills a worker, not the API; single owner of VRAM budget per worker (CMP-07); avoids the immature Rust CUDA ecosystem entirely |
| ADR-10 | LLM agent as an ordinary authenticated API client; tools generated from OpenAPI + tier metadata | In-process function calls | ARC-20 (no privileged path); server-side tier enforcement stays the only gate (SEC-03); provider-agnostic by construction (LLM-16) |
| ADR-11 | E-stop and safety limits live in the mount facade below the API | Enforcement in API layer or UI | Every caller passes through, including future scripting (MNT-15); e-stop path has a dedicated priority lane to the serial task (PRF-12) |
| ADR-12 | React PWA served by the field node; WS-first state, REST for commands. **Android/Chrome is the supported target**; iOS untested, so EXT-06 is advisory and Android-only APIs (Screen Wake Lock, `beforeinstallprompt`) are permitted. Stack detailed in SDD §5.9 | Native app; iOS-compatible-subset discipline | ARC-02/ARC-14 mandates. The iOS-subset discipline was rejected because EXT-06's motivation was iOS PWA limitations and there is no iOS device in the deployment — paying its cost would buy an option nobody holds, while giving up Wake Lock, which matters when the operator watches a live view for minutes at a time |
| ADR-13 | Worker IPC: versioned JSON over stdio, frames passed by filesystem path, workers supervised (spawn, health-ping, auto-restart) by the stack backbone. This IPC exists only inside the stacking server, between the Rust backbone and its Python child processes — it never crosses the network; field→stack frame delivery is HTTP per ADR-05, and the frame is on the stack's local disk before any worker sees it | gRPC/ZeroMQ (extra infrastructure for two co-located processes); PyO3 embedding (brings the GIL and Python crash domain into the backbone process — defeats the purpose) | Same-host processes need no network transport; stdio+paths is the rifflab-proven minimum; crash isolation preserved; protocol versioning catches drift at startup |

## 8. Candidate Architectures Evaluated

Per 12207 §6.4.4.3(c), the significant whole-system candidates and their assessment:

| Candidate | Assessment | Outcome |
|-----------|-----------|---------|
| **A. Ekos/INDI composition** — orchestrate existing tools (INDI, PHD2, Siril) with a web frontend | Fastest to first light, but reproduces the PRD's core problem: opaque multi-process integration, scattered config, poor diagnosability (§1 of PRD). Transparency/scriptability goals (ARC-17-adjacent) unachievable | Rejected; INDI kept as HAL adapter (ADR-02) |
| **B. Single-node monolith** — everything on the field computer | Fails PRF-05/PRF-08 on a Pi/laptop; full-res GPU stacking impossible; couples capture reliability to processing load | Rejected; retained as degenerate deployment (STK-20) |
| **C. Two-node, three-service** (separate LLM-agent service) | Cleaner LLM isolation, but a third systemd unit and network hop for no fault-isolation gain — agent is already isolated by talking HTTP | Folded into field service (ADR-01, ADR-10) |
| **D. Two all-Python modular monoliths** (selected in ADD v1.0.0) | Right topology, but the field node carries a full Python runtime on a Pi: interpreter memory, GIL contention between serial polling and decode work, multi-day process robustness concerns (PRD risk table) | Superseded by E in v1.1.0 |
| **E. Rust backbone + Python workers** (rifflab pattern; selected) | Same topology as D with the field node as one static Rust binary and Python confined to crash-isolated GPU/ML workers on the stack; meets latency split, degraded-mode matrix (§5.4.4), and field simplicity with the smallest field-side footprint | **Selected** |

## 9. Quality Attribute Realization

### 9.1 Performance

| Budget (PRD) | Architectural mechanism |
|--------------|------------------------|
| PRF-01 position ≤ 200ms | 1 Hz+ serial poll task → event bus → WS push; no polling from browser |
| PRF-03 guide loop ≤ 500ms (Ph.3) | Entire loop inside field process; guide pulses via serial priority-adjacent lane; never crosses VPN |
| PRF-05 field ≤ 512 MB steady | Rust backbone, no Python runtime on the field node; decode/detect on a bounded blocking pool; no full-res pipelines on field node (IPP-02) |
| PRF-08 stack ≤ 3s/frame | GPU worker, pinned buffers, registration+accumulate on device (CMP-01/02) |
| PRF-12 e-stop ≤ 500ms | Dedicated route → priority serial lane (§5.4.3); local watchdog path independent of operator. **Three nested budgets, distinct measurement points:** (a) API handler → bytes on the wire ≤ 20 ms, verified in CI against a mock port (SDD T-SER-3); (b) API call → mount motion ceases on real hardware ≤ 100 ms, which adds 9600-baud transmission of the stop frames plus motor response (SDD T-HIL-1 step 3); (c) PRF-12's ≤ 500 ms from the operator's tap, which adds network RTT and is only meaningful on links with RTT ≤ 150 ms. Quote the letter, never a bare number |
| PRF-13 rebuild ≤ 3s/frame | Shadow-accumulator rebuild on GPU; post-chain step cache for sub-second parameter tweaks |

### 9.2 Reliability

Write-ahead ordering is the invariant: **frame to disk (fsync) → metadata → queue → everything else** (REL-05). All queues and FSM state are on disk and idempotently resumable (REL-04, REL-06). Ack-based reclaim (REL-13) is the only path that ever frees a field-node frame. Watchdogs (serial heartbeat, USB presence, clock-sync, disk thresholds) publish alerts on the event bus and trigger Safety Monitor actions.

### 9.3 Safety

Layered independently of the operator link: (1) protocol-level e-stop priority lane; (2) limit enforcement below the API (ADR-11); (3) local watchdog triggers; (4) LLM Blocked tier for e-stop plus server-side confirmation for medium/high (SEC-03). Open-loop mount drift (no encoders) is mitigated by solve-verify-sync in the control pipeline (PLS-05, MNT-10).

### 9.4 Security

VPN as trust boundary (SEC-01) + per-node token (SEC-02) + tier confirmation tokens (SEC-03) + env-var secrets (SEC-04). The LLM agent holds the same token as the UI and gains nothing by prompt injection that the tier system doesn't gate — confirmation is validated server-side against operator-issued tokens, not agent claims.

## 10. Architecture Evaluation and Open Risks

Residual architectural risks to be verified during Design Definition / early implementation:

| Risk | Verification plan |
|------|-------------------|
| ~~gphoto2 crate coverage gaps for the R10 (CR3 download, bulb, live-view stream)~~ — **RETIRED 2026-07-29** | Verification plan executed ahead of schedule, on the real body: bulb, CR3 download, settings, and a 58.5 fps live-view stream all covered by the bindings. No CLI fallback and no custom FFI needed. Evidence: `spikes/gphoto2-r10/FINDINGS.md` |
| erfa-based coordinate code diverges from astropy reference behavior | CI parity suite: fixed set of (time, site, target) cases computed by astropy, asserted against `astroctl-planning` to sub-arcsecond agreement. **The suite's value depends on binding liberfa itself** (`erfars`/`erfa-sys`) — astropy wraps that same C library, so parity then tests *our usage*. Against a pure-Rust reimplementation (the crate confusingly named `erfa`) the same suite would instead be testing a third party's port, which is a different and weaker guarantee. See PRD §7 |
| Worker IPC protocol drift or worker crash loops on the stack | Protocol version handshake at worker start; supervised restart with backoff; accumulator state persisted so a restart replays, not restarts, the stack |
| Serial two-lane queue starvation under heavy polling | Bench with simulator; e-stop injection test asserting budget (a) of §9.1 — ≤ 20 ms handler-to-wire under 50 cmd/s normal load (SDD T-SER-3) |
| Rebuild swap consistency (frames arriving mid-swap) | Property test on Rebuild Manager queue-drain protocol |
| Proxy WS fan-out (preview via field node) adds latency on slow field hardware | Measure on Pi; enable direct browser↔stack path if needed (ADR-07 fallback) |
| SQLite contention between ingest and rebuild on stack | WAL mode; single-writer discipline per DB |

## 11. Requirements Traceability

Architecture-level requirements (ARC-*) to elements; functional groups summarized (full per-ID matrix to be maintained alongside the design):

| PRD | Realized by |
|-----|-------------|
| ARC-01, ARC-03 | ADR-03; §5.4 process models |
| ARC-02, ARC-14 | PWA in field service (ADR-12, §5.6) |
| ARC-04, EXT-01/02 | HAL + dependency rule 1 (§5.6) |
| ARC-05 | Config loader, one YAML per node (§5.5) |
| ARC-06, ARC-09, ARC-21 | Degraded-mode matrix (§5.4.4) |
| ARC-07, EXT-05 | Event bus → WS hub (§5.2.1) |
| ARC-08, ARC-13, ARC-15 | Deployment view (§5.5) |
| ARC-10, EXT-03 | Contract-first API (§6.1) |
| ARC-11 | Transfer Agent + ADR-05/06 |
| ARC-12 | Calibration Library component (§5.2.2) |
| ARC-16 | Three pipelines split across components (§5.2, §5.3.2) |
| ARC-17, REL-11 | Frame Store write-once + Session Manager (§5.3.1) |
| ARC-18, ARC-19 | ML as post-chain steps in stack service (§5.2.2, §6.2) |
| ARC-20 | LLM agent as API client (ADR-10) |
| ARC-22, ARC-23, CMP-* | Python compute workers + supervision (ADR-09, ADR-13) |
| HAL-*, MNT-*, CAM-* | HAL registry, drivers, Safety Monitor (§5.2.1) |
| SES-*, PLS-* | Orchestrator + Control Pipeline (§5.2.1) |
| GDE-* | Guiding Service (§5.2.1), `astroctl-guiding` (§5.6); loop closes on the field node per §9.1/PRF-03 |
| EXT-04 | Sequence definitions are data, not code: the orchestrator's sequence model is a serde type persisted as YAML/JSON in `session.json: sequence_state` and importable/exportable through `/api/session` (§6.1) — no sequence logic is expressible only in Rust |
| EXT-06 | PWA structured for Capacitor wrapping (ADR-12) |
| STK-*, PPR-*, IPP-* | Processed Pipeline, Rebuild Manager, Preview/Export (§5.2.2) |
| CAL-* | Calibration Library (§5.2.2) |
| MLR-* | ML Runtime + step contract (§6.2) |
| LLM-* | LLM Agent Service + Confirmation Service (§5.2.1) |
| SEC-* | Auth middleware, Confirmation Service, deployment binding (§5.5, §9.4) |
| REL-* | §9.2; Transfer Agent; watchdogs |
| PRF-* | §9.1 |
| USB-*, PLN-* | PWA + Planning Service |

---

*Next document in the 12207 chain: Design Definition (ASTROCTL-SDD-001) — detailed design of each element defined here, starting with the Phase 1 elements: HAL + drivers, Safety Monitor, API gateway, live view pipeline, Frame Store.*
