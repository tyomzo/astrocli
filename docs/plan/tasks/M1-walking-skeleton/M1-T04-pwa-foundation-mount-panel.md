# M1-T04 — PWA foundation: WS store, snapshot, mount panel

**Milestone:** M1 · **Track:** C · **Depends on:** M1-T03 (contract; can develop against a mock server) · **Crates:** frontend/
**Size:** L · **Status:** not started
**Spec:** SDD §5.9; PRD USB-01/03/04/05/08/12

## Objective

The PWA's state architecture (WS-fed store, snapshot resync, auto-reconnect) plus the first
real screen: the mount panel.

## Scope

- WS client: connect `/ws` with token, apply snapshot, reduce events into a typed store; auto-reconnect with backoff + resnapshot (REL-10); connection state exposed to UI
- Command layer: REST wrapper attaching auth; UI state changes **only** from events (no optimistic mutation) — enforce via store design
- Header: connection badges for mount/camera/stack (USB-04), reserved e-stop button slot wired to `/api/mount/estop` (route exists after T05; button present now, disabled until then)
- Mount panel: coordinates in astronomical notation (USB-05), tracking mode control, goto form with validation, D-pad with press-and-hold slew (TTL renewal comes alive in T05 — implement send-repeat loop now against the plain slew route), speed selector; 44 px targets, dark theme baseline
- Responsive: single-column phone, panel grid tablet (USB-08)

## Acceptance criteria

- [ ] Kill and restart the field binary mid-view: UI shows disconnect, reconnects, resnapshots, no stale state rendered
- [ ] All mount interactions round-trip: command → event → UI change (verify no direct state writes in code review of store)
- [ ] Usable on a phone-sized viewport in browser dev tools and on a real device over VPN
