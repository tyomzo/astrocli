# M2-T05 — desk integration evidence

The M2 exit run: the M1 demo, rerun with a real Canon R10 on USB.

Everything here was produced by scripts in `scripts/`, against the reference body
(`04a9:32f8`, battery 100 %, 64.7 GB free) on 2026-07-31. Re-running any of it is one command,
listed with each section.

| | |
|---|---|
| body | Canon EOS R10, USB `04a9:32f8`, libgphoto2 2.5.31 |
| host | 32 cores, Ubuntu, gvfs present and auto-mounting |
| build | `cargo build --release -p astroctl-field --features libgphoto2` |
| gates | all six green; 887 Rust tests (874 baseline + 13), 199 frontend |

---

## 1. The contract held — the diffstat

The acceptance criterion is a diffstat, because M2's whole claim is that swapping a simulator for
a real camera changes nothing above the HAL. See `diffstat.txt` for the generated form.

What changed, and why each is inside the sanctioned set:

| area | why it is in scope |
|---|---|
| `crates/astroctl-pipeline/` | *the decoder variant* — the CR3 and JPEG arms M2-T05 asks for |
| `crates/astroctl-field/` (registry + link-state publish) | named in M2-T04's handoff as outstanding work for this task |
| `config/`, `docs/intent/…PRD…` §8.1 | *config*, and the PRD block the example must match verbatim |
| `scripts/`, `docs/evidence/`, `tests/` | *tests and evidence* |
| `Cargo.toml`, `Cargo.lock` | the `rawler` dependency the decoder variant needs |

**Nothing in `astroctl-core`, `astroctl-hal`, `astroctl-session`, `astroctl-transfer`,
`astroctl-stack`, `astroctl-safety`, `astroctl-ipc` or `frontend/` was touched.** The M1 stack ran
a real camera unmodified, which is the result.

`astroctl-drivers` — which the criterion *does* permit changing — was also not touched. M2-T02
through T04 had already finished it.

---

## 2. The desk E2E

```sh
scripts/desk-e2e.sh                 # real camera
scripts/desk-e2e.sh --simulator     # the same run with no body, to test the harness
```

Bundle: `desk-e2e/` — `timings.md`, `timings.json`, `events.jsonl` (the raw `/ws` recording),
`liveview.jsonl`, both node logs, and the generated config.

### Preview latency — the headline number

Measured from `capture.progress: saved` to `capture.progress: preview_ready`, both as arrival
times in one monotonic `/ws` recording. `saved` is the **pessimistic** choice for "exposure end":
it lands after the download, so the ~1.5 s CR3 transfer is already inside every number below.

| | budget | measured |
|---|---|---|
| preview after exposure end | 3 s | **0.124 s** (4 % of it) |

Against the simulator the same script measures 0.113–0.140 s, so the real camera's decode costs
about 10 ms more end to end than a FITS frame — which is the point.

### Live view

| | required | measured |
|---|---|---|
| frame rate | PRF-02: ≥ 5 fps | **5.1 fps** over 7.7 s |
| frame size | — | 149 KB mean |

The body sustains 58.5 fps; `live_view_fps: 5` throttles it down on purpose (USB-11).

### The rest of the path

* `camera.status` on connect: `{"connected":true,"battery_pct":100,"charging":false,"storage_free_mb":66265}`
* settings read *and written* through the API, read back from the body
* transfer queue drained to `state: idle`, `queue_depth: 0`, `last_ack_ts` set — **the stack node
  acked the frame**
* stack node healthy, worker `ready`
* the preview served at `/api/session/frames/{id}/preview.jpg` is **750×500, one component** —
  exactly CR3 6000×4000 → half-size 3000×2000 → quarter-res 750×500 → asinh → Luma JPEG

### What the run could not do, and why

**Five timed frames and one bulb frame cannot happen in one run**, and no script can change that.
The R10's physical mode dial is a constraint on the API, not a setting reachable through it:

| dial | body offers | possible |
|---|---|---|
| **M** | `30"`…`1/4000` | the timed frames |
| **B** | `bulb` only | the bulb frame |

The recorded run was made with the dial on **B** and captured the bulb frame; the script detected
the dial, skipped the timed half, and printed the command for it. See the operator checklist.

---

## 3. CR3 decode — measurements

```sh
ASTROCTL_CR3=/path/to/frame.cr3 \
  cargo test -p astroctl-pipeline --release --test cr3_frame -- --ignored --nocapture
```

| | measured |
|---|---|
| half-size decode (24 MP → 3000×2000) | **74–81 ms** |
| quarter-res + asinh + JPEG encode | **6 ms** |
| total | **80–87 ms** |
| peak RSS, one decode | **+36 MB** |
| RSS over 36 repeated decodes | 253 MB, **flat after round ~21** |
| the same with `MALLOC_ARENA_MAX=2` | **102 MB, zero drift** |

### PRF-05 and the allocator — an operator action

The 253 MB is not a leak: it plateaus and stays flat for fifteen further rounds. It is glibc
handing each of `rawler`'s decode threads its own malloc arena, so it scales with **cores**, not
with frames — a 4-core Pi holds a fraction of what this 32-core host does.

But 253 MB of PRF-05's 512 MB spent on allocator arenas is worth not spending. Capping them costs
nothing measurable in decode time and takes peak RSS to 102 MB:

```
Environment=MALLOC_ARENA_MAX=2
```

`scripts/desk-e2e.sh` and `scripts/desk-soak.sh` set it. **It is not in `deploy/`** — that is
outside this task's permitted diffstat, and putting it there is the follow-up.

---

## 4. The soak

```sh
scripts/desk-soak.sh --hours 2 --bulb 4     # the M2-T05 run
scripts/desk-soak.sh --minutes 16 --bulb 4  # the short proving run recorded here
```

Bundle: `desk-soak/` — `soak.md`, `samples.csv`, `events.jsonl`.

**The full two-hour soak has not been run.** What is recorded here is a short run proving the
machinery works; the two-hour version is one command and belongs to the operator. Said plainly
because a soak nobody ran is not evidence of anything.

Definitions the script asserts against, because "no wedges, zero lost frames" needs them:

* **lost frame** — a capture accepted (202 + frame id) whose `preview_ready` did not arrive before
  the next capture was due. The frame may be on disk; this is a *pipeline* soak and a frame the
  operator never sees is lost to the operator.
* **wedge** — a capture refused, or the node stopping answering `/api/system/health`, or two
  consecutive missed previews.

The decode spikes are **inside** the RSS numbers rather than excluded, though M2-T05 permits
excluding them: each sample is taken at a fixed offset after that round's capture, so the sampler
lands in the same phase every time rather than at a random point between spikes. An excluded spike
is an unmeasured spike, and the spike is the part that would kill a Pi.

---

## 5. The cable pull

```sh
scripts/desk-cable-pull.sh          # --help prints the full procedure
```

**The pull itself is the operator's.** The script is the observation half: it starts live view
(T-CAM-1 induces the wedge mid-stream), holds a `/ws` subscription, prints each transition as it
arrives, and takes a capture at the end — because "the badge went green" is not the criterion,
"the camera works again" is.

The software stand-in is deliberately **not** used. M2-T04 measured `USBDEVFS_RESET` taking this
body off the bus until it was physically power-cycled.

**Not yet run.** See the operator checklist.

---

## 6. Operator checklist

Three things need a human at the desk. Each is one command.

### a. The timed half of the E2E

The recorded run had the dial on **B**. For the five timed frames:

```sh
# turn the mode dial to M, then:
scripts/desk-e2e.sh --no-build --frames 5 --bulb 0
```

Expect five frames, each previewing well inside 3 s. To get a preview that is a *picture* rather
than a black rectangle, drop the ISO and stop the lens down first — see the note on the collapsed
stretch window below.

### b. The two-hour soak

```sh
scripts/desk-soak.sh --hours 2 --bulb 4
```

Attach it to a pair `desk-e2e.sh` left running. It prints a line per round and writes
`docs/evidence/m2/desk-soak/soak.md` at the end. Watch for the RSS column against 512 MB.

### c. The cable pull

```sh
scripts/desk-cable-pull.sh
# wait for ARMED, pull the cable at the CAMERA end, plug it back in ~10 s later
```

If it reports gvfs took the claim, run the `gio mount -u` command the alert names.

### d. The battery reading against the body's own display

Still open from M2-T04, and only a human can close it. The driver reads **100 %**; hold that
against what the body's screen says (±5 % is the bar):

```sh
cargo test -p astroctl-drivers --features libgphoto2 --test hardware_r10 \
  battery_and_storage -- --ignored --nocapture
```

---

## 7. Things found that are worth knowing

* **A blown frame previews as black, identically to a dark frame.** With >99.5 % of a frame at one
  value both stretch percentiles land on it, the window collapses, and `white = black + 1.0` maps
  everything to zero. That guard matches `workers/compute_worker.py` deliberately — the two
  implementations must agree about a dark — but the consequence is that over- and under-exposure
  are the same picture. An operator judging exposure from the preview cannot tell them apart.
* **`rawler`'s R10 white level is not the clipping point.** The profile says 12735; saturated
  photosites read 16383. Normalising by the profile value clips the top of the sensor's range — on
  a star field, the stars. The decoder therefore does not rescale at all; see `cr3.rs`.
* **`rawler` is LGPL-2.1** where the workspace is MIT. Compatible to use, weak copyleft to ship.
  The M2-T01 survey recorded this crate's decoders, profiles and fixtures and not its licence.
* **The shipped example config could not start a node.** `config/field-node.example.yaml` has said
  `driver: gphoto2` since M0 while the registry held only `simulator`. The test fixtures rewrite
  the string on the way past, so nothing caught it. Fixed by this task.
* **With the dial on B the body reports its other shutter as `Unknown value df00`** — libgphoto2
  declining to name a Canon code, not a driver fault.
* **M2-T03's open question is answered: a bulb after an abort works.** The buffer-orphan hypothesis
  holds; that task's failure was the fading battery, not `eosremoterelease` needing a reset. Run:
  `cargo test -p astroctl-drivers --features libgphoto2 --test hardware_r10
  an_aborted_bulb -- --ignored --nocapture`
* **`libgphoto2-dev` is not installed on the reference desk machine.** Every hardware run in M2 has
  gone through an unpacked `.deb` in a scratch directory. `scripts/desk-e2e.sh` prints the recipe
  when the build fails.
