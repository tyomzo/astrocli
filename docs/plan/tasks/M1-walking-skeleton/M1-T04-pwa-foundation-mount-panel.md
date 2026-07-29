# M1-T04 — PWA foundation: WS store, snapshot, mount panel

**Milestone:** M1 · **Track:** C · **Depends on:** M1-T03 (contract; can develop against a mock server) · **Crates:** frontend/
**Size:** L · **Status:** not started
**Spec:** SDD §5.9; PRD USB-01/03/04/05/08/12

## Objective

The PWA's state architecture (WS-fed store, snapshot resync, auto-reconnect) plus the first
real screen: the mount panel.

## Scope

- WS client: **fetch a fresh ws-ticket before every connection attempt** (SDD §4.5 — a browser cannot send a bearer header on the upgrade, and tickets are single-use so a reconnect needs a new one), apply snapshot, reduce events into a typed store; auto-reconnect with backoff + resnapshot (REL-10); connection state exposed to UI. A 401 from the ticket endpoint means the token is bad — surface that rather than retrying forever
- Command layer: REST wrapper attaching auth; UI state changes **only** from events (no optimistic mutation) — enforce via store design
- Header: connection badges for mount/camera/stack (USB-04), reserved e-stop button slot wired to `/api/mount/estop` (route exists after T05; button present now, disabled until then)
- **Target region as a slot** (SDD §5.9): in M1 it holds manual RA/DEC entry with validation plus the current pointing readout. Phase 2a's catalog picker (PLN-03/04) replaces the entry box *inside this slot* — so structure it as `<TargetRegion>` owning a swappable chooser, not as a form that the rest of the layout is built around. It changes how a target is chosen, not what anything else does with one
- **D-pad overlays the image surface**, not a sibling panel (SDD §5.9): nudging is framing, so the control and its effect must share one field of view. Press-and-hold slew with the send-repeat loop (TTL renewal comes alive in T05 — implement against the plain slew route now), speed selector, 60–70 px targets
- `⊕nudge` affordance permanently present but compact, **auto-expanding when a slew completes**. Contextual by default, always summonable — never make the operator hunt for manual control
- Coordinates in astronomical notation (USB-05), tracking mode control; dark theme over the M0-T06 token layer
- Responsive: single-column phone, panel grid tablet (USB-08)

## Acceptance criteria

- [ ] Kill and restart the field binary mid-view: UI shows disconnect, reconnects, resnapshots, no stale state rendered
- [ ] All mount interactions round-trip: command → event → UI change (verify no direct state writes in code review of store)
- [ ] Usable on a phone-sized viewport in browser dev tools and on a real device over VPN
