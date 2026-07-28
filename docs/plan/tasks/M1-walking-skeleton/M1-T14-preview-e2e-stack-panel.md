# M1-T14 — Preview flow end-to-end + stack panel

**Milestone:** M1 · **Track:** B+C · **Depends on:** M1-T11, T12, T13 · **Crates:** astroctl-stack, astroctl-field, frontend/
**Spec:** ADD §5.2.2 (Preview + Export skeleton), ADR-07 (proxy); PRD STK-19, USB-06; SDD §8.3(5)

## Objective

Close the two-node loop the demo is built on: ingested frame → stub worker preview → stack WS
→ field proxy → operator's screen.

## Scope

- On successful ingest: submit preview job (T13); on result, push JPEG over stack WS `/ws` (binary preview topic on stack side mirrors field's liveview socket pattern — separate binary socket `/ws/preview`)
- Field proxy: extend `/stack/*` HTTP proxy with WS proxying for the stack sockets (operator keeps single origin — ADR-07); auth forwarded
- Stack status into field UI: field polls/subscribes stack health + stats, republishes as `stack.status` events (connected, queue depth from T11, frame count, last preview ts — USB-06)
- PWA stack panel: connection state, transfer queue depth + oldest age, last preview image with session frame count overlay
- Single-machine mode verified: both binaries on one host, loopback config (STK-20 degenerate case)

## Acceptance criteria

- [ ] Demo path: capture on field → preview from *stack* appears in PWA ≤ 10 s (sim timing), traversing proxy only (browser talks to field origin exclusively — assert via network log)
- [ ] Stack down: panel shows disconnected + growing queue; stack up: drains, previews resume — no PWA reload needed
- [ ] WS proxying survives stack restart (reconnect through proxy, REL-10 behavior)
