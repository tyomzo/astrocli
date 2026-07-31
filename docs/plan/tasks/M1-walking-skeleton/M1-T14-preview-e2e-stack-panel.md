# M1-T14 — Preview flow end-to-end + stack panel

**Milestone:** M1 · **Track:** B+C · **Depends on:** M1-T11, T12, T13 · **Crates:** astroctl-stack, astroctl-field, frontend/
**Size:** L · **Status:** done
**Spec:** SDD §5.9 — the tablet sketch and the four-slot table define where the stack view sits, what M1 fills, and what 2b/2c drop into. ADD §5.2.2 (Preview + Export skeleton), ADR-07 (proxy); PRD STK-19, USB-06; SDD §8.3(5)

## Objective

Close the two-node loop the demo is built on: ingested frame → stub worker preview → stack WS
→ field proxy → operator's screen.

## Scope

- On successful ingest: submit preview job (T13); on result, push JPEG over stack WS `/ws` (binary preview topic on stack side mirrors field's liveview socket pattern — separate binary socket `/ws/preview`)
- Field proxy: extend `/stack/*` HTTP proxy with WS proxying for the stack sockets (operator keeps single origin — ADR-07); auth forwarded
- Stack status into field UI: field polls/subscribes stack health + stats, republishes as `stack.status` events (connected, queue depth from T11, frame count, last preview ts — USB-06)
- **PWA stack view as a primary destination** (SDD §5.9), not a status strip: `FRAME` and `STACK` are two sources sharing **one image surface**, with the app switching to `STACK` when a capture sequence starts. Shows connection state, transfer queue depth and oldest age, frame count, last-preview age, and the preview itself at full size
- **No knobs in M1** — the stub worker does no stacking, so there is nothing to tune. Build the panel so Phase 2b's controls (method, rejection, stretch — IPP-07) drop into a reserved region rather than forcing a re-layout; the same slot discipline as the target region
- **Reserve the rebuilding state now even though nothing triggers it yet.** From 2b, IPP-16 re-stacks in the background while the preview keeps serving the *pre-rebuild* image, so a knob change correctly produces no visible effect for a while — and looks like a bug. The panel needs a rebuilding indicator with progress; designing it in later means discovering the problem as a support question
- Single-machine mode verified: both binaries on one host, loopback config (STK-20 degenerate case)

## PR split

1. Stack side: preview job on successful ingest, `/ws/preview` binary push
2. Field side: WS proxying for the stack sockets, `stack.status` republishing
3. PWA stack panel + single-machine (loopback) mode verification

## Acceptance criteria

- [ ] Demo path: capture on field → preview from *stack* appears in PWA ≤ 10 s (sim timing), traversing proxy only (browser talks to field origin exclusively — assert via network log)
- [ ] Stack down: panel shows disconnected + growing queue; stack up: drains, previews resume — no PWA reload needed
- [ ] WS proxying survives stack restart (reconnect through proxy, REL-10 behavior)
