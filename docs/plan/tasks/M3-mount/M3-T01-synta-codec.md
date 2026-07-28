# M3-T01 — Synta codec + golden vectors

**Milestone:** M3 · **Depends on:** M1 · **Crates:** astroctl-drivers (skywatcher module)
**Spec:** SDD §5.2.2 (framing, byte-swapped hex, command table); PRD §4.2
**Tests gated:** T-COD-1

## Objective

Pure, exhaustively tested protocol encoding/decoding — zero I/O. Every byte the mount will
ever see or send is producible and parseable here first.

## Scope

- Frame encode: `:` + cmd + axis + payload + `\r`; decode: `=data\r` / `!err\r` with error-code enum
- `encode_u24`/`decode_u24` with the byte-swap quirk (0x123456 ↔ "563412"); also u16/u8 variants used by some commands
- Typed command layer: one type per SDD §5.2.2 table row (`GetPosition(Axis)`, `SetGotoTarget(Axis, Counts)`, …) with encode + typed response parse; motion-mode byte semantics (direction/speed-class bits) as documented enums
- **Golden vectors**: capture or transcribe EQMOD/indi-eqmod traces into `testdata/synta_vectors.txt` (raw request/response byte pairs with meaning); table-driven test over all vectors. Where a trace is unavailable, mark the vector `derived` — T05 step 2 upgrades these to `verified`
- Fuzz: decoder must never panic on arbitrary bytes (cargo-fuzz target or proptest)

## Acceptance criteria

- [ ] T-COD-1 green: all vectors round-trip; byte-swap explicitly covered incl. asymmetric digits
- [ ] Every command the driver will use (SDD table) has a typed constructor + at least one vector
- [ ] Fuzz run (≥ 1 M iterations locally, shorter in CI) with zero panics
- [ ] `derived` vs `verified` vector counts reported in the task result (feeds T05)
