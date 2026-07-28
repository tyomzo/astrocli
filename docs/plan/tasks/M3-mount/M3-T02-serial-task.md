# M3-T02 — Serial task: lanes, timeout, heartbeat

**Milestone:** M3 · **Depends on:** M3-T01 · **Crates:** astroctl-drivers
**Spec:** SDD §5.2.4 (two-lane queue, timings); PRD REL-02, PRF-12; MNT-01 (port autodetect)
**Tests gated:** T-SER-1, T-SER-3

## Objective

The single owner of the serial port: request/response with retry, the priority lane that
makes e-stop real, and the heartbeat that makes link loss detectable.

## Scope

- Task owning `serialport` handle; two mpsc lanes (Normal/Priority) with biased drain per SDD §5.2.4 — in-flight normal completes, no new normal starts while priority pending
- Per-request timeout 500 ms → one retry → `DeviceError::Timeout`; garbled response (codec error) counts as failure; all timings config-overridable
- Heartbeat: piggybacks the 1 Hz position poll; 3 consecutive failures → `HeartbeatLost` notification to the watchdog channel (SafeMount M1-T05 seam)
- Port autodetect: scan `/dev/ttyUSB*`/`/dev/serial/by-id/*` filtering known USB VID/PIDs (PL2303/FTDI/CH340), probe with version inquiry; manual port config bypasses scan
- Mock port test double (in-crate): scriptable responses, delays, garbage, dead-air — used by all gated tests

## Acceptance criteria

- [ ] T-SER-1: timeout, retry-then-fail, garbled-response, reconnect scenarios green against mock
- [ ] T-SER-3: priority request injected under 50 cmd/s normal load reaches mock wire ≤ 20 ms (measured over 1000 iterations, p99)
- [ ] Heartbeat loss fires after exactly 3 misses and recovers cleanly when responses resume
