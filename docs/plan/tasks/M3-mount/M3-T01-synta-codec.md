# M3-T01 — Synta codec + golden vectors

**Milestone:** M3 · **Depends on:** M1 · **Crates:** astroctl-drivers (skywatcher module)
**Size:** M · **Status:** done
**Spec:** SDD §5.2.2 (framing, byte-swapped hex, command table); PRD §4.2
**Tests gated:** T-COD-1

## Objective

Pure, exhaustively tested protocol encoding/decoding — zero I/O. Every byte the mount will
ever see or send is producible and parseable here first.

## Scope

- Frame encode: `:` + cmd + axis + payload + `\r`; decode: `=data\r` / `!err\r` with error-code enum
- `encode_u24`/`decode_u24` with the byte-swap quirk (0x123456 ↔ "563412"); also u16/u8 variants used by some commands
- Typed command layer: one type per SDD §5.2.2 table row (`GetPosition(Axis)`, `SetGotoIncrement(Axis, Counts)`, `SetBreakPointIncrement(Axis, Counts)`, …) with encode + typed response parse. **The goto target is a relative increment (`H`), not an absolute position** — the protocol has no absolute-target opcode; see `spikes/skywatcher-heq5/ENCODINGS.md`
- **Motion mode (`G`) is the highest-risk encoding in the protocol** and must be a typed enum, never an integer formatted into a string. The packing is counterintuitive — mode `0` is GOTO *high* speed and `1` is SLEW *low* speed — so a transposed digit is a 16× speed error, not a direction error. Encode as `MotionMode { slew_or_goto, speed_class, direction }` with a table-driven test covering all eight combinations against the reference values
- `f` status decode per the ENCODINGS.md bit table, with `=100` (uninitialised, at rest) as a verified fixture from the real mount
- **Golden vectors**: `spikes/skywatcher-heq5/FINDINGS.md` already contains **nine `verified` pairs read from the operator's own HEQ5** (handshake, CPR, timer freq, position, axis status, both axes) — seed `testdata/synta_vectors.txt` with those, then extend from EQMOD/indi-eqmod traces. Mark anything without a real trace `derived`; T05 step 2 upgrades the rest
- The `!` error frame **is** now covered by real captures: `!0` unknown command (`:z1`), `!1` missing/invalid parameter (`:j`), `!3` malformed frame (`:`) — all `verified`
- **Width is not uniform**: `:g` returns 2 hex chars and `:f` returns 3; only the u24 fields are byte-swapped. A single "decode payload" path that assumes 6 chars will silently mis-decode both
- **Validate the axis digit in the codec.** The mount does not: `:j9` returns a well-formed response for a nonexistent axis. Typed constructors (`GetPosition(Axis)`) are the mechanism; a test must assert an invalid axis is rejected before transmission
- Fuzz: decoder must never panic on arbitrary bytes (cargo-fuzz target or proptest)

## Acceptance criteria

- [x] T-COD-1 green: all vectors round-trip; byte-swap explicitly covered incl. asymmetric digits
- [x] Every command the driver will use (SDD table) has a typed constructor + at least one vector
- [x] Fuzz run (≥ 1 M iterations locally, shorter in CI) with zero panics
- [x] `derived` vs `verified` vector counts reported in the task result (feeds T05)
- [x] Decoder reproduces the captured pairs exactly: `00B289` → 9,024,000 and `A7FD00` → 64,935 (the byte-swap rule is confirmed by the first, which makes the second trustworthy)

## Result

`crates/astroctl-drivers/src/skywatcher/codec/`, behind a new dependency-free `skywatcher`
feature. 58 unit tests plus 10 `t_cod_1_*` integration tests; `cargo test t_cod_1` is exactly the
gate. The 24-bit domain is walked **exhaustively** (all 16,777,216 values) rather than sampled,
which is affordable precisely because the codec has no I/O. Fuzz: 10 M iterations locally with
zero panics, 250 k on every push (`ASTROCTL_FUZZ_ITERS` raises it), plus exhaustive sweeps over
every 0/1/2-byte buffer and every 4-byte frame from the protocol alphabet.

**Vector tally: 26 `verified`, 52 `derived`** (`testdata/synta_vectors.txt`; the header states the
split and a test fails if it drifts from the rows). What M3-T05 must upgrade, in priority order:

1. **The action acknowledgements.** `:F1`/`:F2` are the only actions whose reply bytes FINDINGS
   quotes. `G I H M J K L P` were all sent to the mount and accepted, but the ack bytes were not
   recorded, so 13 rows are `derived` on a technicality that one `log()` call would close.
2. **The four high-speed motion modes** (`:G100`, `:G101`, `:G130`, `:G131`) and `status=…,high,…`.
   High-speed classes remain entirely unrun on hardware.
3. **The E14 readback triple.** The *relationships* are measured exactly; the wire strings are
   reconstructed from the decimal values FINDINGS quotes via the verified byte-swap rule.
4. **`!2` and `!4`**, the two vendor error codes with real operational meaning (motor not stopped,
   axis not initialised). Both are provokable on purpose in a few seconds.
5. **The firmware-version split.** `firmware=02.04 model=HEQ5` is derived from `indi-eqmod`; its
   only corroboration is that the model code names the mount that produced the capture.
6. **The 24-bit counter wrap.** Assumed modulo 2^24; nothing has driven a counter near `0xFFFFFF`.

Five §5.2.2 corrections came out of the implementation — see SDD change note 1.27.0.
