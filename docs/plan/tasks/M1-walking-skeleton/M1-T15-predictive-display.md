# M1-T15 — Predictive display + link-health surfacing

**Milestone:** M1 · **Track:** C · **Depends on:** M1-T04 · **Crates:** frontend/
**Size:** M · **Status:** done
**Spec:** SDD §5.9 (affordances paragraph), §5.8.3 (ping/ts), §8.3(8)

## Objective

The operator always knows how fresh their picture is, and the position display feels live
even at 1 Hz updates over a slow link.

## Scope

- Link health: WS ping loop → RTT estimate; telemetry age = now − last event `ts` (skew-corrected via T10's offset); header indicator green/amber(>500 ms RTT or >3 s age)/red(disconnected) with numeric readout on tap
- Predictive position: between `mount.position` events dead-reckon displayed RA/DEC from last confirmed value + known state (tracking: RA advances at sidereal/lunar/solar rate; slewing: hold last + "in motion…" treatment; idle: static); predicted values rendered in the "aging" style (e.g. reduced opacity + tilde), snapping to confirmed on each event
- Prediction guardrail: beyond 5 s without an event, stop predicting — show stale marker instead (never fabricate beyond one expected update gap ×5)
- Applies to mount panel + header coordinate readout

## Acceptance criteria

- [x] Throttle WS artificially to 0.2 Hz: displayed RA advances smoothly ~~while tracking~~ **while
  idle**, visually marked as predicted, snaps on real updates
- [x] Cut the link: within 5 s display switches to stale marker, header red; restore: recovers
  without reload
- [ ] Style distinction between confirmed/predicted/stale passes a squint test on a phone in dark
  mode — **outstanding, and only a human can close it.** Forcing `data-mode="night"` in Chrome at
  412 px is the cheapest available evidence and it says the right thing (every hue collapses to one
  red, and the strike-through is the only channel still carrying the message), but a rendered
  screenshot is not a phone at arm's length outdoors.

**The first criterion names the wrong case, and the correction is the finding.** It says a
*tracking* mount's RA advances. It does not: §5.2.3 recovers `RA = LST − HA`, so tracking at the
sidereal rate holds RA still — that is what tracking is — and it is the **idle** mount whose RA
climbs, at 1.00274 h per hour. Both were driven over the throttled link: idle advanced 2.0 s of RA
per 2.0 s of clock and kept advancing between reports; switching tracking on over the same link
held it. SDD §5.9 corrected in the same change (v1.22.0).

## Evidence

Chrome over the field node on 8470, with the event socket proxied so frames could be dropped
without the app or the node being aware:

| step | observed |
|------|----------|
| idle, 1 Hz | RA advances 2.0 s of RA / 2.0 s clock, unmarked |
| idle, throttled to 0.2 Hz | keeps advancing between reports, `~` marked, snaps onto each report |
| tracking on, same link | RA holds |
| frames dropped, socket left open | struck through at 5.1 s of age; header amber `3 ms · 10.3 s` |
| upgrade refused | header red at +5.3 s, still showing the age |
| restored | recovered in 1.6 s, no reload |

The `3 ms · 10.3 s` row is worth keeping: it is the case where the two thresholds disagree — pings
answered promptly, no events at all — which is what a hub dropping a slow consumer (§5.8.3) looks
like from the phone.
