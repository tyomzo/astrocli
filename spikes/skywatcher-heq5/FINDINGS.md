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

---

# MOTION FINDINGS — Phases 1–4 executed

**Date:** 2026-07-29 · Bare HEQ5, no OTA, no counterweight · `motion.py` · Every experiment ran
behind the opcode allowlist, the position fence and a wall-clock deadline. No fence trip, no
emergency stop, no unexpected motion at any point.

## The headline: the timer frequency correction is confirmed on the wire

**E10** — SLEW mode, step period 620, 1,863 samples over 30 s:

| | |
|---|---|
| Measured rate | **104.617 counts/s** |
| Sidereal rate | 104.7304 counts/s → **0.999×** |
| Implied timer frequency | 620 × 104.617 = **64,862** |
| Our corrected value | 64,935 → **0.11% agreement** |
| The old documented value | 460,800 → would predict 743.2 c/s. Off by 7.1× |

**Step period 620 is the sidereal tracking constant.** Synta chose the timer frequency so that
sidereal lands on a round step period, exactly as suspected. The risk that opened this whole
thread is now closed by measurement rather than inference.

## GOTO ignores the step period — the plan's premise was wrong

E3 was run twice with a 10× change in step period:

| step period | plateau rate | duration for 1,000 counts |
|---|---|---|
| 620 | 5,350 counts/s | 0.19 s |
| 6,200 | 5,335 counts/s | 0.19 s |

Identical. **`I` does not control goto speed.** GOTO uses fixed internal speeds selected by the
mode digit of `G`; `I` governs SLEW and tracking only.

This falsified the restructure that moved the rate measurement into Phase 2 as a bounded goto. The
original ordering was right: the rate can only be measured in SLEW mode, which is unbounded, which
genuinely requires `K` proven first. Recorded rather than quietly reverted — the plan was wrong and
the hardware said so.

**Goto speeds, measured:** low ≈ **5,350 counts/s = 51.1× sidereal**. Applying the verified 16×
high-speed ratio gives 85,600 c/s = **817× sidereal**, against PRD §4.2's stated 800× maximum slew.
Independent corroboration of both the ratio and the speed model.

## Per-experiment results

**E1 · `F` initialise** — `:F1` and `:F2` both returned a bare `=`. Status went `100` → `101` on
each axis: the initialised bit, exactly as the Phase 0 bit table predicts. **Zero counter movement
on both axes.** The status decoder is now validated in two distinct states.

**E3 · first bounded goto** — `G`/`I`/`H`/`M`/`J` all accepted. Commanded +1,000 counts, travelled
+1,000 counts, **goto error 0**. Self-terminated with no stop command sent, which was the whole
point of going bounded-first. Constant-velocity plateau with deceleration confined to the final
sample.

**E7 · `K` mid-travel** — sent at +5,044, motion ceased at +5,128. **Overshoot +84 counts**
(12.1 arcsec). Did not reach the 10,000 target, so `K` genuinely arrested the motion.

**E8 · `L` mid-travel** — sent at +5,038, ceased at +5,123. **Overshoot +85 counts** (12.2 arcsec).

**`K` and `L` are indistinguishable at low speed.** One count apart is noise. At 5,350 c/s, 84
counts is 15.7 ms of travel — essentially one serial round trip, so the overshoot is *command
latency, not deceleration*. The ramped-versus-instant distinction only has physical meaning at
high speed, where real momentum exists. PRF-12's rationale should say so rather than implying `L`
is always meaningfully faster.

## Design consequences

1. **PRD §4.2 timer frequency 64,935 — confirmed empirically.** Close the risk.
2. **Sidereal step period is 620.** Record it; it is the tracking constant M3-T03 needs.
3. **The driver must not attempt to control goto speed via `I`.** Goto speed is selected solely by
   the `G` mode digit, which offers exactly two speeds. Any design that computes a goto step period
   is wasting effort and will mislead whoever reads it.
4. **SDD §5.2.3's goto tolerance of "default 10 counts" is generous** — we measured 0 counts of
   error. It can be tightened, though more samples across distances (E13) should inform the value.
5. **Stop overshoot is dominated by link latency**, so it scales with rate rather than with the
   choice of `K` versus `L`. At 817× sidereal the same 16 ms becomes ~1,370 counts.

## IMPORTANT CAVEAT — none of this proves the axis physically rotated

The position counter is **an open-loop stepper step count, not encoder feedback** (PRD §4.2: the
HEQ5 Pro has no position encoders). It reports steps the controller believes it has issued. If a
motor had not turned at all — unpowered, stalled, driver fault — **every measurement above would
look exactly the same.**

So what these experiments establish is that *the motor controller accepted the commands and
executed the step sequence at the commanded rate*. That is genuinely what the driver needs: the
counter is the value the driver reads and converts to coordinates, so the step-period-to-counter-rate
relation confirmed in E10 is directly load-bearing. But it is not evidence of physical rotation,
and it is certainly not evidence that the mount tracks at sidereal in the sky.

Total commanded travel across the whole session was **14,394 counts = 0.574°** on the RA axis —
barely perceptible on a bare mount head with nothing attached for visual reference. And if the
clutches were loose, as the plan calls for, the motor and gear turn while the output shaft
deliberately does not.

**To close this, one of:** listen for the stepper (at 5,350 steps/s it should be clearly audible);
mark the shaft and command a large, unmistakable move; or watch supply current during motion.
Until then, treat every figure above as characterising the *controller*, not the *mechanism*.

## Still open

Physical-rotation confirmation (see the caveat above), Phase 5 characterisation (backlash, per-class slew speeds, goto accuracy across distances, the
`d`/`h`/`r`/`m` readback hypothesis, guide pulses), Phase 6 e-stop latency on the wire with the
sniffer, and Phase 7 endurance. High-speed classes remain unrun.

---

# OPTICAL CONFIRMATION — the axis physically rotates

**Date:** 2026-07-29 · Camera on the table aimed at the mount, live view recorded at ~30 fps while
a **10° goto (250,667 counts)** was commanded. This closes the open-loop caveat above.

## Result: confirmed, 7.1× the noise floor

Comparing two *static* frames — one 1 s before the move, one 4 s after it finished:

| Comparison | Mean abs pixel difference |
|---|---|
| Static vs static, both before motion (8 s vs 10 s) | 0.712 |
| Static vs static, both after motion (21 s vs 23 s) | 0.737 |
| **Before vs after (11 s vs 22 s)** | **5.243 — 7.1× the noise floor** |

The change is **spatially localised**, which is the convincing part. An 8×6 grid of the difference
image shows one vertical band carrying nearly all of it (column 3: 14.4, 35.4, 25.9, 14.0, 15.3,
9.5) while the background columns sit at 0.6–2.6, i.e. noise. Peak pixel change 204 of 255;
4.46% of the frame changed by more than 20 levels. That is an object moving within a static scene,
not a global exposure or noise shift.

**A whole-frame average is the wrong statistic here** and nearly produced a false negative: during
motion the mean inter-frame difference rose only from 0.426 to 0.453 (1.1×), because the mount
occupies a small fraction of the frame. The timing was still visible — a smooth rise beginning at
13.0 s and decaying by 16.5 s, matching the commanded window — but the magnitude looked like
nothing. Spatially-resolved comparison was necessary.

**Why nothing was heard or seen by eye:** the earlier experiments commanded a total of 0.574°, in
bursts as short as 0.19 s, with the slowest segment running at 104 steps/s. Imperceptible on a bare
head. This 10° move is the first one that was ever going to be obvious.

## The goto speed model was wrong — it ramps, it is not fixed

The 250,667-count goto produced a textbook trapezoid:

```
  t=0.02s     3,101 c/s
  t=0.82s    29,887 c/s     ramp up
  t=1.62s    84,116 c/s
  t=2.81s    87,339 c/s     cruise (~1.6 s)
  t=3.61s    47,789 c/s
  t=4.41s     9,377 c/s     ramp down
  t=5.61s         0 c/s
```

**Peak 87,486 counts/s = 835× sidereal**, against PRD §4.2's stated 800× maximum slew — good
agreement, and the strongest confirmation yet of that figure.

This corrects the earlier claim that goto has "two fixed speeds". The 1,000-count goto peaked at
5,350 c/s not because that is a low-speed setting, but because the move was **ramp-limited** — with
the break point at 500 counts it began decelerating before it ever reached cruise. Short gotos
never see cruise speed.

Note 87,486 ÷ 5,350 = 16.4, suspiciously close to the verified 16× high-speed ratio. That may be
meaningful or may be coincidence; the acceleration profile is not yet characterised well enough to
say, and it should not be assumed.

**The step-period conclusion still stands**: identical profiles at step periods 620 and 6,200 mean
`I` does not govern goto speed. Had it done so, the 10× slower period would have produced a
proportionally slower ramp.

## Deployment finding: gvfs steals the camera

Mid-session the camera became unreachable with "Could not claim the USB device". Cause:
`gvfsd-gphoto2` had auto-mounted it (`gphoto2:host=Canon_Inc._Canon_Digital_Camera_...`), holding
the USB claim exclusively. Releasing the gvfs mount restored access immediately.

**The field node must prevent this** — a desktop environment silently taking the camera is a
guaranteed field failure, and the error message points nowhere useful.

### Detection — implemented and verified

`spikes/gphoto2-r10` now diagnoses the claim failure instead of reporting it bare. Reproduced
deliberately (`gio mount "gphoto2://[usb:005,007]/"`), the spike output becomes:

```
FATAL: autodetect failed: Could not claim the USB device
--- diagnosing ---
  CAUSE FOUND: gvfs has the camera mounted and holds the USB claim.
    mount: /run/user/1000/gvfs/gphoto2:host=%5Busb%3A005%2C007%5D
  Release it now:
    gio mount -u "gphoto2://%5Busb%3A005%2C007%5D/"
```

Both the URL-encoded and decoded unmount forms were tested and both work; the camera reconnects
immediately afterwards (autodetect 108 ms). When no gvfs mount is present the diagnostic lists the
other causes — camera off or asleep, not in PTP mode, another process holding the node,
permissions — rather than claiming a cause it has not established.

### Prevention on the field node — mechanism verified, deliberately not applied here

**Masking the systemd user unit alone is not sufficient**, and this is the part worth knowing.
`/usr/lib/systemd/user/gvfs-gphoto2-volume-monitor.service` is `Type=dbus`, and
`/usr/share/dbus-1/services/org.gtk.vfs.GPhoto2VolumeMonitor.service` carries a direct
`Exec=/usr/libexec/gvfs-gphoto2-volume-monitor`. D-Bus can therefore activate the binary even with
the unit masked. Both paths must be closed:

```sh
systemctl --user mask gvfs-gphoto2-volume-monitor.service
mkdir -p ~/.local/share/dbus-1/services
cat > ~/.local/share/dbus-1/services/org.gtk.vfs.GPhoto2VolumeMonitor.service <<'EOD'
[D-BUS Service]
Name=org.gtk.vfs.GPhoto2VolumeMonitor
Exec=/bin/false
EOD
```

Both are user-level — **no root required** — and reversible (`systemctl --user unmask`, delete the
override). A headless field node has no gvfs at all and needs neither.

**Not applied to the development workstation**, where gvfs camera integration is wanted and this
would only break the file manager. The full hotplug path could not be exercised without physically
replugging, so the mechanism is verified (unit is `Type=dbus`, D-Bus file has a direct `Exec`,
override directory is user-writable) while end-to-end hotplug suppression remains untested.

---

# PHASE 5 — characterisation (E12, E13, E14)

## E16 — the standing start is the wall, and software cannot ramp past it — 2026-08-02

E11's discriminating pair, run by ear, plus a ramp attempt and its telemetry. Three results, one
dead end, and the design that follows from them.

**Measured:**

| what was commanded | class | result |
|---|---|---|
| 32× sidereal, unbounded slew | low | **turns** |
| 39× sidereal, unbounded slew | high | **jams** — 21% faster than the rung that turns |
| high-class start at a 16×-equivalent period, then software-ramped `I` frames toward 64/400/800× | high | **buzzes without movement, all three rungs** |

During the buzzing, the counter — which counts *commanded* steps — ran at 140–630× sidereal
within seconds, far past the software ramp's intended schedule (16× at start, ×1.6 per 150 ms).
Whatever the motor controller does with `I` frames while running a high-speed slew, it is not
"apply the requested rate": the commanded rate ran away from the schedule that requested it.

**From the reference implementations** (indi-eqmod `skywatcher.cpp`, indi `skywatcherAPI.cpp`):

* EQMOD's high-speed threshold is **128×**, not this driver's crossover-derived 38.8× — so the
  "previously workable" EQMOD ladder ran its 64× rung in the **low** class. E11's 64× jam was in
  the high class; the two are consistent.
* EQMOD's own comment: *"Can not change speed while motor is running and in goto or highspeed
  slew."* The software-ramp-by-`I`-frames design is illegal by the protocol's de-facto spec.
* Both drivers start high-speed slews cold at the target rate, stop-first, no ramp. That works on
  the mounts they support; on this one, 39× from standstill jams.

**What remains genuinely unknown (E17, ten minutes by ear + counter):** whether a *single fixed*
gentle high-class period starts this mount — tonight's gentle starts always had ramp frames in
flight 150 ms later, so the clean data point was never taken. And what rate the counter reports
for a fixed high-class period, which would pin the high-mode period semantics this driver has
never actually verified (E10 verified the low class only; a goto ignores the register entirely).
Also worth one rung: 64× pinned to the **low** class, which is what EQMOD actually ran for years.

**The design decision this buys:** on firmware 2.04.01 the only primitive with a working
acceleration profile is the bounded goto — measured cruising at 835× — so fast manual motion
should be *bounded, firmware-ramped moves issued while the button is held and stopped with `K` on
release*, not bigger numbers on the unbounded ladder. The ladder stays 1/8/16/24/32: every rung a
rate this mount was heard to start.

## E14 · `h`, `i`, `m` are readback registers — a pre-motion check the design lacks

Wrote `:H`=12,345 and `:M`=6,172 at position 8,654,669, **without sending `J`**, then re-read the
undocumented inquiries:

| Register | Before | After | Decoded | Meaning |
|---|---|---|---|---|
| `:h` | 8,654,669 | **8,667,014** | = position + `H` | **absolute goto target** |
| `:i` | 12 | **620** | = the value written with `:I` | **step period** |
| `:m` | 8,619,853 | **8,660,841** | = position + `M` | **absolute break point** |
| `:r` | 8,619,950 | 8,660,910 | +6,241 from position, 69 past the break point | related to braking; unresolved |

All three matches are exact. This confirms the hypothesis and hands the driver something it does
not currently have:

> **Verify the goto before committing to it.** After `G`/`I`/`H`/`M` and *before* `J`, read back
> `:h`, `:m` and `:i` and assert they equal the intended absolute target, break point and step
> period. Three round trips ≈ 48 ms at the measured 16 ms each. A corrupted frame, a byte-swap
> error, a wrong axis digit — the mount does not validate that one — or a mis-encoded increment is
> caught *before* anything moves, rather than being discovered by watching where the mount ends up.

This is worth more than it sounds given the earlier finding that the mount does not validate the
axis digit and answers `:j9` with plausible data. Cheap insurance on the one command sequence where
being wrong is expensive.

## E13 · goto accuracy: zero error at every distance

| Commanded | Angle | Error | Duration |
|---|---|---|---|
| ±1,000 | 0.040° | **0** | 0.64 s |
| ±10,000 | 0.399° | **0** | 2.33 s |
| ±100,000 | 3.989° | **0** | 4.10 s |

Six gotos, both directions, **maximum error 0 counts**. SDD §5.2.3's tolerance of 10 counts is
ample — it could be tightened to 2–3, though there is no pressing reason to, and a tolerance that
is loose in the safe direction costs nothing.

Note the duration non-linearity again: 100× the distance takes only 6.4× the time, because short
moves are ramp-limited. Reinforces that the orchestrator must not estimate slew time linearly.

## E12 · direction reversal: the counter is faithful, backlash is invisible

Four alternating ±5,000-count moves, **error 0 on every one**.

**This does not measure backlash.** The counter is an open-loop step count, so it reports what the
controller commanded regardless of what the gearbox did. Zero error here means the *controller* is
faithful across direction changes — worth knowing, and not nothing — but mechanical lost motion on
reversal is invisible to it by construction.

Real backlash needs an external angular reference, and the right one is **the sky, not a bench rig**
— see E19 in `MOTION-PLAN.md`. At the 1000 mm equipment profile one count is 0.187 px of star
motion, so a plausible backlash is 39–391 px: about 40× the sensitivity of a camera-on-a-table rig
with a lever arm, with no rod and no second tripod. More importantly it measures the mount
*loaded and balanced*, which is the only configuration whose backlash matters — gear mesh and
preload differ once an OTA is mounted. Remains **open**, and is a Phase 3 input rather than
anything M0–M2 depends on.

## Status

Phases 1–5 complete apart from per-class slew speeds (E11) and guide pulses (E15), plus the
optical backlash measurement noted above. Phase 6 (e-stop wire latency, needs the sniffer) and
Phase 7 (endurance) not started.


## E11 — the manual slew ladder, run 2026-08-01 by ear

The one experiment the bench never ran, settled by an operator standing at the mount while the
API drove it. Bare mount, clutches engaged, no payload, both axes.

| level | commanded | counter says | rotor actually |
|-------|-----------|--------------|----------------|
| 1 | 1× sidereal | 106 counts/s | **turns** |
| 2 | 8× | 839 counts/s | **turns** |
| 3 | 64× | 6,993 counts/s steady (27,974 including the ramp) | **stalls** — ramps up, then jams and stops |

### The ladder was corrected on 2026-08-02, and the stall may not be about rate at all

The driver's ladder was EQMOD's — 1×, 8×, 64×, 400×, 800× — and its own doc comment said E11 "was
never run". It had been. Three of five rungs promised motion this mount does not make, and the top
two by a factor of fifty. The ladder is now **1, 8, 16, 24, 32**.

The stopping point is not an interpolation. On this mount the speed-class crossover sits at
`f/r = 16`, i.e. 4,058 counts/s or **38.8× sidereal**: below it a rate programs in the low class,
one counter step at a time; above it in the high class, sixteen counts per step. Every rate heard
to turn is below that line. The one that jammed is above it. So E11 may have found not a rate
ceiling but that this mount will not *start an unbounded slew in the high class* — and a goto,
which is bounded and ramped, was measured cruising at 835× in that same high class.

Two experiments would settle it, both ten minutes by ear: drive 32× then 39°× — either side of the
crossover, a 20% rate change across a class change — and hear whether the jam follows the class
rather than the rate; then, if it does, start a high-class rate from a ramped profile.

**The stall is invisible to every instrument this project has.** A Synta mount's counter counts
*commanded* steps, so a stalled rotor and a turning one produce identical position telemetry — the
counter walked 1.67° during a slew that ended with the axis buzzing in place. Nothing in the driver,
the API or the PWA can distinguish them. The operator's ears are the only rotor sensor in the
system, which is the open-loop reality PRD §4.2 states and the first time it has bitten in practice.

Two consequences beyond the ladder:

* **The rate model is exonerated.** Level 3's steady rate measured 6,993 c/s against 6,702 predicted
  (1.04×, and the excess is ramp still bleeding into the window) — so `program()`'s speed-class
  crossover and the high-class ratio arithmetic are right. What fails is the *motor*, at an
  acceleration this firmware applies in slew mode.
* **The random-direction report is downstream of this.** A stall corrupts the counter, the corrupted
  counter can cross the pole on paper, and the declination motor's sense is derived from the branch
  that implies — so N and S swap after a stall. Fixing the stall should fix the direction; verify
  rather than assume.

**Bracketed the same evening, by ear, using the labelled control:** every rung up to and including
**8× turns cleanly**; **64× skips**; **400× makes its own distinct noise** — fast enough that the
rotor arguably never synchronises rather than losing sync partway. So the usable manual band on this
mount, bare and unloaded, is **1×–8×**, with the true threshold somewhere in the untested gap
between 8× and 64× (the EQMOD ladder offers no rung between them, which is itself the finding: the
convention's steps are too coarse to bracket further without custom rates).

That band is not a limitation in practice — it is the band framing actually wants. 8× sidereal is
2 arcminutes per second, which crosses an R10 frame in about a minute; 64× would cross it in eight
seconds and 800× in under one. The rungs above 8× exist for the *goto* path, which reached 835×
cleanly tonight through a different firmware ramp.

**Still unmeasured:** where between 8× and 64× the threshold actually sits, whether it moves with balance
or payload (it will), and whether the goto path's own ramp — which reached 835× cleanly tonight —
is a different profile in firmware or merely luckier. The PWA's speed control now labels every rung
in sidereal multiples so the threshold can be bracketed by ear without a rebuild.


## The home hour angle is +6h, not 0 — found by eye, 2026-08-01

SDD §5.2.3 and `math/mech.rs` model the sky as `HA = s·h`, so both counters at `0x800000` mean
hour angle **zero**: the meridian. That is wrong by exactly six hours, and no test could have
caught it — every fixture compares the model against itself, and the mount's counters agree with
whatever the model says they mean. It took an operator looking at metal.

**The evidence, two independent observations from the home pose** (counterweight shaft down, tube
along the polar axis, both counters at home after a power cycle):

| commanded | model said | tube actually pointed |
|-----------|-----------|----------------------|
| DEC axis +90° (dec 90 → 0), RA axis untouched | south, alt 30° (HA 0) | **west**, on the horizon (HA +6h) |
| then DEC to 60°, then RA axis +90° (HA −6h) | north-east, alt 48.5° | **straight up** — the zenith (HA 0) |

Both resolve to one correction: **`HA_true = HA_model + 90°`**. The second is the stronger of the
two, because the zenith is unmistakable — there is no confusing it with anything, and it lands
exactly where the corrected model predicts.

**Why the geometry demands it.** The counterweight shaft lies *along* the declination axis, and at
home it hangs down — which puts it in the meridian plane. Rotating the tube about that shaft
therefore sweeps it through the plane perpendicular to the meridian: east–west. A pure declination
move from home **cannot** put the tube on the meridian, which is precisely what the model claimed.
Reaching the meridian needs the RA axis to turn 90° as well, and that is the six hours the model
was missing.

**What this invalidates, and what it does not.** Everything mechanical is sound: both axes moved
exactly 90.00° as commanded, with correct ramps, readback verification and settling. The codec, the
serial task, the controller, the goto supervision and the limits all did their jobs. What was wrong
is only the *map* — the constant relating axis angle to hour angle. Consequences worth naming:

* Reported RA has been six hours out whenever the declination axis was away from the pole. DEC has
  always been right (it does not depend on this constant).
* The altitude limit computes from that RA, so it has been refusing and permitting the wrong
  targets — the alarming "39° below the horizon" earlier the same evening was very likely this
  error, not a mount that had been driven underground.
* `goto` still *arrives* at the position it computed; the position simply was not the sky
  coordinate the operator asked for.

The fix is a constant in `mech.rs`'s four-line model, but it propagates to `goto_solution`, the
pier-side branch and the altitude limit, so it belongs in a task with fixtures reworked and this
same swing test as its acceptance criterion — not a one-line patch at midnight.


## A collision, and the rule it buys — 2026-08-02

**The tube was driven into the tripod.** No sky-coordinate goto may be commanded while the
mech↔sky map is known to be wrong. That sounds obvious written down; it was violated within an
hour of the error being *documented*, by reasoning that felt sound at the time: "I am only zeroing
the axes, so the offset cancels." It does not. The target was expressed in sky coordinates, it was
computed from a position the broken map had reported, and every step of the derivation therefore
inherited the fault. A 90° slew was then issued with no clearance check, into an arc that had been
explicitly flagged as needing one.

**The rules this buys, and they are not optional:**

1. **While the map is untrusted, motion is axis-relative or it does not happen.** Park-to-home
   counters (M3-T07) is the correct primitive precisely because it never consults the map. Until
   M3-T06 lands and is re-verified against the physical swing test, no `goto` to a sky coordinate
   should be issued on real hardware.
2. **Every slew larger than a nudge gets a clearance check first**, stated as a question to the
   operator and answered before the command is sent — not a warning issued alongside it.
3. **The operator's eyes are the only position sensor.** An open-loop mount plus a model known to
   be wrong means the software's belief about where the tube is has no evidentiary value at all.
   The correct move when the pose is uncertain is to *ask*, or to have the operator set it by hand
   with the clutches loose — never to slew toward a computed answer.

The mount's own protections could not help: the altitude limit computes from the same broken map,
and a tripod is not something any limit in this system models. Mechanical safety here rests
entirely on the operator's judgement and the person issuing commands respecting it.
