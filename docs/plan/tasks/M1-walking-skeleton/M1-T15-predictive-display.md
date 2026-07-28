# M1-T15 — Predictive display + link-health surfacing

**Milestone:** M1 · **Track:** C · **Depends on:** M1-T04 · **Crates:** frontend/
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

- [ ] Throttle WS artificially to 0.2 Hz: displayed RA advances smoothly while tracking, visually marked as predicted, snaps on real updates
- [ ] Cut the link: within 5 s display switches to stale marker, header red; restore: recovers without reload
- [ ] Style distinction between confirmed/predicted/stale passes a squint test on a phone in dark mode
