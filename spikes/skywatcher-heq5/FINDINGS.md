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

---

# T-SER-4 FINDINGS — read-only protocol surface and resilience

**Date:** 2026-07-29 · Same rig · `survey.py`, lowercase opcodes only. The write gate rejects any
uppercase byte on the raw stream, so no misaligned frame can form an action opcode; every probe is
bracketed by a both-axis position read. **Final motion check: counters never moved.**

## The last §4.2 unknown is closed

`:g1` and `:g2` both return `'10'` — two hex characters, not six, so no byte-swap applies:
0x10 = **16**. PRD §4.2's assumed ~16× high-speed ratio is **confirmed**. Note the width: the
codec must not apply the u24 byte-swap rule to this field.

## The command table is less than half the real surface

SDD §5.2.2 documents five inquiries (`e a b j f`). The mount supports **thirteen**:

| Opcode | Response | Decoded (byte-swapped u24) | Status |
|--------|----------|---------------------------|--------|
| `a` | `00B289` | 9,024,000 | documented — CPR |
| `b` | `A7FD00` | 64,935 | documented — timer frequency |
| `c` | `008800` | 34,816 | **undocumented** |
| `d` | `000080` | 8,388,608 — exactly home | **undocumented** |
| `e` | `020401` | — | documented — firmware version |
| `f` | `100` | — (3 chars, status bits) | documented — axis status |
| `g` | `10` | 16 (2 chars, plain hex) | **undocumented** — high-speed ratio |
| `h` | `000080` | 8,388,608 — exactly home | **undocumented** |
| `i` | `A787F6` | 16,156,583 | **undocumented** |
| `j` | `000080` | 8,388,608 | documented — position |
| `m` | `66C59F` | 10,470,758 | **undocumented** |
| `r` | `AE0080` | 8,388,782 (home + 174) | **undocumented** |
| `s` | `1C0501` | 66,844 | **undocumented** |

`k l n o p q t u v w x y z` all return `!`. Notably `:q` errors, so this firmware has no extended
status block.

`d` and `h` reading exactly home, and `r` reading home + 174, suggest target/breakpoint registers —
if so they give the driver a way to *read back* what a goto was actually programmed with, which is
directly useful for verifying `S` before `J` is ever sent. Semantics must come from the EQMOD
source (PRD §4.2's standing requirement); this survey establishes only that they exist and what
they return at rest.

## The mount does not validate the axis digit

`:j9` returned `=000080` — a valid position response for a nonexistent axis 9. The device accepts
any digit and answers anyway. **The codec must validate the axis itself**, because the mount will
not reject a corrupted digit; it will silently return plausible data. This is a strong argument
for the typed command layer M3-T01 already specifies (`GetPosition(Axis)` rather than a free
string).

## Real error frames — three distinct codes

T-COD-1 previously had no genuine error sample. Now:

| Sent | Response | Meaning |
|------|----------|---------|
| `:z1`, `:y1` (undefined opcode) | `!0` | unknown command |
| `:j` (missing axis digit) | `!1` | missing/invalid parameter |
| `:` (bare colon) | `!3` | malformed frame |

Framing confirmed as `!` + single digit + `\r`.

## Framing resilience — better than assumed

| Case | Behaviour |
|------|-----------|
| Truncated frame, no `\r` | No response; times out at ~400 ms. **Does not wedge** — the next well-formed command succeeds |
| Junk prefix `zzz:j1\r` | `=000080` — **resyncs on `:`**, junk discarded |
| Two frames in one write | `=0000=000080` — **response stream corrupts** |

The last one matters. Back-to-back frames produce an interleaved, unparseable reply. SDD §5.2.4
already specifies strict single request-response with no pipelining; this is the hardware evidence
for why that is not merely tidy but required.

## Timing — the design's assumptions were conservative, and are now measured

2000 consecutive `:j1` exchanges: **0 timeouts, 0 malformed**, min 14.7 ms, p50 15.8 ms,
p99 16.9 ms, max 17.2 ms, sustained 62.5 exchanges/s.

- 2.5 ms total spread across 2000 samples — the link is extremely predictable.
- The 500 ms per-request timeout (§5.2.4) is ~29× the observed maximum.
- **The heartbeat threshold is validated.** §5.2.4 fires the watchdog after 3 consecutive
  failures; against a zero-failure baseline over 2000 exchanges, 3 consecutive misses is an
  unambiguous signal of real trouble rather than noise.
- 1 Hz polling is a 1.6% duty cycle; even 2 Hz goto monitoring leaves ~30× headroom.

Port close and reopen: 38 ms, first exchange succeeds. Half of REL-03 confirmed; the physical
unplug case still needs a hand on the cable.

## Still open

Everything requiring motion (T-HIL-1 steps 3–6), the semantics of the seven undocumented
inquiries, the `F` initialise path, and physical-unplug recovery.
