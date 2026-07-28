# M1 — Walking Skeleton

**Goal:** the complete system shape, demoable from a phone over VPN: full GUI, two-node
orchestration with real frame flow (capture → save → transfer → ack → preview back), all
devices simulated, safety mechanisms real. No real hardware, no stacking math.

**Exit criteria (IMP §2/M1, the demo):**
- Connect simulated mount + camera from the PWA; slew with predicted/confirmed position display
- Capture a synthetic frame; watch save → transfer → stack ack → preview return into the UI
- Kill the stack node mid-session: capture continues, queue grows, reconnect drains it
- E-stop halts a simulated slew instantly; slew TTL stops motion when renewals stop
- Gated tests green: T-E2E-1, T-SLW-1, T-STALE-1, T-HOL-1, T-DUR-1, T-XFER-1, T-ING-1, T-IPC-1

## Tasks and order

Three tracks (A: field backbone, B: stack backbone, C: PWA) can run in parallel;
within a track, order matters.

| Task | Title | Track | Depends on |
|------|-------|-------|-----------|
| M1-T01 | HAL traits, capabilities, registry | A | M0 |
| M1-T02 | SimulatorMount with fault injection | A | T01 |
| M1-T03 | Mount facade, routes, position streaming | A | T02 |
| M1-T04 | PWA foundation: WS store, snapshot, mount panel | C | T03 (API contract only) |
| M1-T05 | SafeMount: limits, slew TTL, e-stop, alt/az | A+C | T03, T04 |
| M1-T06 | SimulatorCamera + SimulatorGuideCamera: synthetic star fields | A | T01 |
| M1-T07 | Frame store: sessions, durability, ID reservation | A | M0 |
| M1-T08 | Capture flow, camera routes, PWA camera panel | A+C | T06, T07 |
| M1-T09 | Live view pipeline, /ws/liveview, preview panel | A+C | T08 |
| M1-T10 | Command envelope: staleness + idempotency | A+C | T05, T08 |
| M1-T11 | Transfer agent (field side) | A | T07 |
| M1-T12 | Stack ingest + session mirror | B | M0 |
| M1-T13 | Worker IPC crate, supervisor, stub Python worker | B | M0 |
| M1-T14 | Preview flow end-to-end + stack panel | B+C | T11, T12, T13 |
| M1-T15 | Predictive display + link-health surfacing | C | T04 |
| M1-T16 | E2E suite, fault injection, session log, demo script | all | all |

Suggested PR sequence keeping the tree demoable: T01→T02→T03 (+T04 parallel) →T05→
T06/T07→T08→T09→T10→T11/T12/T13→T14→T15→T16 (IMP §4).
