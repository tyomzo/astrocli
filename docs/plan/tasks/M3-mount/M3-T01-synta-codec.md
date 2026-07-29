# M3-T01 — Synta codec + golden vectors

**Milestone:** M3 · **Depends on:** M1 · **Crates:** astroctl-drivers (skywatcher module)
**Size:** M · **Status:** not started
**Spec:** SDD §5.2.2 (framing, byte-swapped hex, command table); PRD §4.2
**Tests gated:** T-COD-1

## Objective

Pure, exhaustively tested protocol encoding/decoding — zero I/O. Every byte the mount will
ever see or send is producible and parseable here first.

## Scope

- Frame encode: `:` + cmd + axis + payload + `\r`; decode: `=data\r` / `!err\r` with error-code enum
- `encode_u24`/`decode_u24` with the byte-swap quirk (0x123456 ↔ "563412"); also u16/u8 variants used by some commands
- Typed command layer: one type per SDD §5.2.2 table row (`GetPosition(Axis)`, `SetGotoTarget(Axis, Counts)`, …) with encode + typed response parse; motion-mode byte semantics (direction/speed-class bits) as documented enums
- **Golden vectors**: `spikes/skywatcher-heq5/FINDINGS.md` already contains **nine `verified` pairs read from the operator's own HEQ5** (handshake, CPR, timer freq, position, axis status, both axes) — seed `testdata/synta_vectors.txt` with those, then extend from EQMOD/indi-eqmod traces. Mark anything without a real trace `derived`; T05 step 2 upgrades the rest
- The `!` error frame **is** now covered by real captures: `!0` unknown command (`:z1`), `!1` missing/invalid parameter (`:j`), `!3` malformed frame (`:`) — all `verified`
- **Width is not uniform**: `:g` returns 2 hex chars and `:f` returns 3; only the u24 fields are byte-swapped. A single "decode payload" path that assumes 6 chars will silently mis-decode both
- **Validate the axis digit in the codec.** The mount does not: `:j9` returns a well-formed response for a nonexistent axis. Typed constructors (`GetPosition(Axis)`) are the mechanism; a test must assert an invalid axis is rejected before transmission
- Fuzz: decoder must never panic on arbitrary bytes (cargo-fuzz target or proptest)

## Acceptance criteria

- [ ] T-COD-1 green: all vectors round-trip; byte-swap explicitly covered incl. asymmetric digits
- [ ] Every command the driver will use (SDD table) has a typed constructor + at least one vector
- [ ] Fuzz run (≥ 1 M iterations locally, shorter in CI) with zero panics
- [ ] `derived` vs `verified` vector counts reported in the task result (feeds T05)
- [ ] Decoder reproduces the captured pairs exactly: `00B289` → 9,024,000 and `A7FD00` → 64,935 (the byte-swap rule is confirmed by the first, which makes the second trustworthy)
