# HEQ5 motion experiment series — design

**Rig configuration:** bare mount head. No OTA, no counterweight, no payload, no cables beyond
power and the EQDIR stick. This is the safest configuration the mount will ever be in, and it is
the right one for aggressive protocol validation — there is nothing to swing into a tripod leg and
nothing expensive to break. What remains at risk is the drivetrain itself, so the ordering below
still earns its keep.

**Governing constants** (all hardware-verified, `FINDINGS.md`):

| | |
|---|---|
| Counts per revolution | 9,024,000 → **25,066.67 counts/degree** |
| Timer interrupt frequency | **64,935 Hz** (was documented as 460,800 — wrong by 7.1×) |
| High-speed ratio | 16× |
| Counter home | `0x800000` = 8,388,608 |
| Sidereal rate | **104.7304 counts/s** = CPR ÷ 86164.0905 s |

---

## Phase 0 — desk work. HARD GATE, no hardware.

**Nothing in this plan may run until the action-opcode encodings are derived from the EQMOD
source and written down.** We have verified thirteen *inquiry* opcodes empirically. We have never
encoded a single *action* opcode, and the one that matters most — `G`, set motion mode — packs
direction and speed class into a byte whose layout we do not know.

Guessing the `G` encoding is precisely how a low-speed test becomes a high-speed slew. Trial and
error is acceptable for a read-only sweep; it is not acceptable for the byte that selects speed.

**Status: COMPLETE** — see `ENCODINGS.md`. `G` confirmed against three independent sources, `f`
status bits decoded and validated against our own capture, and two corrections found: the goto uses
`H` (relative increment), not `S` (absolute), and `M` (break-point increment) was missing from the
design entirely. The `I` step-period relation remains a hypothesis that E8 tests.

## The software fence

Every motion experiment runs behind a position fence, independent of the experiment's own logic:

- Sample `:j` on the moving axis continuously (~16 ms round trip, measured).
- If the counter departs the start position by more than **`fence` counts**, immediately send `L`
  (instant stop) on both axes, then `K`, then abort the run.
- Default fence: **20,000 counts ≈ 0.8°**. Generous for every experiment below, tight enough that
  a runaway is caught quickly.

**Honest limitation:** the fence reacts at poll rate, so it overshoots by roughly one round trip
of travel. At sidereal that is ~1.7 counts. At a 500× slew it is ~840 counts. This is exactly why
high-speed work is last — the fence is a real backstop at low speed and a coarse one at high
speed. It is not a substitute for the bounded-motion discipline below.

**Global abort criteria** — cut power, do not debug in place:

- Counters moving with no command outstanding
- Motion in the opposite direction to the one commanded
- An axis status bit pattern not in the Phase 0 table
- Any motion continuing more than 2 s after `K` and `L` have both been sent
- Audible grinding, or the motor stalling and buzzing

---

## Phase 1 — initialise and status decoding

### E1 · `F` initialise, both axes
**Question:** does `F` move anything, and what does it do to `:f`?
**Procedure:** read `:j` and `:f` on both axes, decoding `:f` per the Phase 0 bit table. Send `F` to axis 1 only. Re-read both. Repeat for axis 2.
**Prediction:** no counter movement; `:f` changes from the at-rest `100` to an initialised pattern.
**Abort:** any counter movement at all — `F` is not supposed to be a motion command.
**Feeds:** SDD §5.2.2 status decoding; the connect handshake in M3-T04.

### E2 · status bits across states
**Question:** which bits of `:f` mean initialised, moving, direction, speed class?
**Procedure:** sample `:f` at rest, after `F`, during each later motion experiment, and after stop. Build the truth table incrementally as the series runs.
**Feeds:** slew-complete detection (SDD §5.2.3 declares a goto complete "when both axes report stopped") — currently the design's most load-bearing undecoded field.

---

## Phase 2 — first motion, bounded, stop NOT yet trusted

### E3 · smallest self-terminating goto
**Question:** do `G`/`I`/`H`/`M`/`J` encode correctly, does the mount move, and does it stop *itself*?
**Procedure:** axis 1 only. `G` = `"20"` (GOTO, low speed, forward) → `I` step period → `H` increment = **+1,000 counts** (**0.0399° ≈ 2.4 arcmin**) → `M` break-point → `J` start. **Do not send `K`.** Note the goto target is a *relative increment*, not an absolute position (Phase 0 correction). Poll `:j` at max rate throughout. Let the mount terminate on its own.
**Prediction:** axis advances 1,000 counts and stops without further command.
**Records:** direction sign, elapsed time, final counter vs target (the goto error), the `:f` pattern while moving.
**Abort:** fence at 20,000 counts.
**Why first:** the endpoint lives in the mount's firmware, not our code. If everything we sent is wrong except `J`, the motion still ends.

### E4 · repeat on axis 2
Same, DEC. Direction conventions frequently differ between axes.

---

## Phase 3 — prove the stop path, with its failure disarmed

### E5 · `K` mid-travel
**Question:** does the ramped stop work, and how much does it overshoot?
**Procedure:** identical bounded goto to E3, but send `K` at roughly half travel. Record the counter at the moment `K` was written and where motion actually ceased.
**Prediction:** motion ends short of target; overshoot is the ramp-down distance.
**The point:** if `K` does nothing, the mount still stops at the target. You learn the stop path is broken without paying for it. **This is the experiment that converts `K` from assumed to verified.**

### E6 · `L` mid-travel
Same, with instant stop. **Record the overshoot difference between `K` and `L`** — that difference is the physical meaning of the e-stop path and belongs in the PRF-12 discussion.

### E7 · `K` and `L` with both axes moving
Two simultaneous bounded gotos, stop both. Confirms per-axis addressing under concurrent motion.

---

## Phase 4 — the rate formula. The highest-value experiment in the series.

### E8 · sidereal step period
**Question:** is the corrected timer frequency actually right, and is the step-period formula correct?

This closes the loop on the constant we changed. The predicted step period for sidereal tracking is

```
step_period = timer_freq / rate = 64,935 / 104.7304 = 620.0
```

**The old, wrong constant predicts 4,400 for the same rate** — so this experiment discriminates
sharply between them.

**Procedure:** `I` = 620 on axis 1, tracking mode, `J`. Poll `:j` for 60 s. Compute counts/second.
**Prediction:** **104.73 ± 0.5 counts/s.** If instead you measure ~743 counts/s (7.1× fast), then
64,935 is wrong and everything downstream must stop until it is resolved.
**Feeds:** M3-T03's speed math, and final confirmation of PRD §4.2.

### E9 · rate linearity
Repeat E8 at step periods 310, 620, 1240, 2480. Rate should scale inversely and linearly. Confirms the formula rather than a single lucky point.

---

## Phase 5 — characterisation (unbounded motion now permissible)

`K` is proven by Phase 3, so open-ended slews are acceptable from here.

### E10 · slew speeds
Each speed class from `G`, low to high, each axis, each direction. Measure actual counts/s per class. **Run the high-speed classes last**, and note the fence is coarse at those rates.

### E11 · backlash
Move + 5,000 counts, stop, settle, reverse. Count the commanded steps before the counter starts changing. That dead zone is the backlash figure — it feeds guiding (Phase 3) and goto accuracy now.

### E12 · goto accuracy vs distance
Gotos of 1,000 / 10,000 / 100,000 / 1,000,000 counts, both directions, five repeats each. Record final error. **Feeds the `tolerance` value in SDD §5.2.3**, which is currently a guessed "default 10 counts".

### E13 · do `d`, `h`, `r`, `m` track the goto target?
Read them before and after `H`/`M`. Phase 0 makes this sharper: `H` and `M` are the goto-increment and break-point setters, so lowercase `h` and `m` are plausibly their readbacks. The survey found them sitting at or near home, suggesting target/breakpoint registers. If confirmed, the driver gains a way to **read back what a goto was programmed with before `J` is ever sent** — a pre-motion safety check the design does not currently have.

### E14 · guide pulses
`P` sets the autoguide rate; issue a pulse and measure displacement in counts. Validates MNT-12 and the `GuideRate` newtype added in SDD §5.1.

---

## Phase 6 — e-stop latency on the wire

### E15 · PRF-12 budget (b)
**Question:** how long from API call to motion actually ceasing?
**Procedure:** requires `scripts/synta-sniff` from T-HIL-1 step 1 — polling `:j` cannot resolve this, since the 16 ms round trip *is* the measurement floor. Timestamp bytes on the wire, and separately timestamp the last changing counter value.
**Budget:** ADD §9.1 (b) — ≤ 100 ms from API call to motion ceasing, which adds 9600-baud transmission and motor response on top of the ≤ 20 ms handler-to-wire figure verified in CI.

---

## Phase 7 — endurance

### E16 · tracking soak
Sidereal on axis 1 for 8 hours with 1 Hz polling. Record drift from predicted counts, comms errors, timeouts. Compare against the 2000-exchange baseline (zero failures, 14.7–17.2 ms).

### E17 · repeated goto cycling
200 alternating gotos. Watch for cumulative position error, thermal effects, comms degradation.

---

## What this series does not cover

Meridian and altitude limits (T-HIL-1 step 6) need real geometry and a payload, so they stay with
the full bring-up. Park and unpark need a defined park position. Anything requiring the sky is
Phase 2a. Physical USB-unplug recovery still needs a hand on the cable.

## Feedback into the documents

E1/E2 → SDD §5.2.2 status decoding · E5/E6 → `K`/`L` overshoot, PRF-12 · **E8 → final
confirmation of PRD §4.2** · E10 → speed-class table · E11 → backlash constant, new · E12 → SDD
§5.2.3 goto tolerance · E13 → possible new pre-motion safety check · E14 → MNT-12 · E15 → ADD
§9.1 budget (b) · E16 → REL-02 timeout and heartbeat tuning.
