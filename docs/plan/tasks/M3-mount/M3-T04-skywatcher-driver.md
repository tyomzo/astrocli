# M3-T04 — SkywatcherMount driver assembly

**Milestone:** M3 · **Depends on:** M3-T02, M3-T03 · **Crates:** astroctl-drivers
**Size:** L · **Status:** not started
**Spec:** SDD §5.2.1 (layering), §5.2.3 (goto supervision); PRD MNT-01..08, SES-06

## Objective

Assemble codec + serial task + motor controllers into a `MountDevice` implementation that
drops into the slot `SimulatorMount` occupies — API, SafeMount, UI all unchanged.

## Scope

- `SkywatcherMount` implementing the full trait: connect (port open + handshake both axes), position (1 Hz poll mapping through mech_to_sky), goto (target set, high-speed mode, 2 Hz completion polling, stopped-within-tolerance detection, tracking restore per SES-06), tracking modes, manual slew speeds, guide pulses, park/unpark (goto to configured park position + stop), emergency stop (both axes `L` via priority lane)
- **Pre-motion readback verification** (SDD §5.2.2): after `G`/`I`/`H`/`M` and before `J`, read `:h`/`:m`/`:i` and assert they match the intended absolute target, break point and step period. Measured exact on hardware. Abort with a protocol error on mismatch — never send `J` against unverified registers. ~48 ms, and it is the only check that catches an encoding fault before the motors move
- `emergency_stop` path: verifiably lock-free from trait call to priority-lane send (no awaiting normal-lane state)
- Status synthesis: `MountStatus` from axis status decodes
- Capabilities per PRD §4.2 values (position_resolution from handshake)
- Full-driver test against the mock port: scripted handshake + goto + estop session; FaultPlan-equivalent scenarios reusing T02's mock
- Registry name `"skywatcher"`

## Acceptance criteria

- [ ] Complete simulated session against mock port: connect → track → goto (completion detected) → estop, byte-stream asserted against expected command sequence
- [ ] Swap test: M1's T-E2E-1 suite runs green with `skywatcher` driver + mock port (proving the contract swap)
- [ ] E-stop issued during goto polling reaches the mock wire ≤ 20 ms (T-SER-3 conditions)
- [ ] Pre-motion readback rejects a deliberately corrupted `H` payload before `J` is sent (inject at the codec layer, assert no motion command reaches the mock wire)
