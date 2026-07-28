# M1-T05 — SafeMount: limits, slew TTL, e-stop

**Milestone:** M1 · **Track:** A · **Depends on:** M1-T03 · **Crates:** astroctl-safety, astroctl-field
**Spec:** SDD §5.4, §5.8.1 (dead-man paragraph), §5.8.2; PRD MNT-08/15/16, REL-01, PRF-12; ADD ADR-11
**Tests gated:** T-SLW-1

## Objective

Safety enforcement below the API: altitude/meridian limits for every caller, the slew TTL
dead-man's switch, and the e-stop path. After this task the facade the API sees **is** SafeMount.

## Scope

- `SafeMount` implementing `MountDevice`, wrapping the inner device; installed in T03's facade
- Altitude limit: goto/slew target check (alt computation stub: use a simple spherical formula locally until Phase 2a planning lands — mark with a TODO referencing SDD §5.2.3's mech_to_sky note); rejection → 403 `LIMIT_ALTITUDE` envelope
- Continuous check task at 2 Hz during manual slew; meridian watch → tracking stop + alert (MNT-16)
- Slew TTL: `ttl_ms` param (default 500, clamp 2000), renewal on identical repeat, expiry → axis stop + `SLEW_TTL_EXPIRED` alert
- E-stop: `/api/mount/estop` dedicated route (auth-only middleware, empty body OK) → `emergency_stop()` forwarded ungated; wire PWA header button live
- Watchdog task skeleton: 1 Hz tick — disk free (REL-12 warn threshold), clock-sync stub; serial heartbeat slot reserved for M3

## Acceptance criteria

- [ ] T-SLW-1: renewals silently dropped → axis stopped within ttl+100 ms, alert emitted
- [ ] Goto below `min_altitude_degrees` → 403 `LIMIT_ALTITUDE`, mount never commanded (assert via simulator command log)
- [ ] E-stop during simulated slew: motion stops immediately; route responds even while a goto is mid-flight
- [ ] All limit/TTL behavior driven purely by config values (no constants in code)
