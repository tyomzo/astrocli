# AstroCtl — Software Design Description

**Document ID:** ASTROCTL-SDD-001
**Version:** 1.19.0
**Author:** Artiom
**Date:** 2026-07-29
**Status:** Draft
**Conformance:** ISO/IEC/IEEE 12207:2017 (Design Definition process, §6.4.5); description conventions informed by IEEE 1016
**Governing documents:** ASTROCTL-PRD-001 v1.18.0 (requirements), ASTROCTL-ADD-001 v1.5.0 (architecture)
**Change note (1.1.1):** Governing pins advanced. §5.7 no longer names libraw as the RAW decoder — selection moved to the M2-T01 spike (PRD §7).
**Change note (1.1.2):** Pins advanced to PRD v1.8.0 / ADD v1.2.2. The §5.7 decoder is now `rawler`, selected on build evidence; M2-T01 validates its timing and memory rather than choosing.
**Change note (1.0.1):** Manual slew redesigned as a TTL-based dead-man's switch (§5.8.1, §5.4, T-SLW-1) — a lost link or stuck touch can no longer sustain motion.
**Change note (1.0.2):** Remote-link latency mitigations consolidated (§8.3): command staleness rejection, control/bulk connection separation (dedicated live-view socket), transfer pacing rule for Phase 2b, predictive position display and link-health surfacing in the PWA.
**Change note (1.1.0):** Design added for the two-node walking skeleton that ASTROCTL-IMP-001 delivers in M1, which v1.0.x deferred to the Phase 2b increment and therefore left unspecified: transfer agent (§5.10), stack ingest and session mirror (§5.11), worker IPC and supervision (§5.12). Increment table re-scoped accordingly (§1.2). `transfer.acked` and `stack.status` added to the closed topic enum (§4.3); the field node now carries one SQLite database from M1 (§6). `guide_pulse` regains the `rate` parameter PRD §4.1 specifies (§5.1). New verification entries T-XFER-1, T-ING-1, T-IPC-1 (§9).

**Change note (1.1.3):** §5.3.2/§5.3.3 corrected from the R10 spike: unlink stale tmp files before download (libgphoto2 will not overwrite), the USB transfer happens inside the capture call rather than the download, frames measure 32 MB, and the CLI-fallback table is empty for the reference camera.

**Change note (1.2.0):** Thread-isolation gaps closed. **T-ISO-1 added (§9)** — PRF-04 ("image download must not block mount tracking or UI responsiveness") was designed for but never verified, and now has a dedicated regression test rather than being inferred from the topology (§10). §5.3.1 documents the measured live-view/capture contention on the single gphoto2 context and how the UI must surface it; §5.7 and §5.9 follow through. §7 specifies explicit tokio runtime sizing per node. §2 makes "no blocking on the runtime" enforceable via clippy gates rather than convention alone.

**Change note (1.2.1):** §5.2.4 serial timings replaced with measurements from a real HEQ5 — round trip 14.4–16.6 ms, so the in-flight-normal-request assumption behind the e-stop priority lane is a third of what was budgeted.

**Change note (1.3.0):** §5.2.2 rewritten from a read-only hardware survey (`spikes/skywatcher-heq5/FINDINGS.md`): the opcode case convention stated as the safety boundary, seven undocumented inquiries recorded, real `!` error codes captured, and the mount's failure to validate the axis digit promoted to a codec correctness requirement. §5.2.4 gains hardware-validated timings — 2000 clean exchanges confirm the 3-miss heartbeat threshold, and back-to-back frames are shown to corrupt the reply stream, which is why single request-response is required rather than merely tidy.

**Change note (1.4.0):** §5.2.2 action-opcode encodings derived from vendor and reference-implementation sources (`spikes/skywatcher-heq5/ENCODINGS.md`) and two design errors corrected: the goto target is a **relative increment (`H`)**, not an absolute position (`S`, which the protocol does not have), and **`M` set-break-point-increment was missing entirely** from the command table despite being part of every goto. The `G` motion-mode bit layout and the `f` status decoding are now specified, the latter validated against our own hardware capture. §5.2.3's goto description updated for relative targeting.

**Change note (1.5.0):** §5.2.2/§5.2.3/§5.2.4 updated from executed motion experiments: goto ignores the step period (so no goto-speed calculation should be built around it), the goto tolerance is measurably generous, and `L` versus `K` is shown to be indistinguishable at low speed — stop overshoot is link latency scaling with rate, which is the real argument for the priority lane.

**Change note (1.5.1):** §5.2.2 goto-speed note corrected: the profile is a trapezoidal ramp, so goto duration is not linear in distance and must not be estimated by dividing counts by a nominal rate. M2-T02 gains the gvfs USB-claim detection observed on real hardware.

**Change note (1.6.0):** §5.2.2 gains **mandatory pre-motion readback verification** — hardware testing showed `:h`, `:m` and `:i` return the absolute goto target, absolute break point and step period exactly, so the driver can verify a goto is correctly programmed before sending `J`. Given the mount does not validate the axis digit, this is the only check that catches an encoding fault before the motors move. §5.2.3 goto tolerance confirmed ample against a measured error of 0 counts across six gotos.

**Change note (1.6.2):** §5.10.4 claimed the `/api/transfer/status` route and the `transfer.status` event carried "the same data", but listed a fifth field (`attempts_current`) the §4.3 topic does not have. Resolved in favour of §4.3 — the retry counter is REST-only diagnostic detail. Surfaced by M0-T03.
**Change note (1.7.0):** §4.1/§4.2 corrected while implementing them (M0-T02). Four of the five were holes in the very property these sections exist to guarantee. `AltAz` carried **public raw `f64` fields** in a section whose stated purpose is making unit bugs unrepresentable — alt/az are now `AltDegrees`/`AzDegrees` newtypes, named `alt`/`az` to match the §4.3 payload. The **derived `Deserialize`** on the validating newtypes was a bypass around their own constructors, and deserialization is how every coordinate this system acts on arrives — now `#[serde(try_from = "f64")]`. `Axis`/`Direction`/`SlewSpeed` **lacked serde derives** although §5.8.1's slew route deserializes all three by name. The §4.2 HTTP mapping was **silent on `DeviceError::Protocol` and `Busy`** (now 502 non-retryable and 409), and the "closed enum shared with the UI" was never enumerated anywhere — it is now a table, with `retryable` stated per code rather than only for the 502 pair. `CoreError`, referenced by §4.1 but defined nowhere, is now specified.

**Change note (1.6.1):** §3's crate graph drew `astroctl-safety → astroctl-drivers` as a compile-time dependency, contradicting ADD §5.6 rule 1 and its own §5.4, where `SafeMount` holds `Arc<dyn MountDevice>` — a HAL trait object, not a driver. Diagram corrected and the driver-naming rule stated explicitly.

**Change note (1.7.1):** §5.11.1 gains the `/api/system/info` row it was missing — §7 already required the resolved worker-thread count to be reported on *both* binaries, so a reader following §5.11.1 alone would have built a stack node whose runtime sizing could not be inspected. Surfaced by M0-T05.
**Change note (1.8.0):** §5.9 given a real frontend design ahead of M0-T06, since that task sets the pattern five M1 tasks inherit. Stack settled (Tailwind over semantic tokens, headless primitives only where accessibility needs them, Zustand). **Colour architecture decided at M0 although night mode is Phase 4** — tokens now, `data-mode` override, true black surfaces — because the mechanism is cheap to establish and expensive to retrofit. Night-mode image handling specified, which nothing previously covered: a stretched star field is greyscale-white and would destroy the dark adaptation the mode protects, so image surfaces get a red-channel filter with a per-panel true-colour toggle. Touch targets raised to 60–70 px for primary controls on the grounds that the operator may be gloved. Store discipline expanded: selector-based subscription, three-state telemetry, resnapshot on `Lagged`.

**Change note (1.8.1):** §4.5's startup refusal is narrowed from "not a loopback/VPN address" to loopback specifically. A VPN interface carries an ordinary private address and nothing distinguishes it from a LAN one, so the original wording was not implementable and invited a "10.0.0.0/8 is probably a tunnel" heuristic — guessing about precisely what SEC-01 exists to protect. Surfaced by M0-T05.

**Change note (1.9.0):** **WebSocket authentication designed** — a browser cannot send an `Authorization` header on a WS upgrade, which §4.5 previously required and nothing had noticed. Resolved with a single-use 30 s ticket from `POST /api/auth/ws-ticket`, so the long-lived token never enters a URL and therefore never reaches the field node's own access log. Written into §4.5, the §5.8.1 route table, the §5.9 connect flow, and M1-T03/M1-T04. Also §4.2 gains `NODE_UNREACHABLE`, `DISK_LOW` and `NOT_IMPLEMENTED`: the closed enum had no way to say "the other node is not answering", and borrowing `DEVICE_TRANSPORT` tells the operator to check a cable when the problem is a tunnel. Surfaced by M0-T05.

**Change note (1.10.0):** §5.9 given a layout, and it is not the obvious one. The subsystem decomposition — mount panel, camera panel, stack panel — mirrors the backend rather than the operator, whose sequence is pick a target, slew, frame, capture, watch. Manual mount control is not a destination but a brief step after a slew settles, so a permanent panel for it wastes space for most of a session. Layout now follows the session FSM. Three specifics: the D-pad **overlays the image** because nudging is framing and the control must share a field of view with its effect; the nudge affordance is contextual but always summonable, auto-expanding when a slew completes; and the target region is a **slot with a stable contract** holding manual RA/DEC entry in M1, into which Phase 2a's catalog drops without restructuring anything.

**Change note (1.11.0):** the accumulating stack is promoted from a status readout to a **primary view**. PRD §2 promises "immediate visual feedback as signal accumulates" — that is the payoff the two-node architecture exists for, and filing it under status was wrong. `FRAME` and `STACK` are now two sources sharing one image surface, switching to `STACK` when a sequence starts. The stack view is a slot like the target region: no knobs in M1 (the stub worker does no stacking), stats and the first controls in 2b, the post-chain in 2c. Also specifies the **rebuilding state**: IPP-16 re-stacks in the background while the preview keeps serving the pre-rebuild image, so a knob change correctly does nothing visible for a while and looks like a bug unless the panel says so.

**Change note (1.11.1):** the nudge control is **summoned, never automatic** — a badge in the bottom-right of the image surface that expands only on tap. Auto-expanding on slew completion contradicted the decision to overlay the image: it covers the frame the operator waited through the slew to see. The badge also signals availability before it is tapped, encoded **redundantly** rather than by colour alone, since night mode collapses hues toward red and a green/red badge becomes red/red.

**Change note (1.11.2):** §5.9's layout consolidated into three sketches — the three phone destinations, the summoned D-pad with its two badge states, and the tablet arrangement — replacing the partial ones accumulated while the design was being worked out. Added the four-slot table (target chooser, stack controls, rebuilding indicator, nudge availability) stating what M1 builds and what 2a/2b/2c fill, so the boundary between fixed layout and deferred content is explicit rather than inferred. M1-T04 and M1-T14 now point at it directly.

**Change note (1.12.0):** §5.9's target-platform paragraph named the three Android capabilities the PWA relies on without recording that all three require a **secure context**. Added, with the trap that makes it worth stating: every one of them works on `http://localhost` and none work on a phone over the VPN, so a capability check passing on the workstation says nothing about the field. Found by testing M0-T06 on a real phone after it reported the shell working. Also records that an expired certificate revokes the secure context as thoroughly as a missing one, which is why SEC-07 puts expiry in the health payload.

**Change note (1.13.0):** §5.8.1's field health response gains SEC-07's certificate expiry — `cert_expires_at`, `cert_days_remaining`, and a third `status` value `warn`. `warn` is derived when the response is built and never stored: it is a property of the clock rather than a lifecycle transition, so the `starting`→`ok` cell of §8.1 keeps exactly the two states it names, and a renewed certificate turns the warning off without anything having to reset it. TLS itself terminates in `astroctl-field` (ADD §4); the operator procedure is `docs/ops/tls-setup.md`. Landed by M0-T09.

**Change note (1.14.0):** §5.11 specified to the level M1-T11 can implement against, because the ingest contract has two sides and only one of them was written down. The `meta` part now has a schema (§5.11.1) rather than a parenthesized list of field names — it gained `v` and `ext`, the second because §5.11.1's ack format could not name the stored file without it and the first because the object is strict about unknown keys. The procedure (§5.11.2) records six ordering properties that were implicit and are each one bug away from data loss: the dedup key is `(session_id, frame_id)` and **not** `frame_id`, since §5.5's per-session counters make `light_00042` recur in every session; the temporary carries a nonce, since two overlapping retries otherwise write through a rename into a stored frame; the link is `renameat2(RENAME_NOREPLACE)`, since check-then-rename has a window a retry fits through and `EEXIST` is also the crash-recovery signal; the journal row follows the frame, since REL-13's authority may under-claim but never over-claim; a *definitive* answer is delivered only after the body is drained, since an early response is lost to a client still writing and the frame would then be retried forever; and the derived metadata files are outside the ack, since they are rebuildable and the frame is not. §5.11.3 pins `ingest.db` to the archive volume with `synchronous = FULL`, names the shared layout fixture, gives `session.json` a shape, states that the archive opens before the socket, and adds the startup sweep of leftover temporaries. Response bodies gain the `v` §2 requires of every externally visible schema. Landed by M1-T12.

**Change note (1.14.1):** §5.11.2's first ordering property described the wrong failure. A bare `frame_id` dedup key does not produce a false `duplicate: true` — the two frames' hashes differ, so the sha comparison separates them. It fires the **conflict** branch, `409 FRAME_ID_CONFLICT`, which is terminal: the sender stops retrying and the frame is lost. The fix (key on `(session_id, frame_id)`) was right; the justification was not, and a justification that names the wrong mechanism invites someone to "fix" it by comparing hashes more carefully instead of keying on the session.

**Change note (1.15.0):** §5.12 gains the worker-side obligations M1-T13 found by implementing it. The load-bearing one: **a worker must answer pings while it computes**. At the documented defaults a single-threaded worker computing for 15 s is SIGKILLed by its own liveness probe, so every real stacking job would crash-loop, and the config validator's `job_timeout > 3 × health_ping` bound reads as though it prevents this but does not. Also recorded: `result` is rejected at the decoder if it claims success and carries an error; frames are bounded at 1 MiB and a trailing partial frame is dropped, so a worker that loses framing cannot stream its heap into the backbone; unknown fields are ignored *because* a strict decoder would defeat §5.12.2's version-mismatch reporting; the retry backoff resets after a worker outlives the ceiling, or one bad night leaves it there permanently; writes to the child are deadline-bounded, since a full pipe would otherwise park the task holding the ping timer and let a wedge disable its own detector; worker stdout is redirected to stderr, because a stray `print` desynchronises the decoder and surfaces hours later as previews stopping; and the JPEG write is atomic. Found by building it, not by review.

**Change note (1.18.0):** §5.5 gains the seven ordering properties M1-T07 found by implementing it. §5.11.2 had already argued every one of the shared disciplines for the *receiving* side, and §5.5 — the origin of the same layout — said only "the same tmp-fsync-rename discipline", which is not enough to build a store that survives T-DUR-1. The load-bearing one: **the frame sequence is per session, not per kind**, because the sidecar drops the kind prefix and per-kind counters would alias `light_00042` and `dark_00042` onto one `quality_00042.json` — the second write silently describing the first frame. Also recorded: persist-then-grant is a deliberate choice of burning an id over reusing one; the frame is durable before its metadata exists, so a listing must show a frame with `quality: null` rather than hide it; `CURRENT` is relative and rename-swapped; an unparseable manifest rebuilds its counter from the frames on disk rather than from zero; a *failed* commit keeps its temporary and a sidecar failure is returned to the caller, both being the origin's inversion of §5.11.2's discard rule, since nothing can re-send these bytes; and free space that cannot be determined is not a REL-12 refusal. Landed by M1-T07.

**Change note (1.17.0):** the walking skeleton's first vertical slice, and three schema corrections it forced. **§4.3 `mount.position` makes `alt`/`az` nullable.** The mount facade publishes this event and the topocentric transform belongs to the safety monitor (§5.4) that *wraps* it, so between M1-T03 and M1-T05 the honest value is "not computed here" — and the stand-in a non-nullable field would have required is `0.0`, which is the horizon, which is exactly where the altitude limit lives. **§4.3 `mount.status` gains `tracking_mode`.** The payload carried `tracking: bool` and could not say which rate was running, so M1-T04's `TrackingControl` correctly refused to highlight any rate rather than remember the last button pressed — a decision that would have been wrong after every reconnect, every driver that refuses a rate as `Unsupported`, and every goto that suspends tracking and resumes it. The mount is the only thing that knows its rate. `tracking` is kept beside it: it is what most of the UI binds to and removing it would break a shipped client for no gain. **§4.2 gains `ABORTED`** (409, not retryable) and `DeviceError` gains the matching variant; the enum's review-checkpoint count moves 24 → 25. M1-T02 reported that a goto interrupted by an emergency stop had to return `Rejected`, which maps to `DEVICE_REJECTED`/422 — telling the operator their *request* was malformed at the moment their e-stop worked. It is not retryable because re-issuing the goto drives the mount back into whatever stopped it. Also settled, because §5.8.3 leaves them undefined and the PWA already depends on them: **control frames carry `type` where events carry `topic`** (an `Event` is a frozen serialization with no room for an envelope, so the snapshot and the pong cannot be events), **`ping` is an application-level message** (the browser `WebSocket` API can neither send nor observe an RFC 6455 ping, so a protocol-level answer is one no browser could ask for), and **the default subscription is every topic** (`subscribe` narrows; a hub requiring one would deliver nothing to a client that sends none). All three were proposed by M1-T04's mock and adopted unchanged. Landed by M1-T03.

**Change note (1.16.0):** three decisions taken while their cost was still low, all provoked by M1-T12/T13 findings. §4.2 gains the worker vocabulary — `CANCELLED`, `WORKER_UNAVAILABLE`, `WORKER_CRASHED`, `WORKER_TIMEOUT` — because every supervisor failure was about to collapse onto `INTERNAL`, and the PWA that switches on these codes is still one screen, so extending the frozen contract now costs a table edit rather than a migration; the enum's review-checkpoint count moves 20 → 24. `WorkerState` gains `Stopped` (its `Default`): workers spawn on demand, so "no worker running, none needed" is the stack node's normal idle state, and carrying it as `Option::None` collided with `StackStatus`'s use of `null` for "stack unreachable" — two different absences sharing one spelling. §5.11.1 gains the `HEAD /api/ingest/{session_id}/{frame_id}` pre-flight, because M1-T12 proved the POST cannot answer a duplicate cheaply over HTTP, so M1-T11 is built with the pre-flight from the start rather than retrofitted. §5.10.1's queue table keys on `(session_id, frame_id)`, inheriting §5.11.2's dedup correction before M1-T11 implements the schema with the defect in it.

**Change note (1.19.0):** two reconciliations from M1-T08 building §5.3.2 against the real T06/T07 handoffs. **The capture flow's ownership rule is broken at the driver boundary and the workaround is recorded:** §5.3.2 and `StagedFrame` both want the driver writing *into* the store's staged temporary, but HAL-03's `CaptureRequest{dir, stem}` has the driver append its own extension and manage its own temporary — there is no way to hand a driver the staged file. M1 captures into a session-owned scratch directory and renames onto the staged path, which preserves every durability property (the driver fsyncs the bytes before `capture` returns; `commit_frame`'s directory fsync makes the name durable). The honest fix is `CaptureRequest` carrying a full destination path, and it belongs to M2 when the gphoto2 driver defines what a real camera can accept. **`frame.saved` fires at `commit_frame`, not after the metadata write** — §5.3.2's ordering contradicted §5.5 note 3 and §4.3, which both say the frame is durable at commit; the sidecar is rebuildable and the frame is not, so the event announces the thing that cannot be re-made. §5.3.2's sequence is corrected to match. Also noted: `CaptureState` is exposing → downloading → saved; the task file's `saving` stage never existed in §4.3 and the exposure→download transition is inferred (one await, no progress callback) until the real driver can observe it.

**Change note (1.20.0):** §5.8.1's staleness paragraph gains the six things M1-T10 had to decide before it could be implemented, and one correction. The correction first: the paragraph lists **"tracking off" among the stopping commands that are never staleness-rejected, but gives it no route** — it is a `mode` on `/api/mount/tracking`, so a per-route classification cannot express it and a per-*body* one would put a safety property in a field the client fills in. `/api/mount/tracking` is therefore `motion_initiating` whole, which over-refuses exactly one command whose refusal leaves the mount tracking — the state §7 already calls the safe one — while every stop that must never be refused (`slew/stop`, `capture/abort`, `estop`) has a route of its own. The five gaps: the envelope **travels in headers**, because §5.8.1's own route table writes the bodies without it and half the mutation surface (`park`, `unpark`, `capture/abort`, `fault/ack`, both live-view controls) declares no body extractor at all — putting it in the body would make a JSON body mandatory on `/api/mount/slew/stop`, reintroducing on a *stop* the parse-failure path §5.8.2 exists to remove; **only 2xx outcomes are replayable**, since a cached `502` would turn one bad cable moment into a permanently refused command and a cached `409 BUSY` would refuse a goto for five minutes after the mount went idle; **a concurrent duplicate is `409`, not a second execution**, held by a reservation that a dropped client releases, because the flaky tunnel this mechanism is for is exactly what cancels a handler mid-flight; **`/api/auth/ws-ticket` takes no envelope**, since replaying a nonce hands the second caller a ticket the first one spent; and **a future `issued_at` is skew, not staleness** — only "older than" refuses, or a fast clock would be unrecoverable. §8.3(4)'s "retries are idempotent" is bounded at 1024 entries, 5 min, 8 KiB per body. §8.2's `RouteMeta` gains `CommandClass` beside `tier`: a second axis, not a derivation, because `slew/stop` is `low`/`Stopping` and `park` is `high`/`MotionInitiating`. Landed by M1-T10.

---

## 1. Introduction

### 1.1 Purpose

This document is the output of the Design Definition process for AstroCtl. It refines the architectural elements of ASTROCTL-ADD-001 into implementable design: Rust types and trait signatures, protocol encodings, state machines, task/channel topology, API schemas, storage formats, and test design. Where the ADD says *what* an element is responsible for, this document says *how* it is built.

### 1.2 Scope and increment plan

Per 12207's iterative application, this SDD is delivered in increments. **The current increment provides full design for everything the implementation plan delivers in M0–M3** (ASTROCTL-IMP-001 §2) plus the cross-phase foundations that must be stable from the first commit (type system, error model, event schema, concurrency topology, config).

The increment boundary follows the *implementation plan*, not the PRD phase list. IMP §1 deliberately pulls a skeleton of the two-node orchestration — transfer, ingest, worker IPC — into M1 to de-risk it early, so that skeleton is designed here (§5.10–5.12) rather than deferred. What remains deferred is the compute those elements will eventually carry, not the elements themselves.

| SDD increment | Scope | Sections |
|---------------|-------|----------|
| **v1.2.0 (this increment)** | Foundations + everything in IMP M0–M3: core types, HAL, Skywatcher driver, gPhoto2 driver, simulators, safety monitor, frame store, live view pipeline, field API gateway, **transfer agent, stack ingest + session mirror, worker IPC and supervision**, config, testing | all |
| v1.3.x (Phase 2a) | Session FSM detail, control pipeline, solver adapters, planning (erfa), slew limits detail | §5.6 expand, new sections |
| v1.4.x (Phase 2b) | Real stacking compute inside the worker, calibration library, accumulator design, transfer hardening (pacing §8.3.7, reclaim policy) | expand §5.10–5.12, new sections |
| v1.5.x (Phase 2c) | Post-chain executor, rebuild manager, LLM agent, confirmation service | new sections |
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
- **Async:** tokio multi-threaded runtime, **explicitly sized** (§7) rather than left at the default of one worker per core. No blocking calls on runtime threads; blocking C-library work goes to dedicated OS threads (camera) or `spawn_blocking`/rayon (decode, detection). Trait methods are async via `async_trait` until native async-in-traits covers dyn dispatch needs. "No blocking on the runtime" is a convention, and conventions decay — CI denies `clippy::await_holding_lock` and `clippy::await_holding_refcell_ref`, and T-ISO-1 (§9) is the behavioural backstop for everything a lint cannot see.
- **Errors:** `thiserror` enums per crate; no `anyhow` in library crates (binaries may use it at the top level). Every error carries enough context to render an operator-facing message (PRD §2 "transparency" principle).
- **Serialization:** `serde` throughout; all externally visible JSON schemas (API, events, IPC, metadata files) carry a `v` version field.
- **IDs:** session IDs `YYYY-MM-DD_<target-slug>`; frame IDs zero-padded sequence numbers per session (`light_00042`); job IDs monotonically increasing u64 per process run.
- **Time:** all persisted timestamps are UTC RFC 3339 with milliseconds. Local time appears only in UI rendering.
- **Units:** RA in hours, DEC in degrees, alt/az in degrees, exposure in seconds, temperatures in °C — carried in newtypes (§4.1) to make unit bugs unrepresentable.
- **Logging:** `tracing` with structured fields; every log line that corresponds to an operator-visible event also goes through the event bus (single source of truth, SES-07).

---

## 3. Design Overview

M0–M3 deliver **two** binaries. The crates below are the subset of ADD §5.6 that carries code in
these milestones; the rest (`solver`, `planning`, `guiding`, `llm`) are scaffolded empty at M0 and
filled in later phases. ADD §5.6 remains the authoritative full layout and dependency matrix.
Arrows are **compile-time** dependencies; everything also depends on `astroctl-core`:

```
        astroctl-field (bin)                       astroctl-stack (bin)
       /   /      |      |    \                      |       |       \
  safety pipeline session transfer  drivers*        ipc   (drivers*) (spawns)
     |     |       |       |          |              |                  |
     └─────┴───────┴───────┴──────────┤              |                  ▼
                                      ▼              |    workers/compute_worker.py
                              astroctl-hal           |         (Python child)
                                      |              |
                              astroctl-core ◄────────┘   (shared by both binaries)
```

`*` **Only the two binaries may name concrete drivers.** `astroctl-safety` wraps
`Arc<dyn MountDevice>` (§5.4) — a HAL trait object, never a concrete driver — and the same holds
for every crate above the HAL. The `DriverRegistry` is itself a HAL type (§5.1), so
`astroctl-hal` cannot depend on `astroctl-drivers` without a cycle; the deployable that assembles
the system is what supplies the concrete driver set. This is ADD §5.6 rule 1, and
`scripts/check-deps.sh` enforces it.

`astroctl-field` and `astroctl-stack` never depend on each other (ADD §5.6 rule 5); they share
`astroctl-core` for types and events, `astroctl-ipc` for the worker protocol definitions, and the
HTTP contract of §5.11.1.

Runtime task topology — field node:

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
      ├──► transfer agent ──► transfer.db ──► HTTP ──► stack /api/ingest   (§5.10)
      ├──► WS hub ◄── event bus (tokio::sync::broadcast)
      └──► watchdog task (serial heartbeat, USB presence, disk, clock)
```

Runtime task topology — stacking server:

```
 axum server task ──► ingest handler ──► verify+fsync ──► session mirror ──► ingest.db  (§5.11)
      │                                                          │
      │                                                    submit preview job
      ├──► worker supervisor ──► stdio IPC ──► compute_worker.py (child process)  (§5.12)
      │            ▲ ping/restart                     │
      ├──► preview WS (/ws/preview, binary) ◄─────────┘
      └──► watchdog task (disk thresholds, worker health)
```

---

## 4. Foundation Design (`astroctl-core`)

### 4.1 Domain types

```rust
/// Right ascension in hours [0, 24). Constructor normalizes.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, PartialOrd)]
#[serde(try_from = "f64")]
pub struct RaHours(f64);

/// Declination in degrees [-90, +90]. Constructor validates.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, PartialOrd)]
#[serde(try_from = "f64")]
pub struct DecDegrees(f64);

/// Altitude in degrees [-90, +90] (validates) and azimuth in degrees [0, 360)
/// (normalizes), north through east. Same shape as the two above.
pub struct AltDegrees(f64);
pub struct AzDegrees(f64);

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RaDec { pub ra: RaHours, pub dec: DecDegrees }

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AltAz { pub alt: AltDegrees, pub az: AzDegrees }

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TrackingMode { Sidereal, Lunar, Solar }

// serde too — §5.8.1's `/api/mount/slew` body deserializes all three by name.
pub enum Axis { Ra, Dec }
pub enum Direction { North, South, East, West }
pub enum SlewSpeed { Guide, Slow, Medium, Fast, Max }

/// Which side of the pier the tube is on; derived from the DEC counter (§5.2.3),
/// carried in `mount.position` (§4.3).
pub enum PierSide { East, West }

/// Guide-pulse rate as a fraction of sidereal, (0.0, 1.0]. Constructor validates. (MNT-12)
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, PartialOrd)]
#[serde(try_from = "f64")]
pub struct GuideRate(f64);
```

Newtype constructors (`RaHours::new`, `DecDegrees::new`, …) are the only way to build coordinate values; out-of-range input is a `CoreError::InvalidCoordinate`, never a wrapped/clamped silent fix. Non-finite input is rejected everywhere, normalizing types included — `NaN` is not a direction.

**No public `f64` field carries a coordinate**, which is why alt/az are newtypes rather than the bare `alt_deg`/`az_deg` pair this section carried through v1.6.1: the API and the safety monitor both handle horizontal coordinates, and a design that makes RA/DEC unit-safe while leaving alt/az as loose floats protects the half that is not used by the altitude limit. Field names are `alt`/`az`, matching the `mount.position` payload of §4.3; the unit lives in the type, so the suffix was redundant anyway.

**Deserialization goes through the constructors** (`#[serde(try_from = "f64")]`). A derived `Deserialize` would accept `{"dec": 91.0}` off the wire and hand it to the mount, which is the exact failure the newtypes exist to prevent — and every coordinate this system acts on arrives by deserialization.

`CoreError` is this crate's own error type: `InvalidCoordinate { quantity, value, expected }`, rendering as `invalid declination: 91 is not within [-90, +90] degrees`. It maps to `VALIDATION` (422) at the API boundary (§4.2).

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
    #[error("aborted: {0}")]                  Aborted(String),     // e-stop/limit took the axes
}
```

API error envelope (every non-2xx response):

```json
{ "v": 1, "code": "MOUNT_TIMEOUT", "message": "mount did not respond within 2.0s",
  "detail": {"axis": "ra", "command": "j"}, "retryable": true }
```

HTTP mapping: `NotConnected`/`Unsupported` → 409; `Timeout`/`Transport` → 502 (device side, `retryable: true`); `Rejected`/validation → 422; safety-limit rejection → 403 with `code: "LIMIT_ALTITUDE"`; auth failure → 401. Codes are a closed enum shared with the UI.

`DeviceError::Protocol` and `DeviceError::Busy` are not in that sentence and need to be: an unparseable reply is an upstream-device failure like a timeout (**502**, but *not* retryable — it repeats deterministically until the driver or the firmware changes), and "busy" is the same class of answer as `NotConnected` — the device is healthy, its state is wrong (**409**).

The closed enum, with the status and default retryability of each code. `ErrorCode::http_status()` and `ErrorCode::retryable()` are total functions over it, so a new code cannot be added without deciding both:

| Code | Status | Retryable | Raised by |
|------|--------|-----------|-----------|
| `NOT_CONNECTED`, `UNSUPPORTED`, `BUSY` | 409 | no | device state (§5.1), orchestrator FSM (§5.6) |
| `ABORTED` | 409 | no | `DeviceError::Aborted` — a motion that had *started* was stopped by an e-stop, a safety limit or an operator stop. The 409 twin of `CANCELLED`, and separate from it because `CANCELLED` is a stacking job and this is the telescope; rendering worker copy for an e-stop sends the operator to the wrong place. Never retryable: the request was valid, but re-issuing it drives the mount back into whatever stopped it |
| `MOUNT_TIMEOUT`, `CAMERA_TIMEOUT`, `DEVICE_TIMEOUT` | 502 | **yes** | `DeviceError::Timeout`, qualified by which device |
| `DEVICE_TRANSPORT` | 502 | **yes** | `DeviceError::Transport` |
| `DEVICE_PROTOCOL` | 502 | no | `DeviceError::Protocol` |
| `DEVICE_REJECTED` | 422 | no | `DeviceError::Rejected` |
| `VALIDATION` | 422 | no | request/coordinate validation (`CoreError`, §4.1) |
| `COMMAND_STALE` | 422 | no | staleness rejection (§5.8.1) |
| `CHECKSUM_MISMATCH` | 422 | no | ingest (§5.11.2) |
| `LIMIT_ALTITUDE`, `LIMIT_MERIDIAN` | 403 | no | safety monitor (§5.4, MNT-15/16) |
| `SLEW_TTL_EXPIRED` | 403 | no | dead-man's switch (§5.8.1) — also an `alert` code |
| `AUTH` | 401 | no | bearer middleware (§4.5) |
| `FRAME_ID_CONFLICT` | 409 | no | ingest (§5.11.2) |
| `DISK_FULL` | 507 | no | ingest below `disk_critical_free_gb` (§5.11.2, REL-12) |
| `DISK_LOW` | — | — | alert-only, no HTTP status: the warn threshold (REL-12). `alert.code` draws from this same enum |
| `NODE_UNREACHABLE` | 502 | yes | the *other node* is not answering — the `/stack/*` proxy (ADR-07) and the transfer agent (§5.10). Distinct from `DEVICE_TRANSPORT`, which means serial or USB to a **device**; conflating them tells the operator to check a cable when the problem is a tunnel |
| `NOT_IMPLEMENTED` | 501 | no | a route that exists in the design but not yet in this build — e.g. WS proxying before M1-T14. Better than borrowing `UNSUPPORTED`, which means the *device* cannot do it |
| `NOT_FOUND` | 404 | no | unknown session/frame |
| `CANCELLED` | 409 | no | the job was cancelled — by the operator or by shutdown — before it produced a result. Not a failure: the request was fine, the state moved under it |
| `WORKER_UNAVAILABLE` | 502 | yes | no worker can take the job — spawn failed or restarts are backing off (§5.12.3). Retryable because the supervisor is already fixing it |
| `WORKER_CRASHED` | 502 | yes | the worker died mid-job; the supervisor is restarting it. Distinct from `INTERNAL` because "the Python worker crashed and is being restarted" and "the stacking software has a bug" send the operator to different places |
| `WORKER_TIMEOUT` | 502 | no | the job exceeded `workers.job_timeout_seconds`. Not retryable: the same job hits the same ceiling deterministically, like `DEVICE_PROTOCOL` |
| `INTERNAL` | 500 | no | anything unhandled |

One code, one status: `DISK_FULL` is 507 wherever it is raised. The M1-T08 task file currently says a *field-node* capture refused below the critical threshold answers 409 — that must move to 507, or the two refusals need different codes. A UI that maps codes to messages cannot also be asked to interpret the same code differently per route.

`DeviceError` says *what* failed but not *which device*, while the UI needs to tell the operator that the mount stopped answering — hence the device-qualified timeout codes and the `ErrorCode::from_device_error(DeviceKind, &DeviceError)` mapping rather than a bare `From` impl. The status is a `u16`, not a framework type: `astroctl-core` sits below the API layer and must not pull axum into every crate (ADD §5.6).

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
| `mount.position` | `{ra, dec, alt, az, pier_side}` — `alt`/`az` are **nullable**: the facade that publishes this does not compute the topocentric transform, the safety monitor wrapping it does (§5.4, M1-T05). `null` rather than `0.0`, which is the horizon and therefore the one value that would corrupt the altitude limit | 1 Hz (MNT-02) |
| `mount.status` | `{state, tracking, tracking_mode, slewing, parked}` — `tracking_mode` is the rate (`sidereal`\|`lunar`\|`solar`) or `null` when the drive is off, and `tracking` is derived from it so the two cannot contradict. The rate is carried because only the mount knows it: a driver may refuse one as `Unsupported`, and a goto suspends tracking and resumes whatever was running before, so a UI deriving it from the last accepted command guesses wrong exactly when it is checked | on change |
| `camera.status` | `{connected, battery_pct, charging, storage_free_mb}` | on change + 60s |
| `capture.progress` | `{frame_id, state: exposing\|downloading\|saved\|preview_ready, elapsed_s}` | on change |
| `frame.saved` | `{frame_id, path, size_bytes, sha256}` | per frame |
| `liveview.frame` | binary WS frame (JPEG), not JSON — carried on the dedicated `/ws/liveview` socket, never on `/ws` (§8.3) | ≤ camera rate |
| `transfer.acked` | `{frame_id, sha256, acked_at, queue_depth}` — the stack node has the frame and verified it; the only event that makes a field-node frame reclaim-eligible (REL-13) | per frame |
| `transfer.status` | `{state: idle\|uploading\|offline, queue_depth, oldest_queued_age_s, last_ack_ts}` | on change + 30s |
| `stack.status` | `{connected, session_frame_count, last_preview_ts, worker_state, restarts}` — republished by the field node from the stack's health so the PWA has one event source (USB-06) | on change + 30s |
| `alert` | `{severity, code, message}` | as needed |
| `system.health` | `{disk_free_gb, clock_synced, uptime_s}` | 60s |

The WS hub drops slow consumers (bounded per-client queue, close on overflow) rather than applying backpressure to the bus — a stalled phone must never stall capture.

### 4.4 Configuration

Config structs mirror PRD §8.1 exactly, `#[serde(deny_unknown_fields)]` on every level — a typo in the YAML is a startup error listing the offending key, not silent default behavior. Validation pass after parse: port existence deferred to connect, but ranges (baud, limits, thresholds) and cross-field rules (e.g. `mount.limits.min_altitude_degrees ∈ [0, 45]`) checked at load. The loaded, validated config is exposed as `Arc<FieldConfig>`; no component re-reads the file.

### 4.5 Auth (Phase 1 subset of SEC-02)

Bearer token middleware on every route including WS upgrade: constant-time comparison against the token from `auth_token_env`. Absent env var **and** `server.host` not a loopback address → startup refuses with an explanatory
error (SEC-01 enforcement at the earliest possible point). The test is *loopback specifically*, not
"loopback or VPN": a VPN interface carries an ordinary private address and nothing distinguishes it
from a LAN one, so any "10.0.0.0/8 is probably a tunnel" heuristic would be guessing about the
exact thing SEC-01 exists to protect. Binding a non-loopback address without a token is refused
regardless of which interface the operator believes it is. Confirmation-token machinery (SEC-03) is Phase 2c; the route metadata slot for tiers (§8.2) exists from Phase 1 so routes are annotated once.

**WebSocket authentication uses a short-lived ticket, because a browser cannot send a header on
the upgrade.** The `WebSocket` constructor accepts only a URL and a subprotocol list — there is no
way to attach `Authorization`. The bearer token is therefore never used directly on `/ws` or
`/ws/liveview`. Instead:

```
POST /api/auth/ws-ticket        Authorization: Bearer <token>
  → 200 { "ticket": "<opaque>", "expires_in": 30 }

GET  /ws?ticket=<opaque>        (upgrade; ticket validated and CONSUMED)
```

Rules that make this safe rather than a token-in-a-URL with extra steps:

- **Single use.** The ticket is removed from the store when the upgrade consumes it. A replayed
  ticket is rejected.
- **Short TTL** — 30 s, enough for a slow tunnel and modest clock skew, far too short for a
  logged value to be worth anything.
- **Cryptographically random**, at least 128 bits, generated by the server. Never derived from
  the bearer token.
- **Bounded store.** Outstanding tickets live in a small map with a hard cap and aggressive
  expiry, so a client that requests tickets and never connects cannot grow memory.
- **A fresh ticket per connection.** This app reconnects often on a flaky tunnel (REL-10), so the
  PWA fetches a ticket immediately before each `WebSocket` construction, including every retry.

The point is that the *long-lived* token never enters a URL, and therefore never reaches the
field node's own access log under `server.log_dir` — a file the operator may reasonably share
when asking for help. A ticket in that log is expired and spent before anyone reads it.

**This applies to browser clients only.** The field node connecting to the stack node's WebSocket
(M1-T14's preview proxy) is not a browser and uses the ordinary `Authorization` header; it has no
need of a ticket.

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
    /// `rate` is a fraction of the sidereal rate, per PRD §4.1 and MNT-12; the driver
    /// programs it (Synta `P`) before issuing the pulse. Devices without settable rates
    /// return `Unsupported` for any value other than their fixed one.
    async fn guide_pulse(&self, axis: Axis, dir: Direction,
                         duration_ms: u32, rate: GuideRate) -> Result<(), DeviceError>;
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
| `H` | Set goto target **increment** (relative counts) | goto |
| `M` | Set break-point increment (deceleration point) | goto |
| `I` | Set step period (speed) | tracking rates, slew speeds |
| `J` | Start motion | tracking, slew, goto |
| `K` | Stop motion (ramped) | stop_slew, stop_tracking |
| `L` | Instant stop | **emergency_stop only** |
| `P` | Set autoguide rate | guide_pulse setup |
| `g` | High-speed ratio | connect handshake — **2 hex chars, not a byte-swapped u24** |

`E` (set axis position), `O` (set switch) and `U` (set break steps) exist in the vendor protocol
but are unused by this design.

**`G` motion-mode encoding** — `:G<axis><mode><dir>\r`, confirmed against three independent
sources (`spikes/skywatcher-heq5/ENCODINGS.md`). Mode: `0` GOTO high-speed, `1` SLEW low-speed,
`2` GOTO low-speed, `3` SLEW high-speed. Direction: `0` forward, `1` backward. Note the packing is
counterintuitive — `0` is *high* speed and `1` is *low* — so a transposed digit is a 16× speed
error rather than a direction error. Never construct this byte from an unvalidated integer.

**Goto command sequence**: `G` (mode) → `I` (step period) → `H` (target increment) → `M`
(break-point increment) → **read back and verify** → `J` (start).

**Pre-motion verification is mandatory.** Before `J`, re-read `:h`, `:m` and `:i` and assert they
equal the intended absolute target, absolute break point and step period. Measured on hardware
(`spikes/skywatcher-heq5/FINDINGS.md`): `:h` returns position + the `H` increment exactly, `:m`
returns position + the `M` increment exactly, and `:i` returns the step period exactly. Three
round trips ≈ 48 ms. This catches a corrupted frame, a byte-swap error, a mis-encoded increment,
or a wrong axis digit — which matters because **the mount does not validate the axis digit itself**
— *before* the motors are ever commanded. On mismatch, abort and surface a protocol error; do not
send `J`. **`I` is sent for protocol completeness but does not
control goto speed** — measured on hardware, a 10× step-period change left the velocity profile
unchanged. Goto ramps trapezoidally toward a cruise of ~87,000 counts/s (835× sidereal); short
moves are ramp-limited and never reach it, so **goto duration is not linear in distance** and the
orchestrator must not estimate slew time by dividing counts by a nominal rate. Do not build a
goto-speed calculation around the step period; it governs SLEW and tracking only. The mount decelerates and stops itself at the target;
this self-terminating property is what makes a bounded goto the correct first motion during
bring-up (see `spikes/skywatcher-heq5/MOTION-PLAN.md`).

**`f` axis status decoding** — reply `=<n1><n2><n3>`; bit tests apply directly to the ASCII
characters, valid because status nibbles never exceed 7. `n1 & 0x01` slew mode (1 = SLEW,
0 = GOTO); `n1 & 0x02` direction (1 = backward); `n1 & 0x04` speed (1 = high); `n2 & 0x01`
running; `n3 & 0x01` initialised. Validated against our own capture: the mount returned `=100`
powered-but-uninitialised and at rest, which the decoder reproduces exactly. **This is the field
slew-complete detection (§5.2.3) depends on.**

**Opcode case is the safety boundary.** Lowercase opcodes are inquiries; uppercase are actions
(`F G S I J K L P`). Everything the driver sends before a deliberate motion decision must be
lowercase, and test harnesses that talk to real hardware should enforce this on the raw byte
stream rather than by convention — a misaligned frame must not be able to form an action opcode.

**The mount supports more inquiries than this table uses.** A read-only sweep of the real HEQ5
(`spikes/skywatcher-heq5/FINDINGS.md`) found thirteen supported lowercase opcodes:
`a b c d e f g h i j m r s`. The seven this design does not use — `c d h i m r s` — are recorded
with their at-rest values in the findings. `d`, `h` and `r` read at or near the home counter,
suggesting target/breakpoint registers; if the EQMOD source confirms that, they would let the
driver *read back* what a goto was programmed with before `J` is ever sent, which is a cheap
safety check worth adopting. Semantics require the EQMOD cross-reference (PRD §4.2); the survey
establishes only existence and at-rest values.

**Error frames** — `!` + single digit + `\r`, with three codes observed on real hardware:
`!0` unknown command, `!1` missing or invalid parameter, `!3` malformed frame.

**The mount does not validate the axis digit.** `:j9` returns a well-formed position response for
a nonexistent axis. The codec must therefore validate the axis before transmitting — the device
will not reject a corrupted digit, it will answer with plausible data. This is why the typed
command layer (`GetPosition(Axis)`, not a formatted string) is a correctness requirement rather
than an ergonomic preference.

#### 5.2.3 Position math

Per axis: `counts_home = 0x800000`. With CPR read at handshake:

```
ra_counts→hours:  ra_h  = ((counts - counts_home) / CPR) * 24.0   (mod 24, hemisphere-adjusted)
dec_counts→deg:   dec_d = ((counts - counts_home) / CPR) * 360.0
```

RA axis position is mechanical hour angle; conversion to/from RA requires LST — Phase 1 computes LST from system clock + site longitude (REL-14 warns when clock is unsynced; full erfa-based apparent-place pipeline arrives with `astroctl-planning` in Phase 2a, and this module keeps the conversion behind `fn mech_to_sky(&self, counts: AxisCounts, lst: Lst) -> RaDec` so the upgrade is internal). Pier-side handling: DEC counts beyond ±90° imply the flipped pier state; `pier_side` is derived, reported in `mount.position` events, and consumed by the meridian limit (§5.4).

Goto: the wire protocol takes a **relative increment** (`H`), not an absolute target — so the driver computes absolute target counts from target RaDec + LST + chosen pier side, then sends the delta from the current counter. Relative is also the safer primitive: an arithmetic slip yields a small wrong move rather than a slew across the sky. long slews use high-speed motion mode with the ramp handled by the motor controller; the driver polls `j`/`f` at 2 Hz during goto, declares completion when both axes report stopped within tolerance (default 10 counts; measured error is **0 counts across six gotos from 0.04° to 4°, both directions**, so the tolerance is ample and loose in the safe direction), then restores tracking if it was active (SES-06).

#### 5.2.4 Serial task and lanes

One tokio task owns the `serialport` handle exclusively.

```rust
enum SerialRequest { Normal(Cmd, oneshot::Sender<Result<Resp>>),
                     Priority(Cmd, oneshot::Sender<Result<Resp>>) }
// two mpsc channels; the task select!s with bias: priority drained first,
// in-flight normal request completes (single request-response — **never pipelined; two frames in one write provably corrupt the reply stream on real hardware** — **measured 14.4–17.2 ms** over 2000 exchanges on a real HEQ5 over an EQDIR stick, against the ≤ ~50 ms this design assumed, so e-stop's worst-case wait behind a normal command is a third of budget) but
// no new normal request starts while priority queue is non-empty.
```

Per-request timeout 500 ms (≈30× the measured 16.6 ms worst case — deliberately generous, not a guess), one retry on timeout/garbled response, then `DeviceError::Timeout` and a `mount.status` degradation event. Heartbeat: the 1 Hz position poll doubles as the heartbeat; 3 consecutive failures → watchdog fires (§5.4). **Threshold validated against hardware**: 2000 consecutive exchanges produced zero timeouts and zero malformed replies, with a 2.5 ms spread, so three consecutive misses is an unambiguous fault signal rather than noise-triggering. Framing resilience was also measured — the mount resynchronises on `:` after junk, and a truncated frame times out without wedging the link. Emergency stop = `Priority(L axis1)` + `Priority(L axis2)`. **Note what `L` does and does not buy:**
measured at low speed, `L` and `K` arrest motion identically (85 vs 84 counts of overshoot) because
at that rate the overshoot is one serial round trip of command latency, not deceleration. The
instant-stop advantage is real only at high slew rates where momentum exists. Stop overshoot
therefore scales with rate — the same 16 ms becomes ~1,370 counts at 817× sidereal — which is the
actual argument for the priority lane. measured budget from API handler to bytes-on-wire ≤ 20 ms (test T-SER-3, §9).

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

**One context means one queue: live view and capture contend, and cannot be made not to.** Every
`CamCmd` is serviced by the single thread in order, because there is exactly one `gphoto2::Context`
and libgphoto2 forbids sharing it. A second context is not an option. Measured on the R10
(`spikes/gphoto2-r10/FINDINGS.md`): live view sustains 58.5 fps, and `capture_image()` blocks the
thread for **2.08 s**. So every frame you take stalls the live-view stream for roughly two seconds.

This is a property of the hardware interface, not a defect, and the design does not try to hide
it — it surfaces it:

- The facade emits `capture.progress` transitions (`exposing` → `downloading` → `saved`) around
  the blocking region, so the UI always knows *why* the stream stopped.
- The live view pipeline (§5.7) treats a gap as expected during capture rather than as a stream
  fault, and does not attempt reconnection.
- The PWA (§5.9) renders the preview panel in a "capturing" state for the duration. An unexplained
  two-second freeze reads as a crash; a labelled one reads as the camera working.

What must *not* happen is this stall propagating anywhere else — mount polling, the event bus, the
API, and the WS hub are all off this thread by construction, and **T-ISO-1 (§9) exists to prove it
stays that way.**

Every command has an operation-class timeout (config get/set 5 s; capture = exposure + 30 s; download 120 s). A timed-out thread is considered wedged: the facade drops the channel, the thread is abandoned (it cannot be safely killed mid-libgphoto2-call), a fresh thread + context is spawned, and a USB reset is attempted — this is the REL-03 recovery path, surfaced as a `camera.status` reconnecting event.

#### 5.3.2 Capture flow (CAM-03/04, REL-05)

```
capture request → set format/ISO/shutter if changed → trigger
  → wait event CAPTURE_DONE (or bulb timer expiry → release)
  → unlink any stale .tmp_<id>.cr3   ← libgphoto2 refuses to overwrite (spike finding 1)
  → download to <session>/frames/.tmp_<id>.cr3
  → fsync file → rename to light_<id>.cr3 → fsync dir      ← frame is now durable
  → compute sha256 (blocking pool) → write frame meta JSON → emit frame.saved
  → hand path to live view pipeline (§5.7) and enqueue in the transfer agent (§5.10)
```

The rename-after-fsync makes a torn download invisible to every consumer (they only ever see completed frames).

**Two realities measured on the R10** (`spikes/gphoto2-r10/FINDINGS.md`) that the flow above must respect:
`download_to` returns `File exists` rather than truncating, so a crash leaving a stale `.tmp_` file
would make every retry fail — unlink first, unconditionally. And with `capturetarget=Internal RAM`
the USB transfer happens inside the capture call, not the download call: a full frame (**32 MB**
measured, not the ~25 MB the PRD once assumed) is resident inside libgphoto2 before the download
step begins. That is affordable against PRF-05's 512 MB but must be counted, and it means the
"streamed to disk" wording describes the disk write, not the wire transfer. Bulb: driven via the `eosremoterelease` PTP config — `Press Full`, hold, `Release Full`. **Verified on the R10**: a 10 s hold produced a camera-reported `BulbExposureTime 9` and a CR3 via the `NewFile` event. This was the highest-risk item in the plan (ADD §10) and is now closed.

#### 5.3.3 CLI fallback

`GPhoto2Cli` implements the same internal `CamOps` trait by shelling out to the `gphoto2` binary per operation (`--capture-image-and-download`, `--set-config`, `--wait-event`). The concrete driver is composed per-operation from a coverage table in config, so a binding gap on one operation doesn't force the whole driver onto the CLI. **For the R10 the table is empty** — the spike found every operation covered by the bindings, bulb included, so `camera.ops_via_cli: []`. This path exists for future bodies, not for the reference camera.

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

Watchdogs (one task, 1 Hz tick): serial heartbeat freshness; camera thread liveness; disk free vs. thresholds (REL-12: warn → pause-after-frame); clock sync via `adjtimex` state (REL-14 warning). Watchdog actions publish `alert` events and, for serial loss during motion, issue priority-lane stop — a mount slewing on a dead link is the one scenario where the watchdog acts autonomously (REL-02/03). The disk and clock half runs at the §4.3 `system.health` cadence of 60 s rather than 1 Hz, edge-triggered, and has since M0-T05: a `statvfs` sixty times a minute answers a question whose thresholds are in gigabytes. The 1 Hz figure belongs to the hardware watchdogs that arrive with their devices.

**Obligations found by implementing this (M1-T05).** The sketch above is accurate about *what* is enforced and silent about six things that decide whether it can be. Each of these was a defect until it was fixed, not a design preference:

1. **`Err(LimitViolation::Altitude)` needs a home in the frozen error enum.** `SafeMount` implements `MountDevice`, so it returns `DeviceError` — and §4.2's `DeviceError` had no limit variant, while `ErrorCode::LimitAltitude`/`LimitMeridian` had existed since M0 with a 403 mapping and **no producer**. The nearest existing variant, `Rejected`, maps to `DEVICE_REJECTED`/422: it would tell the operator the mount refused their command when the mount was never asked, and would drop the one fact they need — which limit. `DeviceError::LimitViolation { limit, detail }` is the producer; no driver may return it (drivers do no limit checking at all — §5.1).

2. **A limit cannot be enforced *only* inside `goto`.** §5.8.1 makes goto a `202 + WS progress` route: the handler spawns the slew, answers immediately and drops the future. So a refusal raised inside `SafeMount::goto` happens *after* the node has already answered `202 Accepted`, and MNT-15's "rejected with 403 `LIMIT_ALTITUDE`" arrives as an alert instead of as the answer to the request. The two sections are incompatible as written. The resolution is that the wrapper also exposes a synchronous, side-effect-free `check_goto`, which the route asks *before* answering — sharing the one implementation, so asking and enforcing cannot drift. Enforcement stays below the API; only the *answer* depends on the route asking.

3. **The meridian limit needs a pier side Phase 1 does not have.** §5.2.3 says `pier_side` is derived by the driver and "consumed by the meridian limit (§5.4)" — but no Phase 1 driver derives it, and `mount.position` reports `unknown`. Without it, a bare `hour_angle > limit` comparison stops tracking the instant an operator acquires any target in the western sky, which are targets the mount flipped to reach and is perfectly safe on. Phase 1 therefore fires on the **crossing**: the tube was on one side of the limit and is now on the other, under sidereal motion. That needs no pier side to be correct, and it accepts a real limitation — a mount that starts tracking already past the limit on the wrong side is not protected. When a driver reports pier side this becomes a comparison again, with the side deciding the sign.

4. **The slew TTL is not a `MountDevice` parameter.** §5.1's `slew(axis, dir, speed)` has no room for it, because the dead-man's switch lives above the HAL. A wrapper reached through `dyn MountDevice` therefore cannot be handed the operator's `ttl_ms`; it applies `slew_ttl_default_ms` and the API route calls an inherent `slew_for(..., ttl)` instead. Both paths are governed — the difference is only whose number sets the window.

5. **A manual slew has no target, so MNT-15's check is directional.** "Goto/slew targets below the limit are rejected" has an obvious positional reading for slew — refuse while below the limit — and that reading traps the mount underneath its own horizon limit with no way to drive it back up, which an operator meets at 2 a.m. after stopping a goto halfway. The check is on where the axis is *heading*, one degree along: descending below the limit is refused, climbing out of it is not.

6. **The wrapper's background watch holds an `EventBus` handle**, so it falls under the shutdown invariant of §7 step 5 that the M1-T03 follow-up made a hard rule — no sender may outlive shutdown, or the session log cannot flush. The watch is therefore stopped in `Drop` rather than by a `shutdown()` the binary must remember to call, which is how the first two such handles were missed.

### 5.5 Frame store & session layout (`astroctl-session`)

Directory layout exactly as PRD §5.9. Phase 1 writes:

```
sessions/CURRENT -> <session_id>        # relative symlink to the active session
sessions/<session_id>/session.json      # v, target?, equipment (from config), created_ts,
                                        #   frames_reserved, sequence_state (reserved)
                     frames/light_<id>.cr3
                     control/quality_<id>.json   # Phase 1: exposure params, sha256, size
```

`session.json` and per-frame metadata are written with the same tmp-fsync-rename discipline as frames. A `CURRENT` symlink identifies the active session. Disk monitor consults this store for REL-12. The store exposes `reserve_frame_id()` (atomic counter persisted in session.json on each grant) so a crash never reuses an ID (REL-04 groundwork).

**Ordering properties.** This is the origin of the layout §5.11.3 mirrors, so every discipline §5.11.2 argues for the receiving side binds here identically — nonce'd temporaries, `renameat2(RENAME_NOREPLACE)` for the frame link, a directory fsync after the rename, and a startup sweep of leftovers before anything may capture. What is specific to the origin:

1. **The frame sequence is per session, not per kind.** The sidecar is `quality_<id>.json` with the kind prefix dropped, so per-kind counters would put `light_00042` and `dark_00042` on one `quality_00042.json` — two frames, one metadata file, the second write silently describing the first frame. One counter per session makes that unrepresentable.
2. **Persist, then grant.** The counter reaches disk before `reserve_frame_id` returns. The failure this chooses is a *burned* id (granted, never used — a gap in the numbering, which costs nothing); the failure it refuses is a reused one, which lands a second exposure on a stored frame. Two fsyncs per frame against an exposure measured in seconds is not a cost worth optimising with block reservation.
3. **The frame is durable before its metadata exists.** `commit_frame` and the sidecar write are separate steps in that order (ADD §9.2). A crash between them leaves a frame with no sidecar, which the session listing reports as `quality: null` rather than hiding — a frame whose metadata write was interrupted is still a frame (REL-05). The other order would leave metadata for a frame that is not there, and the transfer agent acts on metadata.
4. **`CURRENT` is relative and is swapped atomically** (symlink to a temporary, then rename). Relative so the tree can be moved or mounted elsewhere without the link pointing off this machine; swapped through a rename so a reader never finds it missing.
5. **A `session.json` that cannot be parsed rebuilds its counter from the highest id in `frames/`**, never from zero. Refusing to open would leave the node unable to capture at all; restarting the counter would aim the next exposure at a stored frame.
6. **A failed commit keeps its temporary**, unlike §5.11.2's, and the sidecar write failure is returned rather than logged. The stack node discards freely because the sender still holds the frame; this node is the origin and nothing can re-send, and its sidecar holds exposure parameters that exist nowhere else.
7. **`begin_frame` refuses below `storage.disk_critical_free_gb`** with `DISK_FULL`, because a capture that starts below the threshold finishes as a truncated raw. Free space that cannot be *determined* is not a refusal — losing frames to a failed `statvfs` is the outcome REL-05 forbids. REL-12's graceful pause-after-the-in-flight-frame stays in the capture flow, which is the only layer that knows what is in flight.

### 5.6 Session orchestrator — Phase 1 skeleton

Phase 1 needs single-shot capture only; the FSM ships with three states so the API shape is final from the start:

```
Idle ──start_capture──► Capturing ──saved──► Idle
   └──connect/disconnect device management──┘        Faulted (from any state; operator ack → Idle)
```

The full sequence FSM (targets, dithering, solve-and-center, pause/resume — SES-01..06) is specified in the Phase 2a increment; its states will be a superset and the persistence format (`session.json: sequence_state`) is already reserved.

### 5.7 Live view pipeline (`astroctl-pipeline::liveview`)

Two sources, one output path (WS binary frames on `liveview.frame`):

1. **Camera live view stream** (CAM-05): JPEG frames from the camera thread's watch channel, forwarded as-is; rate-limited per client (default 5 fps LAN / adaptive down to 1 fps, USB-11 groundwork).
2. **Last-captured preview** (CAM-06, IPP-04): on `frame.saved`, a decode job goes to the blocking pool — half-size RAW decode (`rawler`, per PRD §7; M1 handles only the simulator's FITS, the CR3 variant arrives with M2) → quarter-res RGB → asinh auto-stretch (fixed algorithm in Phase 1; the STF options come with the post-chain) → JPEG (quality 85) → cached as `<session>/preview/light_<id>.jpg` and pushed once on the bus.

Decode jobs are a queue of depth 1 with replace semantics: if frames arrive faster than decode, only the newest is previewed (previews are ephemeral; raw frames are what matters).

**Expected gaps.** Source 1 pauses whenever the camera thread is busy capturing (§5.3.1) — about
2 s per frame on the R10. The pipeline must treat this as normal: no reconnect attempt, no stream
-fault alert, no client teardown. The distinction the code needs is *stream idle because the camera
is busy* (fine, driven by `capture.progress`) versus *stream idle because the camera stopped
responding* (a wedge, §5.3.1). Conflating them produces either spurious alerts during every capture
or a missed wedge — both worse than the pause itself.

**Five things M1-T09 had to settle that the paragraphs above leave open.**

1. **Live view needs a control surface, and §5.8.1's table has no row for it.** The table describes
   the socket frames *arrive* on, not the tap that opens it, so `POST /api/camera/liveview/start`
   and `/stop` are declared alongside the other camera routes on the `low` tier, audited. Stopping
   must abort the forwarding task **and** call the driver's `stop_live_view`: aborting alone leaves
   the driver generating frames nobody reads, which on a real body is a USB transfer per frame for
   the rest of the night.
2. **"Pushed once on the bus" is pushed once on the *socket*.** `liveview.frame` is deliberately
   not a `Topic` (§4.3), so there is no bus to push it on; the preview goes to the `/ws/liveview`
   fan-out and the *announcement* — `capture.progress: preview_ready` — goes on the bus. Ordering
   is load-bearing: cache, then push, then announce, because the announcement is what sends a
   client to `/api/session/frames/{id}/preview.jpg` and announcing first races it to the file.
3. **The gap is only explained while the explanation is fresh.** `capture.progress` ticks once a
   second during an exposure, so a *recent* tick is evidence the camera is alive and busy. A
   client that trusted one `exposing` event without bound would show "capturing" until morning
   after a camera wedged mid-exposure — the missed wedge, reached from the other side. Freshness
   rather than "elapsed versus expected" is also the only bound that works for a **bulb** frame,
   whose length nothing on the wire knows.
4. **`capture.progress.elapsed_s` is per-state, not one clock.** It is already seconds of open
   shutter on `exposing` and the exposure's *length* on `saved`; on `preview_ready` it is how long
   the render took. The preview pipeline could not report the exposure even if the field meant
   that uniformly — `frame.saved` carries `{frame_id, path, size_bytes, sha256}` and no duration.
   A consequence worth stating: **the event stream cannot produce a countdown**, only a stopwatch,
   so a UI that wants one reads the exposure from the shutter setting (§5.9).
5. **Previews are a third session directory, `preview/`, and not part of the mirrored archive.**
   §6 calls them "ephemeral, regenerable", so they sit beside `frames/` and `control/` rather than
   inside `control/` — a retention pass must be able to tell "delete freely" from "never delete"
   by the path alone. They are deliberately absent from `testdata/session-layout.txt`, the fixture
   `astroctl-stack` mirrors (§5.11.3): the stack node renders its own.

### 5.8 API gateway (field binary)

#### 5.8.1 Route table (Phase 1)

All routes under bearer auth (§4.5); tier annotations present from Phase 1 (enforced from Phase 2c):

| Route | Method | Tier | Body → Response |
|-------|--------|------|-----------------|
| `/api/system/health` | GET | read | → `{status, disk_free_gb, clock_synced, uptime_s, cert_expires_at, cert_days_remaining, versions}` — `status` is `starting`\|`ok`\|`warn`; the certificate fields are `null` on a plain-HTTP node and `warn` is what SEC-07 degrades to inside `server.tls.warn_days_before_expiry` |
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
| `/api/transfer/status` | GET | read | → queue depth, oldest age, last ack, link state (§5.10.4) |
| `/api/auth/ws-ticket` | POST | read | → `{ticket, expires_in}` — single-use, 30 s, consumed by the WS upgrade (§4.5). The only way a browser can authenticate `/ws` and `/ws/liveview` |
| `/stack/*` | any | pass-through | reverse proxy to the stack node, auth forwarded (ADR-07); WS upgrades proxied too, so the operator keeps a single origin |
| `/ws` | GET | read | WS upgrade — control/status events (JSON only); subscribe message selects topics. Authenticated by `?ticket=` (§4.5), **not** by a bearer header |
| `/ws/liveview` | GET | read | WS upgrade — binary JPEG frames only (live view + previews); separate socket so a large frame can never head-of-line-block control traffic (§8.3). Authenticated by `?ticket=` (§4.5) |

`202 + WS progress` is the pattern for every long-running action; the response includes the event topic and correlation ID to watch.

**Manual slew is a dead-man's switch.** Each `/api/mount/slew` call authorizes motion for `ttl_ms` only (default 500 ms, max 2000 ms, clamped server-side). While the operator holds the D-pad, the PWA re-sends the same request every `ttl_ms / 2`; a repeat with identical parameters extends the deadline without re-issuing motor commands. If no renewal arrives before the TTL expires — dropped VPN packet, stuck touch event, crashed browser tab — the SafeMount TTL watcher stops that axis and emits an `alert` (`code: "SLEW_TTL_EXPIRED"`). Release sends `/api/mount/slew/stop` for immediate stop; TTL expiry is the backstop, not the primary stop path. Goto is *not* TTL-governed — it is a bounded, position-targeted motion supervised by slew-complete detection (§5.2.3) and the safety limits (§5.4).

**Motion-initiating commands reject staleness.** Every state-changing request carries `issued_at` (client UTC) and a client-generated `command_id`. The server rejects motion-*initiating* commands (goto, slew start, tracking start, capture start) whose `issued_at` is older than `max_command_age_ms` (default 2000) with `code: "COMMAND_STALE"` — a request delayed by VPN retry storms must not start motion long after the operator's intent has passed. The asymmetry is deliberate: **stopping commands (slew/stop, tracking off, abort, e-stop) are never staleness-rejected** — a late stop is safe, a late start is not. Client clock skew is handled by echoing server time in every response; the PWA offsets `issued_at` by the measured skew, and skew beyond 30 s raises a UI warning (ties into REL-14 clock discipline). `command_id` makes retries idempotent: a re-sent request with a known id returns the original outcome instead of re-executing.

**The envelope travels in headers (M1-T10).** `astroctl-command-id` and `astroctl-issued-at` on the request; `astroctl-server-time` on every response, and `astroctl-replayed: true` on one served from the ledger. The paragraph above says the request *carries* the two values and does not say where, and the route table's own bodies exclude them. Three facts settle it. Half the mutation surface has **no body to put them in** — `/api/mount/park`, `/unpark`, `/api/camera/capture/abort`, `/fault/ack` and both live-view controls declare no body extractor, and `/api/mount/connect`, `/disconnect` and `/api/mount/slew/stop` take an optional one precisely so a bare `POST` is not a 422; a body envelope would make a JSON body mandatory on a **stop**, reintroducing the parse-failure path §5.8.2 exists to remove. Every request struct is `deny_unknown_fields`, so a body envelope would also be two fields added to each of them by hand — the per-route omission this mechanism exists to prevent. And a header is readable *before* the body is, which is what lets the check run in the §8.2 layer ahead of any extractor.

**Which routes get which rule.** Declared per route as §8.2's `CommandClass`, never inferred from the path and never taken from the request:

| Class | Envelope required | Age-checked | Routes |
|-------|-------------------|-------------|--------|
| `motion_initiating` | yes | yes | `goto`, `slew`, `tracking`, `park`, `camera/capture` |
| `stopping` | **no** | no | `slew/stop`, `camera/capture/abort`, `camera/liveview/stop` |
| `neutral` | yes | no | `mount/connect`, `disconnect`, `unpark`, `camera/connect`, `disconnect`, `settings` (PUT), `fault/ack`, `camera/liveview/start` |
| `exempt` | no | no | `mount/estop`, and nothing else |
| `not_a_command` | no | no | every `GET`, plus `auth/ws-ticket` |
| `pass_through` | no | no | `/stack/*` — the serving node classifies (ADR-07) |

Four of those rows are decisions this section did not previously contain, and each is a safety property rather than a preference:

- **`/api/mount/tracking` is `motion_initiating` whole**, although the paragraph above lists "tracking off" among the stopping commands. There is no `tracking off` route: it is `{mode: "off"}` on the one tracking route, so a per-route class cannot express the split and a per-body class would let the *client* choose whether the age check applies — on the same route that starts sidereal tracking. The cost of classifying it whole is that a delayed "stop tracking" is refused, which leaves the mount tracking; §7 already argues that is the safe state ("a service restart mid-session must leave a tracking mount tracking"). The stops whose refusal would be unsafe each have their own route.
- **`unpark` is `neutral` despite its `high` tier.** It releases the park interlock and moves nothing. Tier and class are different questions and this is the row that proves it.
- **`camera/liveview/start` is `neutral`.** It drives the sensor, so it looks motion-shaped; but it acts only on the operator's own screen, and refusing a stale one produces a black image panel on exactly the slow link that made it stale.
- **`auth/ws-ticket` takes no envelope.** §4.5 makes the ticket single-use, so replaying a `command_id` would hand the second caller a ticket the first one already spent — and demanding an envelope would put a 422 inside the reconnect path, which is the one exchange that must work when the link is at its worst.

**What the idempotency ledger stores.** `command_id → HTTP response`, per process, bounded at ~1024 entries / 5 min / 8 KiB per body, evicted least-recently-used. Caching the *response* is what makes "returns the original outcome" correct for the `202 + WS progress` routes without anyone deciding what a goto's outcome is: a replayed goto answers the original `202 {correlation_id}`, and the slew it started still reports under that same id. Three rules the paragraph above leaves open:

- **Only 2xx outcomes are cached.** A command that failed did not complete. Replaying a cached `502 DEVICE_TRANSPORT` would turn one bad moment on a cable into a permanently refused command, and a cached `409 BUSY` would refuse a goto for five minutes after the mount went idle.
- **A duplicate arriving *while the first is still executing* is `409`, not a second execution.** The id is reserved for the duration of the request, and the reservation is released if the client gives up — which is the flaky tunnel this whole mechanism is for, and a leaked reservation would refuse that client's retry for five minutes with a message about a command nothing is running.
- **A future `issued_at` is skew, not staleness.** Only "older than" refuses. A device whose clock runs fast is precisely the case the skew correction exists for, and refusing it would make a wrong clock unrecoverable rather than merely wrong.

**Where the skew is measured.** From `server_time` on the `/ws` `pong` (§5.8.3), which the PWA already sends a ping for every five seconds and already times. The node's reading is attributed to the midpoint of that round trip, so transit is not counted as offset — on a link with a two-second RTT the uncorrected error is a second, which is half the staleness budget. The estimate is the median of the last few samples, so one pong delayed by a retransmit cannot move it. `astroctl-server-time` on every response carries the same value for clients that have no socket.

#### 5.8.2 E-stop handler

Registered before the normal middleware stack (auth only, no JSON parsing — empty body accepted), calls `SafeMount::emergency_stop()` directly. Handler + priority lane budget ≤ 20 ms; the PRF-12 end-to-end figure is then dominated by network RTT, as intended.

**How "before the normal middleware stack" is satisfied (M1-T05).** There is nothing to register it before: the stack a request traverses is exactly two layers — bearer auth, then routing (§5.8's middleware diagram) — so the e-stop is declared in the same table as every other route, on §5.8.1's `blocked-for-LLM` tier, and audited. The "no JSON parsing" half is a property of the handler's *signature*: it declares no body extractor at all, which is what makes an empty body, a missing content type and a stray JSON object all reach the same line of code. Bypassing a layer would be a mechanism that can be forgotten; a handler with no extractor cannot grow one by accident without a reviewer seeing it. Two orderings inside the handler are load-bearing and easy to lose: the driver call is the first statement (no lock, no status read, nothing that a position poll could be holding), and the wrapper's own bookkeeping — dropping slew leases so the TTL watcher does not fire on an axis already stopped — happens after it.

**The e-stop also voids the node's in-flight motion record.** Found on the running binary, not in review: §5.8.1's goto route keeps a record of the accepted slew and clears it when the driver's future resolves, and the HAL only promises that *dropping* that future does not stop the mount — it says nothing about how quickly a driver notices an abort. The simulator resolves it at the originally planned finish, so an e-stop one second into a 57-second slew left `/api/mount/goto` answering `409 BUSY` for the remaining 56 seconds while `/api/mount/status` answered `idle`. The operator recovering from an emergency stop is the last person who should be told to wait for a slew that is not happening. The handler therefore clears the record itself, after the driver call; and the motion task releases it **by correlation id**, so a task that resolves late cannot evict a newer goto's record and leave the node believing nothing is running while two slews are.

The reply is `200 {"stopped": true}` and deliberately carries no mount status. What the *telescope* did reaches the operator as the `EMERGENCY_STOP` alert on the event stream, because §5.9 forbids the UI rendering a mutation from its own request — and this is the control where that rule is worth the most.

#### 5.8.3 WS hub

One task serving two endpoints per client (§8.3 separation): `/ws` for JSON control/status events, `/ws/liveview` for binary image frames. Per-client bounded queues: on `/ws`, 64 events with a latest-only slot for `mount.position` (high-rate telemetry coalesces, discrete events never dropped while under bound); on `/ws/liveview`, a depth-1 replace queue — only the newest frame is ever in flight. Client subscribe/unsubscribe messages filter topics server-side. Reconnect is client-driven (PWA auto-reconnect, REL-10); on `/ws` connect the hub sends a state snapshot (current status of every stateful topic) so the UI never renders from partial state. Every outbound event carries `ts`, and the hub answers `ping` frames immediately — the PWA derives link RTT and telemetry age from these (§8.3).

**Frame shapes (M1-T03).** §4.3 requires the WS frame and the session-log line to be the identical serialization of `Event`, so an event frame is exactly `{v, ts, topic, data}` and has nowhere to put an envelope. The snapshot and the ping answer are therefore **control frames, told apart from events by carrying `type` where an event carries `topic`**. Nothing carries both.

```json
{"v":1,"type":"snapshot","ts":"…","events":[ <Event>, … ]}
{"v":1,"type":"pong","ts":"…","id":<echoed>,"server_time":"…"}
```

Client → server: `{"type":"ping","id":n}`, `{"type":"subscribe","topics":[…]}`, `{"type":"unsubscribe","topics":[…]}`. Unknown `type` values and unknown topic names are **ignored, not fatal** — a client newer than the node is normal after an upgrade, and refusing the connection would turn a forward-compatible client into a reconnect loop.

Three points this pins down that the paragraph above leaves open:

- **`ping` is an application-level message, not RFC 6455.** The browser `WebSocket` API can neither send a protocol ping nor observe a pong, so an answer at the protocol level is one no browser could ever ask for. `pong` echoes the request's `id` so a client can match an answer to its question rather than to "some recent pong".
- **The default subscription is every topic.** `subscribe` narrows it, `unsubscribe` narrows further. The PWA sends neither message, so a hub that required an explicit subscribe would deliver nothing to the only client there is.
- **The snapshot carries the latest event for each *stateful* topic** — `mount.position`, `mount.status`, `camera.status`, `capture.progress`, `transfer.status`, `stack.status`, `system.health`. `alert`, `frame.saved` and `transfer.acked` are occurrences rather than values that are true, and replaying them on every reconnect would show the operator a warning they already dealt with. **A topic absent from the snapshot means the node has no value for it**, and the client reduces it back to unknown: disconnect the mount and `mount.position` leaves the snapshot, so a reconnecting client shows no coordinates rather than the ones from before the drop.

Overflow closes the connection rather than dropping an event (§4.3), which is what makes "discrete events are never dropped while under bound" true rather than approximate: the client reconnects and resnapshots, so it sees current state instead of a panel quietly missing a frame.

**`/ws/liveview` frames carry an envelope (M1-T09).** "Binary JPEG frames only" names the payload and leaves the framing open, and the socket carries *two kinds* of image — live view and capture previews (§5.7). A client that cannot tell them apart cannot use either: a preview the operator is studying would be overwritten by the next live-view frame 200 ms later, and correlating with `preview_ready` on `/ws` cannot fix it, because the two sockets have no ordering between them — which is the entire point of separating them. Each frame is therefore `"ACLV" | version:u8 | kind:u8 | meta_len:u16be | meta JSON | JPEG`, where `kind` is `0` for live view and `1` for a preview, and the metadata carries `ts` always and `frame_id` on a preview. This is the same class of decision as the `type`-vs-`topic` split above: a convention the document does not state, recorded here so there is one.

**The depth-1 replace queue is the channel, not a policy over one.** A `watch` channel gives every client an independent cursor over a single slot, so a client stalled on a write misses the frames replaced while it was away and wakes to the newest — which is exactly the specified behaviour, with no eviction logic to get wrong. Unlike `/ws`, nothing on this socket must *not* be dropped: every frame is self-superseding, which is what makes it the socket that may drop things. It follows that a slow live-view client falls behind in **time**, never in queue depth, so it costs the node no memory and no other client anything.

### 5.9 PWA (M1 scope)

**Stack.** React + TypeScript, Vite build, output embedded in the binary via `include_dir!`
(ARC-02). Styling is **Tailwind** over a semantic token layer; headless primitives (Radix/Ark) are
used only where accessible behaviour is genuinely needed — dialogs, sliders — not as a component
library. State is **Zustand**. The reasoning is that bundle size is a *functional* requirement here
rather than a nicety (USB-10 promises the shell opens while the tunnel is still connecting), and
the visual language this app needs — near-black, red-capable, oversized targets, dense telemetry —
is unusual enough that a general-purpose component library is something you fight rather than use.

**Target platform.** Android/Chrome is the supported and tested target. iOS may work but is
untested and is not gated by any acceptance criterion. **EXT-06 (Capacitor-readiness) is therefore
advisory, not binding** — its stated motivation in PRD §11 was iOS PWA limitations. That buys three
things worth having: the **Screen Wake Lock API**, so the display does not sleep mid-session while
the operator watches live view; `beforeinstallprompt` for a real install flow rather than a
share-sheet instruction; and reliance on service-worker cache persistence, which iOS evicts after
about seven days of non-use and would otherwise have made USB-10's promise quietly conditional.

**All three of those capabilities require a secure context, which is why SEC-05 makes TLS a Must
rather than a hardening option.** Chrome exposes `navigator.wakeLock`, service-worker registration
and `beforeinstallprompt` only on HTTPS or `localhost`. Being inside a VPN does not qualify — the
origin is what the browser judges, and `http://` over a tunnel is still an insecure origin. The
practical consequences for anyone working on the frontend:

- **`localhost` is a false positive.** Every one of these works when the developer opens
  `http://localhost:8470/`, and none of them work on the phone over the VPN. A capability check
  that passes on the workstation says nothing about the field. This was found by testing on a real
  phone after M0-T06 reported the shell working, and it is the reason the four device gates in
  that task can only be honestly run over TLS.
- **Degrade, do not assume.** Report each capability's actual state rather than presuming it —
  M0-T06's device card is the pattern. `navigator.wakeLock` being `undefined` is a normal
  condition on an insecure origin, not a browser too old to support the app.
- **An expired certificate revokes the secure context** exactly as a missing one does, silently
  disabling the wake lock and the installed app. SEC-07 puts expiry in the health payload for
  this reason; the UI should surface it as a real warning, not a diagnostic detail.

**Colour architecture — decided at M0 even though night mode is Phase 4.** All colour resolves
through semantic CSS custom properties (`--surface`, `--fg`, `--accent`, `--warn`, `--danger`),
never literal values in components. USB-02's red/dim mode is then a `:root[data-mode="night"]`
override in one file instead of a pass over every component. This ordering is deliberate: the
*implementation* of night mode is Phase 4, but the *architecture that permits it cheaply* has to
exist from the first component or it never will. Surfaces are true black rather than dark grey —
better on the OLED panels these devices have, and better for dark adaptation.

**Night mode and images.** A stretched star field is greyscale-white, so a preview at full
brightness destroys exactly the dark adaptation the rest of night mode is protecting — recovery
takes 20–30 minutes. Image surfaces therefore render through a red-channel filter at reduced
brightness when night mode is active, with a clearly-labelled per-panel **"true colour"** toggle
that auto-reverts after a short interval. The toggle is not a concession: framing and focus checks
need honest pixels, and a mode the operator has to fight is a mode they will switch off entirely.

**Touch targets.** USB-12's 44 px is the floor for incidental controls, not the goal for the ones
that matter. The operator may be wearing gloves, which materially reduces pointing precision, so
primary controls — D-pad, capture, e-stop — are **60–70 px**. The e-stop is larger still and holds
a constant screen position across every view (USB-03) so it can be hit without looking at it.

**Layout is organised by session state, not by subsystem.** The obvious decomposition — a mount
panel, a camera panel, a stack panel — mirrors the *backend's* structure and is wrong for the
operator, whose sequence is **pick a target → slew → frame → configure → capture → watch it
accumulate**. Manual mount control in particular is not a destination: it is a brief
fine-adjustment step that only matters in the window after a slew settles. Given a permanent panel
it occupies screen space for the 95% of a session when nobody touches it.

The session FSM (§5.6) already knows which state the system is in, so the UI follows it.

```
   TARGET                      FRAME                       STACK
┌──────────────────────┐  ┌──────────────────────┐  ┌──────────────────────┐
│ ●mnt ●cam ○stk 21:14 │  │ ●mnt ●cam ○stk 21:14 │  │ ●mnt ●cam ●stk 21:14 │
│               [STOP] │  │               [STOP] │  │               [STOP] │
├──────────────────────┤  ├──────────────────────┤  ├──────────────────────┤
│ TARGET               │  │ M42 Orion   alt 47°↑ │  │ M42 Orion   alt 47°↑ │
│ ┌──────────────────┐ │  ├──────────────────────┤  ├──────────────────────┤
│ │ RA   __:__:__._  │ │  │                      │  │                      │
│ │ DEC ±__°__'__"   │ │  │                      │  │  ACCUMULATING STACK  │
│ │                  │ │  │      LIVE VIEW       │  │                      │
│ │    [  GOTO  ]    │ │  │                      │  │                      │
│ └──────────────────┘ │  │                      │  ├──────────────────────┤
│   ↑ 2a: catalog      │  │              ╭───╮   │  │ 47 fr · 23m30s · q3  │
│     replaces this    │  │              │ ⊕ │   │  │ FWHM 3.1" ↓          │
│                      │  │              ╰───╯   │  │ ┌──────────────────┐ │
│ now  05:34:32.1 ~    │  ├──────────────────────┤  │ │ (2b: knobs land  │ │
│      +22°00'52"      │  │ ISO1600 30s RAW      │  │ │  in here)        │ │
│      alt 47° az 128° │  │          [ CAPTURE ] │  │ └──────────────────┘ │
├──────────────────────┤  ├──────────────────────┤  ├──────────────────────┤
│ ◎target ▣frame ⛁stack│  │ ◎target ▣frame ⛁stack│  │ ◎target ▣frame ⛁stack│
└──────────────────────┘  └──────────────────────┘  └──────────────────────┘
```

Three consequences worth stating explicitly, because each is easy to get wrong:

1. **The D-pad overlays the image; it does not sit beside it.** Nudging *is* framing, so the
   control and the thing it affects must be in one field of view. A separate panel makes the
   operator look back and forth between their hand and the result.
2. **Summoned, never automatic.** The nudge affordance is a small badge in the **bottom-right
   corner of the image surface**, and the D-pad appears only when the operator taps it. It does
   **not** auto-expand when a slew completes: the operator has just waited through a slew to see
   the frame, and popping a D-pad over it covers precisely what they were waiting for. The badge
   itself is always visible, so nothing is hidden — only the expansion is deliberate.

   Bottom-right is where a thumb naturally rests on a phone, which suits a frequent action. That
   it is diagonally distant from the e-stop's fixed header position is the point: the frequent
   control should be easy, and the irreversible one should take a deliberate reach.

   **The badge shows whether nudging is possible before it is tapped** — the mount may be
   disconnected, mid-goto, or against a limit, and discovering that by tapping and getting nothing
   is the frustrating case. Colour carries this (green available, red not) **but colour cannot
   carry it alone**: night mode collapses every hue toward red, so red-versus-green becomes
   red-versus-red, and roughly 8% of men have a red-green deficiency in any lighting. The state
   must therefore be encoded redundantly — a **filled** badge when available, **hollow with a
   slash** when not — so the distinction survives both night mode and the operator. Tapping an
   unavailable badge explains why rather than doing nothing.

   ```
      summoned D-pad, over the image        badge states
   │                      │              ╭───╮   filled  → available
   │      LIVE VIEW       │              │ ⊕ │   (green, but the shape carries it)
   │         ┌────┐       │              ╰───╯
   │      ┌──┼ N  ┼──┐    │              ╭╌╌╌╮   hollow + slash → unavailable
   │      │W │    │ E│    │              ┆ ⊘ ┆   (mount down / goto in flight /
   │      └──┼ S  ┼──┘    │              ╰╌╌╌╯    axis at a limit) — tap explains
   │         └────┘  ╭───╮│
   │    ●●●○○ speed  │ ✕ ││
   ```

3. **The target region is a slot with a stable contract.** In M1 it holds manual RA/DEC entry.
   Phase 2a's catalog (PLN-03/04) drops into the same slot without restructuring anything around
   it — it changes how a target is *chosen*, not what the rest of the UI does with one.

**The accumulating stack is a primary view, not a status readout.** PRD §2 promises "immediate
visual feedback as signal accumulates — no waiting until post-processing to know if the session is
working." That is the payoff the whole two-node architecture exists to deliver, so it gets
first-class screen real estate rather than a queue-depth badge.

`FRAME` and `STACK` are therefore **two sources sharing one image surface**, not two panels:

```
┌──────────────────────────────────────────────────────────────┐
│ ●mount ●camera ●stack    LST 21:14   RTT 18ms       [ STOP ] │
├────────────────┬─────────────────────────────────────────────┤
│ M42            │             [ FRAME │ STACK ]               │
│ Orion Nebula   │  ┌───────────────────────────────────────┐  │
│                │  │                                       │  │
│ alt 47° ↑      │  │            image surface              │  │
│ transit 1h20   │  │                                       │  │
│ RA  05:35:17   │  │                             ╭───╮     │  │
│ DEC -05°23'    │  └─────────────────────────────╰─⊕─╯─────┘  │
│                │  ( 2b: stretch ▁▃▅  σ-clip 2.5/3.0 [apply] )│
│ ── STACK ──    │  ( re-stacking 34/120 ▓▓▓▓░░░░  ← IPP-16 )  │
│ 47 frames      ├─────────────────────────────────────────────┤
│ 23m30s integ   │  ISO 1600    30s    RAW        [ CAPTURE ]  │
│ queue 3        │                                             │
│ FWHM 3.1" ↓    │                                             │
│ [ change ]     │                                             │
└────────────────┴─────────────────────────────────────────────┘
```

They answer different questions at different times — live view is for framing and focus, the stack
is for "is this working" — so the app switches to `STACK` when a capture sequence starts, and the
operator can switch back at will.

The tablet target column carries the stack statistics deliberately: both answer "what is this
session doing", and keeping them together leaves the image surface uninterrupted — which is the
same reason the D-pad goes on top of the image rather than beside it.

**The stack view is a slot with a stable contract**, exactly like the target region:

| | What the operator sees | Requirements |
|---|---|---|
| **M1** | Preview from the stub worker, queue depth, frame count, last-preview age. **No knobs** — the stub does no stacking, so there is nothing to tune | USB-06 subset |
| **Phase 2b** | Real accumulating stack, live statistics, and the first knobs: method, rejection thresholds, stretch | STK-05, STK-10, IPP-07 |
| **Phase 2c** | Post-processing chain, before/after comparison, presets, reprocessing of past sessions | PPR-*, IPP-13 |

**A knob change must show that it is working.** IPP-16 rebuilds the accumulator in the background
while capture continues, and ADD §5.4.2 keeps the preview serving the *pre-rebuild* stack until the
swap. So adjusting sigma and seeing the image not change is the correct behaviour — and looks
exactly like a bug. The panel needs an explicit rebuilding state with progress ("re-stacking
34/120"), or the operator turns the knob again, and again.

**This is where night mode's true-colour toggle earns its place.** The stack preview is the largest
image on screen and the most greyscale-white thing in the app; it is also precisely what you are
looking at when judging a stretch. Filtering it red by default protects dark adaptation; being able
to check it honestly, briefly, is what stops the operator disabling night mode altogether.

Phone navigation is therefore three concerns rather than five subsystems: **Target** (what to point
at), **Frame** (acquire) and **Stack** (the result). Connection status and alerts live in the
header where USB-04 already puts them. On tablet all three are visible at once and the navigation
disappears.

**Four slots, so later phases fill structure rather than replace it.** Everything else in the
layout is fixed from M0 — e-stop top-right on every screen, status and LST in the header, the image
surface as the persistent centre, nudge bottom-right, 60–70 px primary targets, semantic tokens
with the night-mode override.

| Slot | M1 | Phase 2a | Phase 2b / 2c |
|------|----|----------|----------------|
| Target chooser | manual RA/DEC entry | catalog picker (PLN-03/04) | — |
| Stack controls | nothing — the stub worker does no stacking | — | knobs (IPP-07), then post-chain (PPR-*) |
| Rebuilding indicator | reserved, never fires | — | fires on every knob change (IPP-16) |
| Nudge availability | connected / disconnected | + goto in flight | + axis at limit |

**Connect flow.** Before each `WebSocket` construction — the first and every reconnect — the PWA
POSTs `/api/auth/ws-ticket` with its bearer token and opens the socket with the returned ticket
(§4.5). A reconnect therefore costs one extra round trip, which is the right trade against putting
a long-lived credential in a URL that gets logged. If the ticket request itself fails with 401 the
token is bad and the UI must say so rather than retrying forever.

**Store discipline.** A thin store fed exclusively by WS events plus the connect snapshot — no REST
polling. Commands are REST calls that **optimistically do nothing**: UI state changes only when the
corresponding event arrives. On a link where a command may not have landed, optimistic UI actively
lies about where the mount is, which is the one thing this display must never do. The store is
shaped as an explicit reducer over the event stream, and subscription is selector-based because
`mount.position` ticks at 1 Hz and must not re-render panels that do not read it. Telemetry carries
three states — **confirmed, predicted, stale** — for the predictive display below. If the hub drops
this client as a slow consumer (§5.8.3), the store must **resnapshot rather than resume from a
hole**. Two link-latency affordances (§8.3): **predictive position display** — between `mount.position` updates the UI dead-reckons the displayed coordinates from the last update and the known tracking/slew state (a tracking mount's motion is exactly predictable), rendering predicted values in a visually distinct "aging" style that resolves to confirmed on the next event; and **link-health surfacing** — header shows WS RTT and telemetry age, turning amber past 500 ms RTT / 3 s age and red on disconnect, so the operator always knows how stale their picture is before issuing commands. Phase 1 screens: connect panel, mount panel (coordinates, tracking, D-pad with press-and-hold slew — hold renews the slew TTL per §5.8.1, release sends stop, speed selector), camera panel (settings, capture, bulb countdown), live view/preview panel, header status bar (USB-04), e-stop button fixed in the header on every screen (USB-03, 44 px targets USB-12). Manifest + service worker per USB-09/10 (shell cached, data never cached).

**The live-view panel must explain its own pauses.** During a capture the stream stops for about
two seconds (§5.3.1 — one gphoto2 context, unavoidable). Driven by `capture.progress`, the panel
shows a "capturing" state with the exposure countdown over the last frame, and resumes on
`preview_ready`. This is not decoration: an unexplained freeze in the one widget that shows live
motion is indistinguishable from a crashed app, and the operator's next move is to reload — in the
dark, mid-session, on a phone. Every state the backend can be in that stops pixels arriving needs a
distinct visual, including the wedge-recovery path (`camera.status: reconnecting`).

### 5.10 Transfer agent (`astroctl-transfer`)

Durable, resumable delivery of frames to the stacking server. The invariant is that the frame
is already durable locally before the agent ever sees it (§5.3.2, REL-05) — the agent can
therefore fail, restart, or stay offline indefinitely without endangering data.

#### 5.10.1 Journal and state machine

One SQLite database, `<queue_dir>/transfer.db`, WAL mode, single writer:

```sql
CREATE TABLE queue (
  session_id   TEXT NOT NULL,         -- 2026-07-29_ngc7000
  frame_id     TEXT NOT NULL,         -- light_00042 — per-session counter (§5.5), so the
                                      -- id alone recurs in every session; §5.11.2's dedup
                                      -- correction applies to this table identically
  PRIMARY KEY (session_id, frame_id),
  session_id   TEXT NOT NULL,
  path         TEXT NOT NULL,         -- absolute; frame lives in the session dir, not copied
  sha256       TEXT NOT NULL,
  size_bytes   INTEGER NOT NULL,
  state        TEXT NOT NULL,         -- queued | uploading | acked | failed
  attempts     INTEGER NOT NULL DEFAULT 0,
  queued_ts    TEXT NOT NULL,
  acked_ts     TEXT,
  reclaimable  INTEGER NOT NULL DEFAULT 0
);
```

Frames are **referenced, never copied** into a spool: `queue_dir` holds only the journal. This
keeps the write-once frame the single copy on the field node (REL-11) and makes enqueue O(1).

State transitions: `queued → uploading → acked`, with `uploading → queued` on any failure.
`failed` is terminal and requires operator action; it is reached only when the stack node
returns a *definitive* rejection (checksum mismatch after re-read, or a 4xx that is not 408/429).
Transport failure is never terminal — an unreachable stack is a normal operating state.

#### 5.10.2 Upload loop

Single task, one upload in flight (ordering matters for the operator's mental model, and
concurrency buys nothing on a constrained tunnel):

```
subscribe frame.saved ─► insert row (queued) ─► notify uploader
uploader: pick oldest queued ─► mark uploading ─► POST multipart to stack /api/ingest
          ─► on 200 {sha256, stored}: verify echoed sha == ours
                 ─► mark acked, reclaimable=1, emit transfer.acked
          ─► on transport error / 5xx / timeout: mark queued, attempts+=1, backoff
```

Backoff is capped exponential from `stacking_server.retry_interval` (config), doubling to a
5-minute ceiling. **One** `alert` is emitted when the link transitions to offline and one when
it recovers — never per attempt; a night-long outage must not produce thousands of events.

#### 5.10.3 Restart recovery and reclaim

On startup the agent scans for `uploading` rows and returns them to `queued`: re-upload is
always safe because ingest deduplicates by `(frame_id, sha256)` (§5.11.2). A crash mid-upload
therefore costs one retransmission, never a lost or duplicated frame.

`reclaimable=1` is *marking only*. No deletion path exists in this increment — REL-13's retention
policy (operator-configured, opt-in) is designed in the Phase 2b increment. The flag is the durable record
that the archive of record has the frame.

#### 5.10.4 Interface

`GET /api/transfer/status` → `{state, queue_depth, oldest_queued_age_s, last_ack_ts, attempts_current}`.
The `transfer.status` **event** (§4.3) carries the first four of those; `attempts_current` is
REST-only, because a retry counter ticking behind a temporarily unreachable stack node is
diagnostic detail the operator can pull when they care, not something worth pushing to every
connected client. Earlier wording claimed the two were "the same data", which they are not. Pacing (§8.3.7) is a
binding rule on this element but its implementation lands with Phase 2b; the config keys exist
from M1 (PRD §8.1 `stacking_server.pacing`) and are parsed and validated but not yet enforced —
a deviation that must be removed, not forgotten, when 2b lands.

### 5.11 Stack ingest and session mirror (`astroctl-stack`)

The receiving half of ADR-05. Its contract is narrow and absolute: **an ack means the bytes are
on the stack node's disk, fsynced, and their checksum matched.**

#### 5.11.1 Route table (M1 scope, stack node `:8471`)

| Route | Method | Body → Response |
|-------|--------|-----------------|
| `/api/system/health` | GET | → `{status, disk_free_gb, versions, worker: {state, restarts}}` |
| `/api/system/info` | GET | → config summary, **resolved runtime worker threads (§7)**, route table. §7 requires this on *both* binaries; it was listed only for the field node |
| `/api/ingest` | POST | multipart, `meta` **then** `frame` — see the schema below → `{v, session_id, frame_id, sha256, stored: true, duplicate: bool}` |
| `/api/ingest/{session_id}/{frame_id}` | HEAD | pre-flight, asked **before** committing to an upload: 204 with `X-Astroctl-Sha256` if the frame is stored, 404 if not. M1-T12 proved the POST cannot answer a duplicate cheaply — HTTP forbids acking before the body drains (an early response is lost to a client still writing), so without this a duplicate still costs its full ~200 s at 1 Mbit. The sender treats any HEAD failure as "not stored" and uploads; the pre-flight is an optimisation, never a gate |
| `/api/stacking/stats` | GET | → `{v, session_id, frame_count, last_ingest_ts, last_preview_ts}` (real statistics arrive in 2b) |
| `/ws` | GET | WS — JSON status events |
| `/ws/preview` | GET | WS — binary JPEG previews only (mirrors the field node's `/ws/liveview` split, §8.3(5)) |

All routes under the same bearer-token middleware as the field node (§4.5). `/api/ingest` declares
tier `low` and is **audited** (§8.2): it changes state but moves nothing, and the audit log is
where "when did this node stop receiving frames" gets answered. It also raises the request body
limit to 512 MiB + 1 MiB of slack for the metadata part and the MIME framing; the framework
default is 2 MiB, which rejects every real frame.

##### The `meta` part

```jsonc
{ "v": 1,                       // equality-checked, like the worker handshake (§5.12.2)
  "session_id": "2026-07-29_ngc7000",   // [A-Za-z0-9._-]{1,64}, no leading dot
  "frame_id":   "light_00042",          // <kind>_<id> per §5.5: [a-z]{1,16} _ [A-Za-z0-9-]{1,64}
  "sha256":     "<64 hex>",             // of the frame bytes; case-insensitive on the wire
  "size":       26214400,               // exact, 1..=512 MiB — enforced per chunk, not at the end
  "ext":        "cr3",                  // [a-z0-9]{1,8}; the stored name is <frame_id>.<ext>
  "capture":    { … } | null,           // opaque; mirrored verbatim to control/quality_<id>.json
  "session":    { "target": … | null,   // opaque; session.json is built from this (§5.11.4)
                  "equipment": … | null,
                  "created_ts": "<RFC 3339 ms>" | null } | null }
```

`session_id`, `frame_id` and `ext` are whitelist-validated because they become path components:
this is the check that keeps an upload inside the archive. `capture` and `session` are deliberately
opaque — the field node owns those schemas (§5.5), and a second declaration here would drift from
it. The object is otherwise strict: an unrecognized key is a `VALIDATION` refusal, since silently
dropping capture metadata has no symptom, and `v` is what makes that strictness safe.

`meta` **must precede** `frame`. Streaming to the right path requires the destination, the declared
size and the dedup answer first, and buffering the frame instead would put 25 MB in RAM.

#### 5.11.2 Ingest procedure

```
parse meta                                    → 422 VALIDATION on anything malformed
  → free space < storage.disk_critical_free_gb → 507 DISK_FULL, body NOT read (see below)
  → journal lookup (session_id, frame_id)
        same sha256      → drain body, 200 {duplicate: true}     ← file never touched
        different sha256 → drain body, 409 FRAME_ID_CONFLICT
  → stream frame → sessions/<sid>/frames/.tmp_<frame_id>.<pid>.<nonce>   (never whole in RAM)
        hashing as the bytes arrive; over meta.size → 422, tmp deleted
  → sha mismatch → delete tmp, 422 CHECKSUM_MISMATCH, nothing stored
  → fsync file → renameat2(RENAME_NOREPLACE) → fsync frames dir      ← durable
        EEXIST → re-hash what is there: equal → adopt, differ → 409 FRAME_ID_CONFLICT
  → control/quality_<id>.json  → journal row → session.json → 200 {stored: true}
```

Hashing happens *during* the stream, so a corrupt 25 MB upload costs one disk write and no
second pass. Six properties of that order are load-bearing:

1. **The dedup key is `(session_id, frame_id)`, not `frame_id`.** §5.5 hands out frame ids from a
   per-session counter, so `light_00042` recurs in every session. On a node that mirrors many
   sessions, a bare `frame_id` key rejects the second session's frame 42 as
   **`409 FRAME_ID_CONFLICT`** — same id, different bytes — and that verdict is terminal, so the
   sender stops retrying and the frame is simply lost. The near miss is worth naming: the failure
   is *not* a false `duplicate: true`, because the two frames' hashes differ and the sha
   comparison would separate them. It is the conflict branch firing on two frames that were never
   in conflict, which is why fixing this by comparing hashes harder does not work — the key itself
   has to carry the session. (§5.10.1's `frame_id TEXT PRIMARY KEY` holds only within one field
   node's queue.)
2. **The temporary carries a nonce.** Two overlapping uploads of one frame id would otherwise
   share `.tmp_<frame_id>`, and the loser goes on writing into a descriptor the winner has already
   renamed *into the archive* — a stored raw modified after the fact, which REL-11 forbids.
3. **`renameat2(RENAME_NOREPLACE)`, not `rename`.** A `metadata()` check followed by a rename has
   a window in which a concurrent retry lands, and `rename(2)` replaces silently. The kernel does
   it atomically or fails with `EEXIST`, and `EEXIST` is the crash-recovery path: a frame on disk
   that no journal row knows about (a crash between the two steps) is re-hashed and adopted rather
   than refused or rewritten.
4. **The journal row is written after the frame, never before.** The row is REL-13's authority: a
   field node deletes its only copy on the strength of it. It may under-claim — a missing row costs
   one retransmission — but never over-claim.
5. **A definitive answer is delivered after the body is drained.** Responding before the body is
   consumed makes the server close the connection, and a client still writing gets a transport
   error instead of the response — so a `duplicate` or `conflict` verdict would never be recorded
   and the frame would be retried forever. The `507` is the deliberate exception: it means "come
   back later", which is what a transport error produces anyway, and making a node whose disk is
   full accept 25 MB before saying so defeats REL-12's back-pressure.
6. **The derived files are not part of the ack.** `quality_<id>.json` and `session.json` are
   rebuildable from the frames and the journal; a failure to write one is logged, not returned.
   Failing an ingest whose bytes are already durable would ask the field node to re-send 25 MB to
   fix a metadata write.

Every answer is chosen against §5.10.1's rule that a 4xx which is not 408/429 is *terminal* for
the sender: `CHECKSUM_MISMATCH`, `FRAME_ID_CONFLICT` and `VALIDATION` are 4xx because they are
definitive; `DISK_FULL` is 507 and carries `retryable: true` (overriding §4.2's default for that
code) because freeing space makes the identical request succeed; a body that stops arriving is
mapped to 5xx, never 4xx, because abandoning a good frame over a dropped link would lose it.

#### 5.11.3 Session mirror and journal

The mirror layout is **byte-for-byte the same structure** as the field node's (§5.5) — this is
asserted by a fixture test shared between `astroctl-session` and `astroctl-stack`, so the two
layouts cannot drift. The fixture is `crates/astroctl-session/testdata/session-layout.txt`, a list
of relative paths read by both crates' tests with `include_str!` rather than through a Cargo
dependency: it is data, and the ADD §5.6 matrix should not acquire an edge to carry it. It fixes
the *paths*; `session.json` differs in content between the nodes by design, so a content-level
fixture would assert something this design does not require.

`session.json` on the stack is constructed from ingest metadata rather than copied, and tolerates
frames arriving in any order or long after the session ended (IPP-15):

```jsonc
{ "v": 1, "session_id": "…",
  "created_ts": "…",              // earliest known: the field node's, else the first arrival —
                                  //   a late frame must not rewrite when the session began
  "target": … | null,             // latest non-null wins; a frame with no session block does not
  "equipment": … | null,          //   erase what an earlier one supplied
  "mirror": { "frame_count": 3, "first_ingest_ts": "…", "last_ingest_ts": "…" } }
```

`ingest.db` (SQLite, WAL) records every received frame with source and timestamp — one row keyed
`(session_id, frame_id)` holding `sha256`, `size_bytes`, `rel_path`, `source` and `received_ts`.
It lives **on the archive volume**, at `<storage.sessions_dir>/ingest.db`: an index that can be
mounted separately from the frames it indexes will one day describe a different disk's contents,
and PRD §8.2 has no key for a state directory. `synchronous = FULL` under WAL, because the ack
that quotes a row is a durability claim and the alternative loses the last commits to a power cut
— exactly the window in which the field node was told it could reclaim. It is the future authority
for REL-13 reclaim decisions and, from 2b, the work list for rebuilds.

"Which session is current" is answered by **insertion order** (`MAX(rowid)`), not by
`MAX(received_ts)`: timestamps are millisecond-resolution (§2), so two sessions gaining a frame in
the same millisecond tie, and REL-14 admits a clock that steps backwards, which insertion order
cannot do.

Opening the archive and its journal is a **startup step, before the socket** (§8.1): a node that
cannot record an ingest cannot honour the contract this section opens with, and one that accepts
frames it cannot account for is worse than one that refuses to start. Startup also sweeps
`.tmp_*` files from every session directory — nothing else ever removes a temporary left by a
killed upload, and on an archive volume those accumulate into an unexplained disk-full alert
months later (REL-12).

### 5.12 Worker IPC and supervision (`astroctl-ipc`)

Per ADR-13: versioned JSON over stdio, frames passed by filesystem path, workers supervised as
child processes. This channel never crosses the network and never carries pixel data.

#### 5.12.1 Framing and message set (protocol v1)

One JSON object per line on stdin/stdout, UTF-8, newline-delimited; the worker's stderr is
captured into the backbone's `tracing` output with a `worker` field and is *not* part of the
protocol. Line framing (not length prefixing) keeps the worker debuggable by hand — a developer
can pipe messages into `compute_worker.py` from a shell.

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToWorker {
    Hello    { proto_version: u16 },
    Job      { id: u64, kind: JobKind, params: serde_json::Value, paths: Vec<PathBuf> },
    Cancel   { id: u64 },
    Ping     { nonce: u64 },
    Shutdown,
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromWorker {
    Hello    { proto_version: u16, capabilities: WorkerCaps },  // gpu, vram_mb, libs
    Progress { id: u64, pct: u8 },
    Result   { id: u64, ok: bool, data: Option<serde_json::Value>, error: Option<WorkerError> },
    Pong     { nonce: u64 },
    Log      { level: String, message: String },
}
```

`JobKind` in this increment is `Preview` only. `WorkerError` carries a `code` from a closed enum
plus a message, so worker failures reach the operator through the same error vocabulary as
everything else (§4.2).

#### 5.12.2 Handshake

The backbone writes `Hello{proto_version}` and waits for the worker's `Hello`. Version equality
is required — **not** compatibility ranges. A mismatch is logged with both versions and the
worker is not used; the supervisor does not retry a version mismatch (retrying a deterministic
failure is a crash loop). This is the drift detector ADR-13 exists for, and it must fail at
startup rather than on the first job.

A worker that produces no `Hello` within 10 s is killed and treated as a failed start.

**`result` cannot express a contradiction.** `{ok, data, error}` as listed permits "succeeded,
here is the failure". Both decoders reject it: `ok: true` ⟺ `error` absent, checked at parse time
so a contradictory frame is a decode error rather than a runtime surprise three layers up. Absent
`data`/`error` are omitted, never null.

**Frames are bounded at 1 MiB including the newline, and a trailing partial frame is dropped.** A
worker that loses framing would otherwise stream its heap into the backbone — and a reader that
grows a buffer before anyone can object gives no opportunity to refuse. A worker SIGKILLed midway
through writing a result leaves a newline-less remainder; discarding it at EOF is what stops a
truncated JSON object from parsing as a whole one by luck.

**Unknown fields are ignored, deliberately.** A strict decoder would defeat §5.12.2: a v2 worker's
`hello` must parse far enough for the version check to fire, or the operator gets "malformed frame"
instead of the two version numbers the mismatch path exists to report.

**A stray `print` in worker code breaks framing.** fd 1 belongs to the protocol, so the worker
redirects Python-level stdout to stderr at channel open. Without that, a library banner from some
Phase 2b dependency desynchronises the decoder, and the symptom arrives hours later as previews
quietly stopping.

**The JPEG write is atomic** — temp file then `os.replace`, the same discipline as §5.11.2's
archive. A worker killed mid-save would otherwise leave a truncated file at the path the API serves.

#### 5.12.3 Supervision

```
spawn(python_interpreter, compute_worker.py) ─► handshake ─► ready
   │                                                          │
   ├─ Ping every `health_ping_seconds` (config, default 5)     │
   │     3 consecutive missed Pongs ──► SIGKILL ──────────────┤
   ├─ child exit (any cause) ────────────────────────────────►┤
   └─ job exceeds `job_timeout_seconds` ─► Cancel, then kill ─┘
                                                              ▼
                                            restart with capped exponential backoff
                                            (base `restart_backoff_seconds`, 60 s ceiling)
```

**A worker must answer pings while it computes.** This is a requirement on the worker, not an
implementation note, and it is the one thing above that a naive worker gets wrong. At the defaults
— ping every 5 s, three misses is a kill — a single-threaded worker that computes for **15 seconds
is SIGKILLed by its own liveness probe**, and every real stacking job is longer than that. The
symptom is a worker that dies partway through each frame, restarts, and dies again: a crash loop
whose cause is the health check.

The config validator does **not** protect against this, and reads as though it might: bounding
`job_timeout_seconds` above `3 × health_ping_seconds` only ensures the timeout is not the first
thing to fire. It says nothing about whether pongs arrive during the job.

So the contract is: **read stdin and answer `ping` on a thread that compute does not occupy.**
`workers/compute_worker.py` is the reference — it reads on the main thread and computes on another.
Verified by `pings_are_answered_while_a_job_is_running`, which was shown to fail (`no answer to 3
consecutive pings`) when the stub was temporarily changed to compute on its reading thread; a test
that has never failed would not be evidence of anything here.

Two further supervision rules that follow from the same place:

- **The retry backoff resets** once a worker has run longer than the 60 s ceiling. Without a reset,
  one bad night leaves the supervisor at the ceiling permanently, and the next night's first frame
  waits a minute for a worker that would have started instantly.
- **Writes to the child are deadline-bounded.** A worker that stops reading stdin fills the pipe,
  and an unbounded write then parks the same task that holds the ping timer — the wedge disables
  its own detector. The deadline is what keeps the liveness check independent of the thing it
  watches.

An in-flight job whose worker dies is retried **once** on the fresh worker, then failed with an
alert — a job that reliably kills its worker must not become a restart loop. The restart counter
is exposed in `/api/system/health` and in `stack.status`, because a worker quietly restarting
every few minutes is the failure mode most likely to go unnoticed.

Capture on the field node is unaffected by any of this by construction: the worker sits behind
ingest, and ingest acks on durability, not on processing.

#### 5.12.4 The M1 stub worker

`workers/compute_worker.py` implements the handshake and `JobKind::Preview` only: read the frame
at the given path, asinh-stretch, write a JPEG beside it, return its path. **No stacking math, no
GPU, no accumulator.** Its dependency list stays minimal (numpy plus a FITS reader) so that
`workers/requirements.txt` does not acquire the CUDA stack before anything needs it.

The point of the stub is that the machinery around it — spawn, handshake, ping, restart, job
round-trip, protocol versioning — is exercised end-to-end from the first milestone with compute
too trivial to hide bugs. When real stacking arrives in 2b, only the inside of this file changes.

A small Python mirror of the message types lives in `workers/astroctl_ipc.py`; it and the Rust
enums are checked against a shared golden-message fixture (T-IPC-1) so the two definitions cannot
drift silently.

---

## 6. Data Design (M0–M3)

| Artifact | Format | Written by | Schema highlights |
|----------|--------|-----------|-------------------|
| `field-node.yaml` | YAML | operator | PRD §8.1; deny-unknown-fields |
| `session.json` | JSON v1 | frame store | id, created, equipment snapshot, frame counter, sequence_state (reserved) |
| `frames/light_<id>.cr3` | CR3 | camera driver | immutable after rename (REL-11) |
| `control/quality_<id>.json` | JSON v1 | capture flow | ts, exposure, iso, format, sha256, size |
| `preview/light_<id>.jpg` | JPEG | live view pipeline | ephemeral, regenerable |
| session log | JSONL of Event | event bus sink | one file per session; rotation not needed (bounded by session) |
| `<queue_dir>/transfer.db` | SQLite (WAL) | transfer agent | §5.10.1 schema; references frames by path, never copies them |
| `stacking-server.yaml` | YAML | operator | PRD §8.2; deny-unknown-fields |
| stack `sessions/<sid>/…` | mirror of the field layout | ingest service | structurally identical to the field node's, asserted by a shared fixture test (§5.11.3) |
| stack `ingest.db` | SQLite (WAL) | ingest service | received frames, source, timestamps; future authority for REL-13 |

The field node carries exactly **one** SQLite database (the transfer journal) and the stack node
one (the ingest journal). Both are single-writer with WAL, per ADR-06 and the contention risk in
ADD §10. Everything else on either node stays human-readable and travels with the data.

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
| transfer agent | tokio task (field) | `transfer.db`, one in-flight upload | bus in (`frame.saved`), HTTP out, bus out (`transfer.acked`) |
| ingest handler | tokio tasks (stack) | `ingest.db` (single writer), session mirror | HTTP in, worker job queue out |
| worker supervisor | tokio task (stack) | child process handles | stdio pipes, job queue in, bus out |
| compute worker | OS process (Python, stack) | its own memory/GPU context | stdio IPC only (§5.12); crash isolated from the backbone |

**Runtime sizing.** The threads above are not free, and the field node may be a 4-core Pi. Left at
its default the tokio runtime takes one worker per core, and then the camera OS thread, the decode
pool (2–3), and the solver subprocess all compete for the same cores — producing exactly the
latency jitter this topology exists to prevent. Both binaries therefore size the runtime
explicitly from config (`server.runtime_worker_threads`, PRD §8.1/§8.2):

| Node | Default | Reasoning |
|------|---------|-----------|
| Field | `min(2, cores - 2)`, floor 1, when unset | The async work is I/O-bound and light — a serial poll, WS fan-out, HTTP handlers. Reserve cores for the camera thread and the decode pool, which are the ones that actually saturate a CPU |
| Stack | one per core when unset | The backbone is I/O-bound too, but the heavy compute lives in child processes with their own scheduling; there is nothing to reserve against |

An operator on larger field hardware raises it; the point is that the number is a decision with a
reason, not an accident of `num_cpus`. The chosen value is reported in `/api/system/info` so a
support question about sluggishness can be answered from the API rather than by guesswork.

Shutdown order (SIGTERM): stop accepting API → abort live view → if capturing, finish download (bounded 120 s) → stop tracking? **No** — tracking state is left as-is (an operator restart of the service mid-session must not stop the mount) → flush session log → exit. This asymmetry (finish camera, don't touch mount) is deliberate: the mount is safe while tracking; a half-downloaded frame is a lost frame.

---

## 8. Design of Cross-Cutting Mechanisms

### 8.1 Startup sequence

config load+validate → auth check (§4.5) → frame store open/create session → registry builds drivers (no connect) → safety wrapper → API up (health returns `starting`) → watchdogs on → health `ok`. Hardware connect is always an explicit operator action (or `--auto-connect` flag for fixed installations) — matching "startup to first capture < 60 s" (PRD §12) without surprise motion on boot.

### 8.2 Route metadata

Every route registers `RouteMeta { tier: Tier, audit: bool, command: CommandClass }` via a typed layer. Phase 2c's confirmation middleware and the LLM tool generator (ADD §6.1) consume the same declarations — the invariant "one declaration drives both" is established now.

`command` is M1-T10's, and it is a **second axis rather than a derivation of the tier**. Tier answers "how much consequence does this carry, and should a human confirm it"; `command` answers "does refusing a delayed one make the observatory safer or less safe". They disagree on most rows: `/api/mount/slew/stop` is `low` and `Stopping`, `/api/mount/park` is `high` and `MotionInitiating`, `/api/mount/unpark` is `high` and `Neutral`. Deriving one from the other would make one of them wrong wherever they differ.

The layer that publishes the declaration is also the layer that **enforces** the envelope (§5.8.1), and that placement is the requirement rather than an implementation choice: the classification must be visible in one table, reviewed in the diff that adds the route, and unreachable from the handler — a handler that could see its own class could exempt itself, on a rule whose whole purpose is that a client cannot. The router type accepts no route without a `RouteMeta`, so there is no path that mounts a handler undeclared.

### 8.3 Remote-link latency mitigations

Consolidated design position on operating over a slow/lossy VPN. The first two are architectural (ADD) and restated here for completeness; the rest are binding on the designs in this document:

1. **Edge autonomy** — deadline-bearing loops (guiding, watchdogs, limits, sequence execution) close on the field node; the link carries intents, not actuation (ADD §5.4.4, REL-09, PRF-03).
2. **Intent-based commands** — goto/track/sequence are goals executed under local supervision; nothing requires a sustained command stream to remain safe.
3. **Leases for continuous motion** — the slew dead-man's switch (§5.8.1): silence means stop.
4. **Staleness rejection + idempotency** — `issued_at`/`command_id` envelope (§5.8.1): late starts are refused, late stops are always honored, retries are idempotent. Carried in headers, classified per route by §8.2's `CommandClass`, and bounded — the ledger is per process, ~1024 entries, 5 min, and holds only successful outcomes.
5. **Connection separation** — safety/control traffic never shares a TCP stream with bulk image data: `/ws` (JSON events) and `/ws/liveview` (binary frames) are distinct sockets, and the e-stop POST uses the browser's separate HTTP connection pool with `keepalive`. A 500 KB JPEG retransmit can therefore never head-of-line-block a stop command or a position update.
6. **Coalescing telemetry** — latest-only delivery for self-superseding state (§5.8.3); the UI shows the present or a marked prediction, never a replayed past.
7. **Transfer pacing** *(binding rule on the §5.10 transfer agent; config keys exist and validate from M1, enforcement lands with Phase 2b — see §5.10.4)* — the frame uploader must (a) enforce a configurable bandwidth cap, and (b) automatically yield: while any operator motion command is active or was issued within the last 10 s, uploads throttle to a configured interactive floor (default 20% of cap). Prevents self-inflicted bufferbloat where a 25 MB CR3 upload queues the operator's commands behind it in the tunnel.
8. **Predictive display + link-health surfacing** — the PWA dead-reckons between updates and displays RTT/telemetry age (§5.9); degradation is explicit, never silent (PRF-01, USB-11).

---

## 9. Verification Design (M0–M3)

| ID | Test design | Verifies |
|----|-------------|----------|
| T-COD-1 | Golden-vector unit tests for SyntaCodec (encode/decode u24, framing) incl. vectors captured from EQMOD logs | §5.2.2, protocol risk |
| T-POS-1 | Property tests: counts↔coordinates round-trip within 1 count; hemisphere/pier cases table-driven | §5.2.3 |
| T-SER-1 | Serial task against a mock port: timeout, retry, garbled response, lane priority under load | §5.2.4, REL-02 |
| T-SER-3 | E-stop latency: request injected during 50-cmd/s normal load; assert bytes-on-wire ≤ 20 ms | PRF-12 budget |
| T-SLW-1 | Slew TTL: start manual slew, silently drop renewals (simulated link loss); assert axis stop within ttl_ms + 100 ms and `SLEW_TTL_EXPIRED` alert emitted | §5.8.1 dead-man's switch |
| T-STALE-1 | Command staleness: goto with `issued_at` 5 s old → `COMMAND_STALE`, no motion; slew/stop with same age → executed; duplicate `command_id` → original outcome returned, no re-execution | §5.8.1 staleness/idempotency |
| T-HOL-1 | Connection separation: saturate `/ws/liveview` with frames over a shaped 1 Mbit link; assert `/ws` position events and e-stop POST latency unaffected (≤ 2× baseline) | §8.3(5) |
| T-ISO-1 | **Thread isolation — the PRF-04 test.** While a capture + 32 MB download runs (simulator configured with a realistic ~2 s blocking capture and a slow download), assert concurrently: `mount.position` events keep 1 Hz cadence with no gap > 1.5 s; `/api/mount/position` and `/api/system/health` p99 latency stays ≤ 2× the idle baseline; an e-stop issued mid-download still meets its ≤ 20 ms handler-to-wire budget; the event bus never lags a subscriber. Repeat with a decode job saturating the blocking pool. **Fails if any single-threaded assumption creeps back in** — this is the regression guard, not a one-off measurement | PRF-04, PRF-01, §5.3.1, §7 |
| T-CAM-1 | Camera thread against gphoto2 vusb/simulator: capture, settings, timeout-wedge recovery respawn | §5.3.1, REL-03 |
| T-E2E-1 | Full API-level two-node session against simulator drivers: connect → goto → capture → frame durable → transferred → acked → stub-worker preview returns through the proxy; assert event stream shape | IMP M1 exit criteria |
| T-DUR-1 | Kill -9 during download / during meta write; on restart assert no partial frame visible, no ID reuse | §5.3.2, §5.5, REL-04/05 |
| T-XFER-1 | Transfer durability: kill the stack node mid-session (queue grows, capture unaffected, one offline alert not thousands); restart it (queue drains in order, every frame acked exactly once); kill the *field* node mid-upload (row returns to `queued`, frame re-uploaded, journal intact) | §5.10, REL-06/07/13, ARC-11 |
| T-ING-1 | Ingest contract: bit-flipped upload → `CHECKSUM_MISMATCH`, nothing stored, tmp cleaned; duplicate `(frame_id, sha256)` → `duplicate: true`, one file on disk; same id different sha → `FRAME_ID_CONFLICT`, original untouched; below critical disk → 507 | §5.11.2, REL-11/12, IPP-15 |
| T-IPC-1 | Worker protocol and supervision: golden-message fixture asserted against **both** the Rust enums and `workers/astroctl_ipc.py`; version mismatch → clean refusal, no retry, no hang; `kill -9` mid-job → restart, job retried once, disruption < 10 s; job that always kills the worker → failed with alert, no restart loop | §5.12, ADR-13, ARC-22 |
| T-HIL-1 | Hardware-in-loop checklist (real HEQ5 + R10): handshake values vs. EQMOD reference, low-speed slews first, bulb prototype — **first powered milestone, gates Phase 1 completion** | §5.2, §5.3, ADD §10 risks |
| T-SOAK-1 | 8 h simulator soak: 1 Hz polling + capture every 60 s; assert memory flat ≤ 512 MB steady (PRF-05), no task death | PRF-05, robustness |

Simulators (HAL-11) are first-class: `SimulatorMount` implements realistic slew ramps, settle, and configurable fault injection (timeouts, garbled frames) — fault injection is a constructor parameter so T-SER/T-E2E tests express failure scenarios declaratively.

## 10. Requirements Traceability (M0–M3 elements)

| Requirement | Design element |
|-------------|----------------|
| HAL-01..07 | §5.1 traits + registry |
| HAL-08 | §5.1 probe design |
| HAL-11 | §9 simulators |
| MNT-01..08 | §5.2 driver; §5.4 wrapper; §5.8 routes |
| MNT-12 | §5.1 `guide_pulse` incl. rate; §5.2.2 opcode `P` |
| MNT-15/16 | §5.4 SafeMount |
| CAM-01..05 (05 basic), CAM-06, CAM-08 | §5.3, §5.7, §5.8 |
| IPP-04, IPP-09/10 (Phase 1 subset) | §5.7, §5.5, §6 |
| SES-07 (basic) | §4.3 bus → session log sink |
| ARC-01..05, ARC-07 | §2, §3, §4.4, §5.8.3 |
| REL-01..05, REL-11, REL-12, REL-14 | §5.2.4, §5.3.2, §5.4, §5.5 |
| PRF-01, PRF-05, PRF-12 | §5.2.4, §5.3.1, §7, §5.8.2, T-SOAK-1/T-SER-3 |
| **PRF-04** | §5.3.1 (camera on its own OS thread), §5.4 (bounded blocking pool), §7 (runtime sizing) — **verified by T-ISO-1**, not inferred from the topology |
| SEC-01/02 (subset) | §4.5 |
| USB-03/04/09/10/12 | §5.9 |
| STK-16, STK-17, ARC-11, REL-06, REL-13 (marking) | §5.10 transfer agent |
| STK-18, STK-19, STK-20, ARC-08/13, ADR-05/07 | §5.11 routes + §5.8.1 `/stack/*` proxy |
| IPP-15, REL-07, REL-11/12 (stack side) | §5.11.2, §5.11.3 |
| ARC-22, CMP-06 (worker-side fallback path), ADR-13 | §5.12 IPC + supervision |
| USB-06 | `stack.status` / `transfer.status` topics (§4.3), stack panel (§5.9) |

Requirements of later phases trace at architecture level via ADD §11 and will be detailed in the SDD increments of §1.2. Note that §5.10–5.12 design the *skeleton* these requirements need in M1; the stacking mathematics behind STK-01..15 and the reclaim mechanics of REL-13 arrive with the Phase 2b increment.

---

*Verification note (12207 §6.4.5.3(c)): each design element above names its governing requirement IDs; the Phase 1 exit review walks this table against T-E2E-1/T-HIL-1 results.*
