# M3-T04 — SkywatcherMount driver assembly

**Milestone:** M3 · **Depends on:** M3-T02, M3-T03 · **Crates:** astroctl-drivers
**Size:** L · **Status:** done
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

- [x] Complete simulated session against mock port: connect → track → goto (completion detected) → estop, byte-stream asserted against expected command sequence
      — `tests/mount_driver.rs::a_complete_session_connects_tracks_gotos_and_stops_with_the_frames_sdd_5_2_mandates`
- [x] Swap test proving the contract swap — **at the wrapper rather than in the containers**.
      `astroctl-field`'s `the_safety_wrapper_drives_the_skywatcher_driver_exactly_as_it_drives_the_simulator`
      drives connect → track → status → position → a limit-refused goto → e-stop through `SafeMount`
      over the mock port, and `the_shipped_example_config_builds_the_mount_driver_it_names` closes
      the config → registry → driver path with the example unmodified. Running T-E2E-1 itself would
      mean putting a scriptable serial port inside the shipped binary, which this crate's own rules
      forbid; what the containers add over the in-process suite is the camera and the stack, neither
      of which the mount driver touches.
- [x] E-stop issued during goto polling reaches the mock wire ≤ 20 ms (T-SER-3 conditions)
      — `tests/mount_driver.rs::an_emergency_stop_during_the_completion_poll_reaches_the_wire_inside_the_budget`,
      and `mount.rs::an_emergency_stop_reaches_the_wire_while_a_normal_exchange_is_wedged` for the
      harder case: a normal exchange with 450 ms of its own timeout still to run.
- [x] Pre-motion readback rejects a deliberately corrupted `H` payload before `J` is sent
      — `tests/mount_driver.rs::a_corrupted_goto_register_stops_the_motion_before_any_motor_is_commanded`

## What this task changed in the design

SDD 1.28.0: §5.2.1 and §5.2.3 both corrected. LST is injected and the *binary* wires it (ADD §5.6
rule 1 puts the workspace's one implementation out of this crate's reach); the goto's completion
poll, settle and tracking restore run in a driver-owned task because §5.8.1 drops the caller's
future on every goto; tracking is restored *before* the settle, not after; "stopped within
tolerance" needed its converse and a first-poll guard; the speed class is per axis; and `f` cannot
tell tracking from a manual slew, so `mount.status` comes from the driver's own record of intent.
