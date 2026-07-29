# T-HIL-1 step 2 FINDINGS — read-only Synta handshake

**Date:** 2026-07-29 · **Mount:** Sky-Watcher HEQ5 Pro · **Interface:** Pegasus Astro EQDIR Stick
(FTDI FT232R, `0403:6001`) → `/dev/ttyUSB0`, 9600 8N1 · **Method:** `probe.py`, inquiry opcodes
only (`e`, `a`, `b`, `j`, `f`). No motion opcode was sent; the script refuses to emit one.

## Headline: PRD §4.2's timer frequency was wrong by a factor of 7

| Parameter | PRD §4.2 assumed | Mount reports | Verdict |
|-----------|-----------------|---------------|---------|
| Counts per revolution | ~9,024,000 | **9,024,000** (both axes) | exact match |
| Timer interrupt frequency | ~460,800 Hz | **64,935 Hz** (both axes) | **wrong — corrected** |
| Counter home | `0x800000` | `0x800000` (both axes read exactly home) | confirmed |

The decode is not in doubt. The same byte-swap rule (`00B289` → `0x89B200`) that yields the CPR
*exactly* right yields 64,935 for the timer field. If 460,800 were correct it would have arrived as
`000807`; it arrived as `A7FD00`. And 460,800 ÷ 64,935 = 7.0963 — not a clean factor, so this was a
wrong constant rather than a unit slip.

PRD §4.2 carried these as "typical HEQ5 Pro values — verify against the EQMOD source before
implementation, per the protocol-documentation risk." The verification was worth doing.

### How much damage it would have done — honestly

Less than the risk register implies, and the reason is a design decision worth crediting.
SDD §5.2.3 reads CPR and timer frequency **from the handshake**, and M3-T03 says explicitly
"never hardcoded 9,024,000 — that's a test fixture value only." So the running driver would have
used the mount's real values regardless, and no goto would have been driven at 7× the intended
rate.

What the wrong constant *would* have poisoned is everything built around the driver: M3-T03's
acceptance criterion is "step periods for all rates match hand-computed values for fixture
CPR/timer-freq", and hand-computing against 460,800 produces expected values that are wrong by
7×. The likely failure mode is not a runaway mount — it is a test that fails, gets "corrected" to
match the bad arithmetic, and thereafter certifies the wrong thing. Documentation errors of this
shape are dangerous precisely because they look authoritative.

## Other results

**Firmware version** — RA returns `020401`. Interpretation deliberately not asserted here: the
Synta version encoding is field-ordered and PRD §4.2 requires the EQMOD cross-check for semantics.
Recorded raw so M3-T01 can pin it once the reference is consulted.

**Axis status** — both axes return `100`. Consistent with uninitialised axes, which is expected:
`F` (initialise) was never sent.

**Position counters** — both axes read exactly `0x800000`, i.e. dead on home. Consistent with a
mount freshly powered and not yet moved.

**Serial round-trip** — 20 × `:j1`: min 14.4 ms, median 16.5 ms, max 16.6 ms. Remarkably tight.
Two design consequences:

- SDD §5.2.4 budgets a 500 ms per-request timeout — roughly 30× headroom. Fine, but the value is
  now known to be very conservative rather than guessed.
- More usefully, §5.2.4 assumes an in-flight normal request completes in "≤ ~50 ms" before the
  priority lane can proceed. Measured worst case is **16.6 ms**, so e-stop's exposure to a
  mid-flight normal command is a third of the design assumption. The 20 ms handler-to-wire budget
  (budget (a) of ADD §9.1) remains achievable with room to spare.
- 1 Hz polling at ~16 ms per exchange is a 1.6% duty cycle on the wire. Serial contention is a
  non-issue at Phase 1 rates.

## Golden vectors — now `verified`, not `derived`

M3-T01 says to mark vectors `derived` where no EQMOD trace exists and upgrade them at bring-up.
These are real request/response pairs from the actual mount and can start as `verified`:

```
:e1\r  ->  =020401\r      firmware version, RA
:a1\r  ->  =00B289\r      CPR       = 9,024,000
:b1\r  ->  =A7FD00\r      timer f   = 64,935
:j1\r  ->  =000080\r      position  = 0x800000 (home)
:f1\r  ->  =100\r         axis status, uninitialised
:a2\r  ->  =00B289\r      CPR, DEC
:b2\r  ->  =A7FD00\r      timer f, DEC
:j2\r  ->  =000080\r      position, DEC
:f2\r  ->  =100\r         axis status, DEC
```

Framing confirmed as documented: `:` + opcode + axis + `\r`, response `=` + payload + `\r`.
No `!` error frame was provoked, so the error path remains unverified.

## Still open — everything that moves

Steps 3–6 of T-HIL-1 were **not** run and must not be until their prerequisites are green:
low-speed slews per axis, e-stop latency measured on the wire, TTL expiry against real motors,
tracking-rate verification, overnight poll soak, goto accuracy, park/unpark, and the altitude and
meridian limit demonstrations. Those need T-COD-1 passing, the clutches loose, and an operator at
the mount.

Also unverified: the `!` error-frame path, `F` axis initialisation behaviour, and the high-speed
ratio (PRD §4.2 assumes ~16× — untested, and now under suspicion given the timer-frequency result).
