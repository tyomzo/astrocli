# Phase 0 — action-opcode encodings, derived from reference sources

Desk work, no hardware. This is the gate the motion plan sits behind: we had verified thirteen
*inquiry* opcodes empirically but never encoded a single *action* opcode, and `G` packs direction
and speed class into a byte whose layout we did not know.

**Confidence is stated per item.** The timer-frequency episode is the reason — a single
authoritative-looking source is not evidence.

## Sources

| # | Source | Nature |
|---|--------|--------|
| S1 | [Sky-Watcher official protocol wiki](https://github.com/skywatcher-pacific/skywatcher_open/wiki/Skywatcher-Protocol) | vendor primary, incomplete |
| S2 | [INDI `indi-eqmod/skywatcher.h`](https://raw.githubusercontent.com/indilib/indi-3rdparty/master/indi-eqmod/skywatcher.h) | working implementation, opcode enum |
| S3 | [INDI `indi-eqmod/skywatcher.cpp`](https://raw.githubusercontent.com/indilib/indi-3rdparty/master/indi-eqmod/skywatcher.cpp) | working implementation, call sequences |
| S4 | Our own captures (`FINDINGS.md`) | the actual HEQ5 on the bench |

## `G` SetMotionMode — CONFIRMED, three independent sources

Two characters: `:G<axis><mode><dir>\r`

| char 0 | meaning |
|--------|---------|
| `0` | GOTO, high speed |
| `1` | SLEW, low speed |
| `2` | GOTO, low speed |
| `3` | SLEW, high speed |

| char 1 | meaning |
|--------|---------|
| `0` | forward |
| `1` | backward |

S1, S2 and S3 agree exactly. **`2` is the mode for every first motion experiment** — GOTO (so it
self-terminates) at low speed. Note the counterintuitive packing: `0` is *high* speed and `1` is
*low*, so a transposed digit is a 16× speed error, not a direction error. This is why the encoding
had to come from a reference rather than a guess.

## `f` GetAxisStatus — CONFIRMED against our own hardware

Reply `=<n1><n2><n3>\r`. Bit tests apply directly to the ASCII characters, which works because the
status nibbles never exceed 7 and ASCII `0`–`7` carry their value in the low three bits.

| Field | Test | Meaning |
|-------|------|---------|
| slew mode | `n1 & 0x01` | 1 = SLEW, 0 = GOTO |
| direction | `n1 & 0x02` | 1 = backward, 0 = forward |
| speed mode | `n1 & 0x04` | 1 = high, 0 = low |
| running | `n2 & 0x01` | 1 = axis in motion |
| initialised | `n3 & 0x01` | 1 = `F` has been issued |

**Validated against S4.** Our mount returned `=100` on both axes: not initialised (we never sent
`F`), not running (at rest). The reference predicts exactly that. This is the field
slew-complete detection depends on, and it is now decoded rather than guessed.

## The GOTO sequence — and two corrections to SDD §5.2.2

Per S1 and S3, a goto is:

```
G  set motion mode          e.g. "20" = GOTO, low speed, forward
I  set step period          rate control
H  set goto target INCREMENT   <-- relative, not absolute
M  set break point increment   <-- deceleration point
J  start motion
```

**Correction 1 — the design has the wrong goto opcode.** SDD §5.2.2 lists `S` as "Set goto target
(absolute counts)". S1 does not list `S` at all, and S3 shows INDI performing gotos exclusively
through `H`, a *relative increment*. Relative is also the safer primitive: you state a delta
directly, so an arithmetic slip cannot fling the mount across the sky the way a bad absolute
target can.

**Correction 2 — `M` is missing entirely.** SetBreakPointIncrement sets where deceleration begins
and is part of every goto. It is absent from the SDD table.

Also absent and worth recording: `E` SetAxisPosition, `O` SetSwitch, `U` SetBreakSteps (S1).

## Lower-confidence items

**`I` SetStepPeriod → rate.** This is the least-confirmed piece. The simplest plausible relation is
`period = timer_freq / counts_per_second`, giving **620** for sidereal at 64,935 Hz. S3's
`SetRARate` uses a worm-based expression rather than this form, and the fetched excerpt did not
show the conversion to interrupt units. **E8 in the motion plan is designed to settle this
empirically** — set 620, measure the rate, and compare against the predicted 104.73 counts/s. Treat
620 as a hypothesis to be tested, not a derived constant.

**`P` SetST4GuideRate.** S3 shows a single character `'0'`–`'9'` denoting a rate magnitude, not a
computed value. Sufficient to attempt E14; semantics of each level unconfirmed.

**`F` Initialize.** Confirmed as the initialise opcode (S1, S2). Whether it energises or moves
anything is untested — E1 exists precisely to find out, with a position guard.

**The undocumented inquiries.** S1 explains two of our seven: `g` = InquireHighSpeedRatio (matches
our measured 16) and `s` = InquirePECPeriod (explains `:s1` = 66,844). It does not list `c`, `d`,
`h`, `i`, `m` or `r`. Given `H` and `M` are goto-increment and break-point setters, lowercase `h`
and `m` are plausibly their readbacks — which is exactly what **E13** tests, and if true gives the
driver a pre-motion check it currently lacks.

## A caution about one source

The automated read of S3 reported "SetMotionMode ('M')" and "SetBreakPointIncrement ('B')", which
contradicts the opcode enum in S2 (`SetMotionMode = 'G'`, `SetBreakPointIncrement = 'M'`) and
contradicts S1. The enum is authoritative and the letters above follow it. Flagged rather than
silently reconciled, because a transposed opcode letter is precisely the class of error this whole
exercise exists to catch — and anyone re-deriving this should verify against the header directly.

## Gate status

**Sufficient to proceed** to motion Phases 1–3 (`F`, `G`, `H`, `M`, `J`, `K`, `L`), which is
everything up to and including proving the stop path. Phase 4 onward depends on the `I` step-period
relation, which E8 resolves. E14 needs `P` semantics, which remain approximate.
