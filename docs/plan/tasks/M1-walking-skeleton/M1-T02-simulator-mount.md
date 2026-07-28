# M1-T02 — SimulatorMount with fault injection

**Milestone:** M1 · **Track:** A · **Depends on:** M1-T01 · **Crates:** astroctl-drivers
**Size:** M · **Status:** not started
**Spec:** PRD §4.5 (SimulatorMount), HAL-11; SDD §9 (fault injection as constructor param)

## Objective

A `MountDevice` implementation with realistic *timing* behavior — the simulator's fidelity is
what makes the M3 hardware swap boring.

## Scope

- Position model: two axes, tracking advances RA at selected rate; slews with trapezoidal ramp (accel → cruise per `SlewSpeed` → decel), configurable settle oscillation after stop
- `goto`: async completes when motion + settle done; concurrent goto → `DeviceError::Busy`
- Guide pulses displace position by rate×duration; park/unpark to configured position
- Optional drift + periodic error terms (config) for later pipeline testing
- `FaultPlan` constructor parameter: scripted faults — `TimeoutOnce(cmd)`, `GarbledResponse(n)`, `DisconnectAfter(duration)`, `StallDuringSlew` — consumed declaratively by tests
- Emits nothing itself: pure device; facade does events (T03)

## Acceptance criteria

- [ ] Goto of 30° completes in a plausible duration for the speed profile (assert bounds, not exact)
- [ ] Position monotonic + continuous during slew (sampled at 10 Hz, no jumps)
- [ ] Each `FaultPlan` variant provably triggers its `DeviceError` and, where scripted, recovery
- [ ] Behind `simulator` feature flag; registry name `"simulator"`
