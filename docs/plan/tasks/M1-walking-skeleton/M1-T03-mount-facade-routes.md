# M1-T03 — Mount facade, routes, position streaming

**Milestone:** M1 · **Track:** A · **Depends on:** M1-T02 · **Crates:** astroctl-field
**Size:** L · **Status:** not started
**Spec:** SDD §5.8.1 route table (mount rows), §5.8.3 WS hub, §4.3 topics; PRD MNT-01..07

## Objective

The mount is drivable over authenticated REST and observable at 1 Hz on `/ws` — the first
vertical slice from HTTP to (simulated) hardware.

## Scope

- Mount facade task: owns `Arc<dyn MountDevice>`, runs the 1 Hz position poll → `mount.position` events, `mount.status` on change. The `alt`/`az` fields are `null` **only until M1-T05 lands** — SafeMount already computes topocentric altitude for the limit check, and once it wraps this facade it populates both fields from the same helper. Do not describe this as a Phase 2a gap: MNT-03 is a Phase 1 Must and is met at M1 exit; Phase 2a replaces the approximate transform with the erfa one, it does not introduce it
- Routes per SDD table: connect/disconnect, position, status, goto (202 + correlation ID + progress events), tracking, slew, slew/stop, park/unpark — request validation into core types, error envelope mapping
- **`POST /api/auth/ws-ticket` and ticket validation on the WS upgrade** (SDD §4.5). A browser cannot send `Authorization` on a WebSocket handshake, so this is the *only* way `/ws` is authenticated — single-use, 30 s TTL, ≥128 bits of server-generated randomness, bounded store with aggressive expiry. Consume the ticket on upgrade; a replay must be rejected
- WS hub `/ws`: JSON events, per-client bounded queue with latest-only coalescing for `mount.position`, topic subscribe messages, snapshot-on-connect (current mount/camera/system state)
- Long-running action pattern: goto returns `202 {correlation_id, watch_topic}`

Out of scope: safety wrapper (T05 wraps this facade), staleness envelope (T10), e-stop route (T05).

## Acceptance criteria

- [ ] Scripted `curl` session: connect → goto → position stream shows motion → tracking on — all against SimulatorMount
- [ ] WS client receives snapshot first, then 1 Hz positions; slow client gets coalesced positions but no dropped discrete events (test with artificial stall)
- [ ] Two concurrent goto requests: second → 409 `Busy` envelope
- [ ] WS upgrade without a ticket → rejected; with a valid ticket → accepted; **replaying the same ticket → rejected**; a ticket older than its TTL → rejected
