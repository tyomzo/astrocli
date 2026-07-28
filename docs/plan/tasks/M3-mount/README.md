# M3 — Real Mount (Sky-Watcher HEQ5 Pro)

**Goal:** replace `SimulatorMount` with `SkywatcherMount`. Software tasks (T01–T04) are
CI-testable against mock ports and golden vectors; T05 is the scripted hardware bring-up.

**Exit criteria (IMP §2/M3):** PRD Phase 1 exit criteria on real hardware; T-HIL-1 checklist
executed and archived.

**Safety framing:** the mount can damage itself and its payload. No task powers motors
before its listed prerequisites are green. The bring-up script (T05) is ordered from
read-only to full motion deliberately — do not reorder it. Opcode semantics MUST be
verified against the EQMOD source before first powered command (PRD §4.2 note).

## Tasks and order

| Task | Title | Depends on | CI-able |
|------|-------|-----------|---------|
| M3-T01 | Synta codec + golden vectors | M1 | yes |
| M3-T02 | Serial task: lanes, timeout, heartbeat | T01 | yes (mock port) |
| M3-T03 | Motor controller + position math | T01 | yes |
| M3-T04 | SkywatcherMount driver assembly | T02, T03 | yes (mock) |
| M3-T05 | HIL bring-up (hardware, operator present) | T04 + M2 complete | no |
