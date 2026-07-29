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

### E5 · step-period relation, measured inside a bounded goto — **PREMISE FALSIFIED, see FINDINGS.md**
> **Executed and disproved.** GOTO ignores the step period — a 10× change left the rate unchanged
> at 5,350 counts/s. The rate can only be measured in SLEW mode (E10), which is unbounded and
> therefore does genuinely require `K` proven first. The original Phase 4 placement was correct;
> this restructure was wrong and the hardware said so. E10 subsequently confirmed the timer
> frequency at 0.11% agreement.

**This was originally Phase 4 and has been moved forward.** It does not need unbounded tracking,
and therefore does not need `K` proven — a goto self-terminates, so the highest-value measurement
in the series can happen before we rely on the stop path at all.

**Question:** is the corrected timer frequency right, and is `rate = timer_freq / step_period`?

**Procedure:** `G` = `"20"`, `I` = **620**, `H` = **+6,000 counts** (0.239°), `M`, `J`. Poll `:j`
throughout at full rate (~62/s). Then:

1. Discard the first and last ~15% of samples — those are the accel and decel ramps.
2. Linear-fit the middle. The slope is the commanded rate in counts/s.
3. `measured_timer_freq = 620 × slope`.

**Prediction — and it discriminates without needing the fit:**

| | rate at period 620 | 6,000-count goto takes |
|---|---|---|
| timer_freq = **64,935** (ours) | 104.73 counts/s | **57.3 s** |
| timer_freq = 460,800 (old) | 743.23 counts/s | **8.1 s** |

A stopwatch separates those. The fit then gives the precise figure: ~3,580 samples at 1.68 counts
each makes the slope estimate very tight.

**Worth noting:** 64,935 ÷ 620 = 104.7339 counts/s against a true sidereal rate of 104.7304 — a
0.003% match. Synta almost certainly chose the timer frequency so that sidereal lands on the round
step period 620, which is independent circumstantial support for 64,935 being correct. E5 is what
turns that from a suggestive coincidence into a measurement.

**Abort:** fence at 20,000 counts.
**Feeds:** M3-T03 speed math; final confirmation of PRD §4.2.

### E6 · rate linearity, also bounded
Repeat E5 at step periods 310, 1240 and 2480, adjusting the increment so each run lasts roughly a
minute. Rate should scale inversely and linearly. Confirms the *formula*, not just one lucky point.

**Residual uncertainty, stated honestly:** E5 and E6 measure the rate in **GOTO** mode. Tracking
uses **SLEW** mode, and the two could interpret the step period differently. The 7× constant
question is settled either way — that difference is far too large to be a mode artefact — but the
precise tracking rate needs the slew-mode confirmation in Phase 4.

---

## Phase 3 — prove the stop path, with its failure disarmed

### E7 · `K` mid-travel
**Question:** does the ramped stop work, and how much does it overshoot?
**Procedure:** identical bounded goto to E3, but send `K` at roughly half travel. Record the counter at the moment `K` was written and where motion actually ceased.
**Prediction:** motion ends short of target; overshoot is the ramp-down distance.
**The point:** if `K` does nothing, the mount still stops at the target. You learn the stop path is broken without paying for it. **This is the experiment that converts `K` from assumed to verified.**

### E8 · `L` mid-travel
Same, with instant stop. **Record the overshoot difference between `K` and `L`** — that difference is the physical meaning of the e-stop path and belongs in the PRF-12 discussion.

### E9 · `K` and `L` with both axes moving
Two simultaneous bounded gotos, stop both. Confirms per-axis addressing under concurrent motion.

---

## Phase 4 — tracking rate in SLEW mode

The constant is already settled by E5/E6. What remains is confirming that **slew mode** — which is
what tracking actually uses — interprets the step period identically to goto mode.

### E10 · sidereal in slew mode
`G` = `"10"` (SLEW, low speed, forward), `I` = 620, `J`. Unbounded, so this requires `K` proven
(Phase 3). Poll `:j` for 300 s, linear-fit, compare against **104.7304 counts/s**.
**Prediction:** matches E5's goto-mode figure to within measurement error. A discrepancy means the
two modes scale the step period differently, which the driver must then encode explicitly.
**Feeds:** MNT-04 tracking rates; the sidereal/lunar/solar step-period table in M3-T03.

---

## Phase 5 — characterisation (unbounded motion now permissible)

`K` is proven by Phase 3, so open-ended slews are acceptable from here.

### E11 · slew speeds
Each speed class from `G`, low to high, each axis, each direction. Measure actual counts/s per class. **Run the high-speed classes last**, and note the fence is coarse at those rates.

### E12 · direction reversal / counter fidelity — **executed**
Four alternating ±5,000-count moves; error 0 on every one. **This is not a backlash measurement** —
the counter is open-loop and reports commanded steps regardless of what the gearbox did. It
establishes that the *controller* is faithful across direction changes. True backlash needs an
external angular reference: see **E19**, under the sky.

### E13 · goto accuracy vs distance
Gotos of 1,000 / 10,000 / 100,000 / 1,000,000 counts, both directions, five repeats each. Record final error. **Feeds the `tolerance` value in SDD §5.2.3**, which is currently a guessed "default 10 counts".

### E14 · do `d`, `h`, `r`, `m` track the goto target?
Read them before and after `H`/`M`. Phase 0 makes this sharper: `H` and `M` are the goto-increment and break-point setters, so lowercase `h` and `m` are plausibly their readbacks. The survey found them sitting at or near home, suggesting target/breakpoint registers. If confirmed, the driver gains a way to **read back what a goto was programmed with before `J` is ever sent** — a pre-motion safety check the design does not currently have.

### E15 · guide pulses
`P` sets the autoguide rate; issue a pulse and measure displacement in counts. Validates MNT-12 and the `GuideRate` newtype added in SDD §5.1.

---

## Phase 6 — e-stop latency on the wire

### E16 · PRF-12 budget (b)
**Question:** how long from API call to motion actually ceasing?
**Procedure:** requires `scripts/synta-sniff` from T-HIL-1 step 1 — polling `:j` cannot resolve this, since the 16 ms round trip *is* the measurement floor. Timestamp bytes on the wire, and separately timestamp the last changing counter value.
**Budget:** ADD §9.1 (b) — ≤ 100 ms from API call to motion ceasing, which adds 9600-baud transmission and motor response on top of the ≤ 20 ms handler-to-wire figure verified in CI.

---

## Phase 7 — endurance

### E17 · tracking soak
Sidereal on axis 1 for 8 hours with 1 Hz polling. Record drift from predicted counts, comms errors, timeouts. Compare against the 2000-exchange baseline (zero failures, 14.7–17.2 ms).

### E18 · repeated goto cycling
200 alternating gotos. Watch for cumulative position error, thermal effects, comms degradation.

---

---

## Phase 8 — under the sky (OTA mounted, clear night)

Everything above runs on a bare mount indoors. This phase needs the telescope mounted, balanced,
roughly polar aligned, and a clear sky. It corresponds to T-HIL-1 step 5.

### E19 · backlash, measured against stars

**Why the sky rather than a bench rig.** Backlash is angular, and a star is at infinite distance,
so the angular shift measured in the image *is* the axis rotation — no lever arm, no radius
geometry, no question of whether the rod flexed. At the PRD's 1000 mm equipment profile
(0.767 arcsec/px on the R10's 3.72 µm pixels):

| | |
|---|---|
| 1 count | 0.1436 arcsec = **0.187 px** |
| backlash 0.5 arcmin (209 counts) | **39 px** of star displacement |
| backlash 2 arcmin (836 counts) | **156 px** |
| seeing, ~2 arcsec | 2.6 px — **30× smaller** than a 1 arcmin backlash |

A table-top rig with a 300 mm lever and full-resolution stills yields about 2 px per arcmin. The
sky yields **78 px per arcmin — roughly 40× better** — and needs no rod, no macro framing, and no
second tripod.

**The stronger reason is validity, not precision.** Backlash on a bare mount is not the same number
as backlash on a loaded, balanced one: gear mesh, preload and flexure all change once an OTA is
bolted on. A bench measurement characterises a configuration the mount will never image in. Under
the sky it is measured as it will actually be used.

**Measure DEC first.** DEC carries no sidereal motion, so with tracking running the field is static
in DEC and the measurement is a clean subtraction with nothing to model. DEC backlash is also the
one guiding actually suffers from — it is why unidirectional DEC guiding and DEC compensation
exist. Easiest axis and most valuable, which is a rare combination.

**Procedure:**
1. Track at sidereal. Choose a moderately bright star; defocus very slightly so the centroid is
   well sampled rather than landing in one pixel.
2. DEC **+2,000 counts** to seat the gear train in one direction. Settle.
3. Capture the reference frame.
4. Step **−100 counts**, settle ~2 s, capture. Repeat ~20 times.
5. Centroid the star in each frame; plot pixel displacement against cumulative commanded counts.
6. **Backlash = the x-intercept of a linear fit to the engaged region**, extrapolated back to zero
   displacement. Better than eyeballing first motion, and robust to the first step or two being
   partially engaged.
7. Repeat in the opposite direction, three runs each, and report the spread.

No plate solving required — a single star centroid suffices, which keeps this far simpler than the
Phase 2a machinery. Where several stars are in frame, averaging their centroids suppresses seeing
further.

**Then RA**, which is messier: track at sidereal so the star sits still and commanded moves add on
top. Any tracking-rate error contaminates the result, so characterise the drift first over a minute
with no commands issued and subtract it.

**Confounders:** settle before capturing (the mount rings); re-shoot the reference at the end to
check for thermal or tracking walk; and avoid running this near the meridian where a flip could
intervene.

**Feeds:** GDE-01..05 (guiding), MNT-12 guide pulses, and the dither settling in SES-05. It is a
**Phase 3 input — nothing in M0–M2 depends on it**, so the argument for doing it is opportunistic:
the marginal cost on a night you are already set up is about ten minutes, and the centroid harness
overlaps heavily with the pointing-verification work of PLS-05.

## What this series does not cover

Meridian and altitude limits (T-HIL-1 step 6) need real geometry and a payload, so they stay with
the full bring-up. Park and unpark need a defined park position. Physical USB-unplug recovery was
executed as part of M2-T01 step 7 (`../gphoto2-r10/FINDINGS.md`).

The bench-rig backlash approach — camera on a table, rod clamped in the saddle, cross-correlated
ROI — was designed and then **discarded** in favour of E19. It is more work, ~40× less precise, and
measures an unloaded configuration that does not correspond to how the mount is used.

## Feedback into the documents

E1/E2 → SDD §5.2.2 status decoding · **E5/E6 → final confirmation of PRD §4.2** · E7/E8 → `K`/`L`
overshoot, PRF-12 · E10 → tracking rates, MNT-04 · E11 → speed-class table · E12 → backlash
constant, new · E13 → SDD §5.2.3 goto tolerance · E14 → possible new pre-motion safety check ·
E15 → MNT-12 · E16 → ADD §9.1 budget (b) · E17 → REL-02 timeout and heartbeat tuning.
