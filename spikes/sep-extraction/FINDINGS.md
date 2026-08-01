# SEP star-extraction spike — FINDINGS

**Date:** 2026-08-01 · **Question:** go/no-go on Phase 2a's oldest open risk — first-party Rust
binding over vendored libsep C source (the `erfars` pattern), resident on the field node
(ARC-06) · **Base:** `d86d9cf` (M1-T17 merge)

**Host:** AMD Ryzen 9 9950X (16C/32T), 127 GB RAM, Ubuntu 25.10, kernel 6.17.0-41, gcc 15.2.0,
Rust 1.97.1 · **Library:** SEP 1.4.1, commit `93b3ac5` · **Cross-check:** Python `sep` 1.4.1,
numpy 2.5.1, astropy 8.0.1 · **Frames:** the session's own 24 MP simulator FITS
(`data/sessions/2026-07-30_session/frames/`) and the M2-T01 spike's real R10 CR3s

Provenance is labelled per item: **measured** on this host, **derived** by arithmetic from
measured values or from the C source, **assumed** where neither.

---

## Verdict: GO — with one condition that is not technical

Both technical questions come back clean, and by wider margins than the risk register feared:

| Question | Answer | Number |
|---|---|---|
| Vendor-builds with `cc`? | **yes**, no system deps, no bindgen, no CMake | 8 `.c` files, 1.7 s full rebuild |
| Fits PRF-05's 512 MB? | **yes**, with 4.7x headroom | **108.3 MB** peak for a 24 MP frame |
| Fast enough? | **yes** | **230 ms** on this host; 0.9–1.4 s extrapolated to a Pi |
| Accurate? | **yes** | centroid RMS **0.4585 px**; **0.015–0.13 px** for mag < 15 |
| Same as Python? | **identical** | centroids **bit-identical**, 144/144 objects |

**The condition is the licence.** libsep is **LGPLv3**, not BSD/MIT as the plan assumed. That is
not a blocker but it is a decision the project has not yet knowingly taken, and it is the one
finding here that should go back to whoever recorded "first-party Rust binding, vendored C
source". §1 works through it.

---

## 1. Vendor build — works, and the licence is the surprise

**Measured.** The build is as boring as hoped. `cc::Build` over eight `.c` files, one include
directory, one `-D`, and `-lm`. No `pkg-config`, no `bindgen`, no libclang, no CMake, no system
package. Contrast M2-T01, where `libgphoto2_sys` needed `pkg-config` *and* `bindgen` and could
not even compile on a bare machine.

| | |
|---|---|
| Sources | `analyse.c aperture.c background.c convolve.c deblend.c extract.c lutz.c util.c` |
| Headers | `sep.h sepcore.h extract.h overlap.h` + `aperture.i` |
| Total C | 6,595 lines |
| Flags | `-O3`, `-DSEP_VERSION_STRING="1.4.1"`, `-lm` |
| Full C rebuild | **1.7 s** wall (32 threads available; `cc` parallelises) |
| Compiler warnings | **zero**, at `-Wall -Wextra -Wcast-qual -O3`, all 8 units, gcc 15.2.0 |
| SIMD / threading flags | **none needed** — see below |

**No SIMD flags, no threading flags, no math-lib surprises.** The library is plain C99 with
`math.h`. It contains no intrinsics, no OpenMP, no pthreads. `-lm` is the only link requirement
and on glibc Rust's std already pulls it, so even that is belt-and-braces.

Zero warnings deserves a note because it changes a default. Upstream's Makefile passes
`-Wall -Wextra -Wcast-qual` with `# -Werror` commented out, which reads like a codebase carrying
tolerated warnings. It is not: there are none. So `build.rs` leaves warnings **on**, and a future
version bump that introduces one will be visible rather than swallowed.

### The licence — LGPLv3

**Measured** (read from the source headers and `README.md`):

| Files | Licence |
|---|---|
| everything derived from Source Extractor — `analyse.c aperture.c background.c convolve.c deblend.c extract.c lutz.c util.c sep.h sepcore.h extract.h` | **LGPLv3** |
| `overlap.h` (from photutils) | BSD 3-clause |
| `sep.pyx`, the Python wrapper — **not vendored** | MIT |

Upstream states it plainly: *"The license for the library as a whole is therefore LGPLv3."*

AstroCtl is MIT (root `LICENSE`, `Cargo.toml` `license = "MIT"`). The tension is specific and
worth stating precisely rather than hand-waving:

* LGPLv3 permits use from a differently-licensed program. It is not GPL; it does not make
  AstroCtl LGPL.
* What it requires is that the end user be able to **replace the LGPL component**. For dynamic
  linking that is automatic. For **static linking — which is what `cc` + `cargo` does — it is
  not**: §4(d) of LGPLv3 requires either shipping relinkable object files/source, or using a
  shared library mechanism.
* The practical routes, in rough order of cost: (a) ship the vendored source, which we would do
  anyway since it is in-tree and the field node image can carry it; (b) link libsep dynamically;
  (c) drop SEP and write the extraction ourselves.

Route (a) is cheap for this project specifically, because the field node is not a mass-market
proprietary binary and the source is already in the repository. **But this is a legal judgement,
not an engineering one, and it should be taken explicitly.**

This is the second LGPL flag in two milestones — `rawler` raised the first in M2. Two is a
pattern, and the pattern is that astronomy's C/Rust ecosystem is largely (L)GPL. If the project
intends to stay MIT, a standing licence check at dependency-selection time is now clearly worth
its cost. `vendor/sep/VENDOR.md` records this one at the point of use.

### Two build traps, recorded for whoever comes next

**`src/aperture.i` is not a SWIG interface.** Despite the extension, it is a C template
`#include`d twice by `aperture.c` (lines 207 and 246), once per data type. It **must** be
vendored, and it **must not** go in the `cc` source list. Both mistakes fail confusingly.

**`SEP_VERSION_STRING` is a compile-time define with no header fallback.** `util.c:31` is
`const char * const sep_version_string = SEP_VERSION_STRING;`. Upstream's Makefile derives it
from `git describe`; a vendored copy has no git. Without the `-D`, `util.c` does not compile.
With a stale one, `sep_version_string` silently lies — which is exactly the string a future bug
report would quote.

---

## 2. FFI — hand-rolled, no bindgen, and *verified* rather than hoped

**Measured.** The whole binding is ~90 lines of `extern "C"` plus three `#[repr(C)]` structs.
bindgen would have added a build-time libclang dependency — which M2-T01 found is a real cost on
a bare machine — to generate that. Not worth it. **Judgement: hand-roll, and this spike is the
evidence.**

What bindgen genuinely buys is that struct layouts are right *by construction*. That is replaced
by a layout probe: a small MIT C file (`src/layout_probe.c`) reports the C compiler's own
`sizeof`/`offsetof`, and Rust asserts against them at startup.

```
libsep version : 1.4.1
pixstack       : 300000 pixels
  sizeof(sep_image)   = 120 bytes, Rust agrees
  sizeof(sep_bkg)     =  96 bytes, Rust agrees
  sizeof(sep_catalog) = 264 bytes, Rust agrees
  sep_image:   17 field offsets match
  sep_bkg:     13 field offsets match
  sep_catalog: 33 field offsets match
```

**63 field offsets and 3 struct sizes, all checked, every run, before any measurement is taken.**
This is the cheap half of what bindgen provides and it is the half that catches real bugs. A
production binding should keep it: it costs one C file and turns "the mirror is probably right"
into a startup assertion, and it is precisely what a version bump breaks.

The smoke path works end to end: background estimation → in-place subtract → extract → catalogue
with `x, y, flux, peak, a, b, theta, npix, flag`.

---

## 3. 24 MP mono float32 — speed and peak RSS

### The number PRF-05 cares about

**Measured**, single pass, one buffer, with `VmHWM` reset via `/proc/self/clear_refs`
immediately before the first SEP call so the figure isolates extraction from frame loading:

| Frame | Image buffer | **Peak RSS** | **SEP overhead above the image** | Total |
|---|---|---|---|---|
| `light_00001.fits` (session, 5 s) | 91.6 MB | **107.7 MB** | 12.8 MB | 229 ms |
| `deep30.fits` (rendered, 30 s) | 91.6 MB | **108.3 MB** | 13.4 MB | 230 ms |

**Peak RSS for a 24 MP extraction is 108 MB against a 512 MB ceiling — 4.7x headroom.**

The composition is fully understood, which matters more than the number:

* 91.6 MB is the image itself — 6000x4000x4, exactly as the task predicted.
* **13.4 MB is essentially the extraction pixel stack.** Default 300,000 entries x 44 bytes =
  12.59 MB (**derived** from `extract.c:431` and `plistinit` at `extract.c:1058`;
  `pbliststruct` is 8+8+8+4 padded to 32, +4 for `cdvalue`, +8 for `var`/`thresh`). The
  remaining ~0.8 MB is the background spline (5,922 tiles x 4 arrays x 4 B ~= 95 KB), the
  catalogue, and line buffers.
* SEP allocates **nothing proportional to the image** beyond what you hand it. `sep_bkg_subarray`
  subtracts in place; the alternative `sep_bkg_array` would cost a second 91.6 MB and is simply
  not used.

**This is the finding that retires the memory risk.** Extraction's own appetite is a fixed ~13 MB
knob, not a multiple of the frame.

### Throughput

**Measured**, 100 consecutive runs on `deep30.fits`, and 20 on `light_00001.fits`:

| Stage | mean | min | p50 | max |
|---|---|---|---|---|
| background (`sep_background`) | 153 ms | 149 | 153 | 180 |
| subtract (`sep_bkg_subarray`) | ~24 ms | — | — | — |
| extract (`sep_extract`) | **39 ms** | 37 | 38 | 53 |
| **total** | **216 ms** | 211 | 215 | 257 |

Note the shape: **background estimation is 70% of the cost and extraction is 18%.** That is the
opposite of the intuition the task's framing carries ("extraction wants the whole image as
float32"), and it matters for optimisation — the thing to speed up, if anything ever needs
speeding up, is the background spline, not the object finder.

### RSS is flat — the rawler discipline, applied

**Measured.** The M2-T01 rule was "flat RSS across repeated decodes is the PRF-05 condition".

| | 20 runs | 100 runs |
|---|---|---|
| `VmRSS` drift (first → last) | +14.8 MB | **+15.0 MB** |
| `VmHWM` drift | +1.6 MB | +1.7 MB |
| object count | 37, every run | 107, every run |

**The drift is identical at 5x the run count**, so it plateaus immediately and is glibc arena
behaviour, not a leak. Had it been a leak, 100 runs would have shown 5x the drift. This is why
the check was run twice at different lengths rather than once — a single +15 MB figure is
ambiguous and would have had to be reported as a suspicion.

Object counts are bit-stable across every run. `extract.c` reseeds its RNG per call
(`randseed = 1`) specifically so that deblending is reproducible; that promise holds.

### The Pi — stated as extrapolation, because that is what it is

**Derived, not measured. No Pi was involved in this spike.**

At the task's 4–6x factor, 230 ms becomes **0.9–1.4 s** for a 24 MP frame. For a plate-solve
that runs once per slew that is comfortable; for a per-frame loop it would not be.

Two caveats that make the honest range wider than 4–6x:

* This host is a Ryzen 9 9950X with very high single-thread throughput and large caches. The
  workload is memory-bandwidth-bound (a 91.6 MB buffer, streamed several times), and a Pi's
  bandwidth deficit is larger than its clock deficit. **6x may be optimistic.**
* Memory does **not** scale with the CPU. The 108 MB is a property of the data and the
  algorithm, so it transfers to the Pi essentially unchanged. That is the number PRF-05 asks
  about, and it is the one that transfers most safely.

---

## 4. Ground truth — completeness, false positives, centroid error

**Measured**, and this is the one result with a real answer to check against rather than another
detector's opinion: the simulator's sky is procedural, so for a given seed and pointing the true
star positions are *computable* by the same code that rendered the frame.

Both frames use seed `0x415354524F43544C` ("ASTROCTL"), M42, 0.7673"/px, 6000x4000, 980 true
stars in frame. Match tolerance **3 px** (2.3", under the 3.0" FWHM rendered).

### `light_00001.fits` — the session's own frame, 5 s

```
37 detections · 37 matched · 0 spurious  ->  false-positive rate 0.0%
centroid RMS 0.4475 px radial  (0.2934 px x, 0.3379 px y)  =  0.3434 arcsec
median 0.3163 px · max 1.1468 px
```

### `deep30.fits` — rendered, 30 s, to reach deeper

```
107 detections · 107 matched · 0 spurious  ->  false-positive rate 0.0%
centroid RMS 0.4585 px radial  (0.2986 px x, 0.3479 px y)  =  0.3518 arcsec
median 0.2587 px · max 1.5681 px
```

| mag bin | truth | found | completeness | centroid RMS (px) |
|---|---|---|---|---|
| 11–12 | 1 | 1 | 100% | **0.0150** |
| 12–13 | 1 | 1 | 100% | **0.0223** |
| 13–14 | 4 | 4 | 100% | **0.0680** |
| 14–15 | 14 | 14 | 100% | **0.1308** |
| 15–16 | 42 | 42 | 100% | 0.2892 |
| 16–17 | 188 | 45 | 23.9% | 0.6450 |
| 17–18 | 730 | 0 | 0% | — |

**Zero false positives on both frames.** Not one of 144 detections was spurious. At 1.5 sigma over
24 million pixels, pure noise would produce a great many; the 3x3 convolution filter and the
5-pixel minimum area are doing exactly what they are for.

**Completeness is 100% down to magnitude 16 and then falls off a cliff.** The cliff is the
photometry, not SEP: at 30 s the threshold is 291.7 ADU and a magnitude-17 star delivers a peak
of roughly that, so the 16–17 bin is where stars cross the detection threshold. The 5 s frame's
cliff sits ~1.9 magnitudes brighter, which is the 6x exposure ratio (2.5*log10(6) = 1.94 mag) —
the two frames agree with each other and with the photometry, which is the cross-check that says
the scoring harness is not fooling itself.

### The centroid number the guiding loop inherits — read this carefully

The headline **0.4585 px RMS is dominated by the faintest detections** and is the wrong number to
quote for guiding. A guide loop picks *bright* stars. For magnitude < 15 the RMS is
**0.015–0.13 px**, i.e. **0.012–0.10 arcsec** at this plate scale.

**Derived:** at the 1000 mm reference rig, 0.1 px is 0.077" — roughly 0.4 mount counts
(one count = 0.187 px of star motion, per the HEQ5 spike). So SEP's centroid precision on a
bright star is **well below one motor step**, and the guiding loop's error budget will be
dominated by seeing, mount backlash and the correction cadence — never by the star detector.
That is the useful form of this result.

The per-axis figures (0.30 px x, 0.35 px y) are each ~1/sqrt(2) of the radial, as they should be
for isotropic error; a guide loop corrects axes independently and inherits the per-axis number.

---

## 5. Python cross-check — bit-identical

**Measured.** Python `sep` 1.4.1 wraps the *same* C core as the vendored 1.4.1 source, so this is
the sharpest possible test of whether the hand-rolled FFI drives it correctly. Same file, same
parameters (bw/bh 64, fw/fh 3, thresh 1.5 sigma, minarea 5, default 3x3 kernel, deblend 32/0.005,
clean on).

| | `light_00001` | `deep30` |
|---|---|---|
| object count | 37 = 37 | 107 = 107 |
| paired within 0.5 px | 37 / 37 | 107 / 107 |
| **centroid dx, dy** | **exactly 0.0** | **exactly 0.0** |
| **bit-identical centroids** | **37 of 37** | **107 of 107** |

**Every centroid agrees to the last bit of a `f64`.** Not "within tolerance" — identical.

The `f32` fields (`flux`, `peak`, `a`, `b`, `theta`) differ by at most 5e-3 ADU on fluxes up to
4.2e5. That residual is **sub-ULP** — one `f32` ULP at that magnitude is 3.1e-2 — so it
cannot be a difference between two `f32` values. It is the decimal text round-trip through the
CSV the two sides are compared over, and it disappeared entirely for the `f64` centroids once
the CSV was written at full round-trip precision instead of `{:.6}`.

**Tolerance, stated:** none was needed. The claim is exact equality on centroids and sub-ULP on
the `f32` catalogue fields.

Python timing on the same frame, for reference: background 174 ms, subtract 26 ms, extract 44 ms,
total 243 ms — against Rust's 230 ms. The two paths are the same C code, so the ~5% is process
noise and numpy's array handling, not a Rust advantage worth claiming.

---

## 6. The API subset a production binding needs

**Measured** — this is the complete set the spike actually called, and it did everything asked of
it. libsep's header exposes ~30 functions; a plate-solve/guiding front-end needs **eight**.

### Required

| Function | Why |
|---|---|
| `sep_background` | build the spline |
| `sep_bkg_globalrms` | the sigma the detection threshold is relative to |
| `sep_bkg_subarray` | **in-place** subtraction — the reason 24 MP fits in 108 MB |
| `sep_bkg_free` | |
| `sep_extract` | the catalogue |
| `sep_catalog_free` | |
| `sep_get_errmsg` | turn a status int into a diagnosable message |
| `sep_bkg_global` | not strictly required; one line, and worth logging |

### Worth binding, not on the hot path

`sep_set_extract_pixstack` / `sep_get_extract_pixstack` (see §7a — this is not optional in
practice), `sep_version_string` (log it; it is the first thing a bug report needs).

### Structs

`sep_image` (17 fields), `sep_bkg` (13), `sep_catalog` (33). Only `sep_image` is constructed by
the caller, and for a mono float32 frame with no noise/mask/segmap planes, **eight of its
seventeen fields are zero or null**.

### Not needed at all

The entire aperture-photometry half — `sep_sum_circle`, `sep_sum_ellipse`, `sep_kron_radius`,
`sep_flux_radius`, `sep_windowed`, and friends. Roughly 29 KB of `aperture.c`. It still compiles
(it is one translation unit among eight and costs nothing to build), but nothing in a plate-solve
or guiding path calls it. Should photometry ever be wanted, it is already linked.

**So: `sep_extract` + `sep_background` and their teardown is genuinely the whole of it.** The
"small is likely" guess in the task was right — a production `astroctl-sep` is a few hundred
lines including the safety wrappers, not a generated monster.

---

## 7. Surprises — the things that change how this gets built

### 7a. The pixel stack is a hard failure, and its auto-grow is dead code

**Measured, and this is the biggest practical trap.**

`sep_extract` maintains a fixed-size stack of "active object pixels". Default **300,000**. When
it fills, extraction **fails** with status 2, `internal pixel buffer full`.

The library *contains* code to grow the stack on demand — and it is unreachable. `extract.c:640`
does `goto exit` immediately before it, with an upstream comment:

> *"The code in the rest of this block increases the stack size as needed. Currently, it is never
> executed. This is because it isn't clear that this is a good idea: most times when the stack
> overflows it is due to user error: too-low threshold or image not background subtracted."*

Consequences for a production binding, all of them real:

1. **`PIXSTACK_FULL` is an expected error, not a panic.** A frame taken at dawn, through cloud,
   with the lens cap off, badly out of focus, or on a bright moonlit sky can trip it. The binding
   must return it as a typed error the caller can act on ("frame unsuitable, skip it"), never
   `unwrap`.
2. **The setter is process-global.** `static _Atomic size_t extract_pixstack` — it is *not* a
   per-call parameter. A field node running guiding and solving in one process shares one value.
   This is a startup decision.
3. **Raising it costs real memory**, linearly: **44 bytes per entry** (derived from the struct,
   confirmed by measurement to 44.0 B/entry across four doublings). Measured on a 24 MP star
   field:

| pixstack | peak RSS | extract time | objects |
|---|---|---|---|
| 300,000 (default) | 200.2 MB | 38 ms | 107 |
| 1,200,000 | 240.0 MB | 44 ms | 107 |
| 4,800,000 | 390.8 MB | 69 ms | 107 |
| 19,200,000 | 995.1 MB | 160 ms | 107 |

*(These are cumulative `VmHWM` in one process and include the harness's retained copy; the
interesting quantity is the step, which is a clean 44 B/entry.)*

**For a star field the default is ample** — 107 objects, and the default was never close to
filling. Do not raise it speculatively: at 19.2 M it alone would blow the 512 MB budget.

### 7b. SEP's cost tracks above-threshold area, not pixel count

**Measured, and it inverts the intuition the whole spike was framed around.**

| Frame | pixels | above threshold | objects | extract time |
|---|---|---|---|---|
| 24 MP star field | 24.0 M | tiny | 107 | **39 ms** |
| Real R10 CR3, one CFA plane | 6.3 M | 21.9% | 4,312 | **900 ms** |

**A quarter of the pixels, twenty-three times the extraction time.** The real frame is a lit
indoor room — the M2-T01 spike's genuine R10 capture — where a fifth of the sensor sits above
threshold in enormous connected regions. It also needs the pixstack raised past ~1,000,000 to
complete at all, and the full Bayer mosaic never completes even at 19.2 M.

This is **not a defect for our workload**: SEP is built for astronomical frames, where objects
are small and sparse, and on those it is fast. But it means the honest statement of the
performance result is *"39 ms for a star field"*, never *"39 ms for 24 megapixels"* — and a field
node needs the §7a error path for the nights when the frame is not a star field.

**Recorded as a limit of the real-sensor arm:** the CR3s available are of a room and contain no
stars, so they answer the decode, buffer and memory questions on genuine sensor data (rawler
decode 158–172 ms, 95.9 MB float32, 199 MB peak) and they answer the pixstack question
emphatically — but they cannot say anything about accuracy.

### 7c. The FITS row-order trap — it scores 0%, not "slightly worse"

**Measured, by walking into it.**

`Exposure::render` returns rows top-first. The simulator's FITS writer emits them **in reverse**
(`for row in (0..height).rev()`, `simulator/fits.rs:107`) because FITS numbers rows from the
bottom. So anything reading the file linearly — this spike, astropy, `sep`, DS9 — sees a
vertically mirrored copy of the renderer's coordinate system.

The first scoring run produced **37 plausible detections, 980 truth stars, and zero matches.**
Zero. Not a degraded RMS — nothing at all. The failure looks exactly like a broken detector, and
the natural response is to go and tune the threshold, which would have wasted the afternoon and
"fixed" nothing.

Anyone scoring a detector against simulator truth must flip rows
(`Truth::into_fits_row_order`). Anyone comparing against a *FITS world* coordinate must also
remember FITS is 1-indexed, where SEP is 0-indexed — that one is a half-pixel-scale error that
would look like a plausible measurement rather than an obvious break, which makes it worse.

**Confirmed non-issue:** the simulator's pixel-centre convention (sample `i` sits at coordinate
exactly `i`) already matches SEP's. No half-pixel shift is needed, and adding one "for FITS"
would introduce a 0.5 px bias.

### 7d. SEP 1.4.1 is thread-safe — verifiable from the source

**Derived** from reading the C, not measured by running threads.

All per-call mutable state is `_Thread_local`: `plistsize`, the `plistoff_*`/`plistexist_*`
offsets, `randseed` (`extract.c:41-44`), and the error-detail buffer (`util.c:32`). The only
shared mutable state is the two `_Atomic` tunables — `extract_pixstack` and `nsonmax`.

So concurrent `sep_extract` calls on different images in one process are safe. This is
load-bearing for a field node that may guide and solve at once, and it is **not** something to
assume for other versions: this is a property of 1.4.1's `_Thread_local` annotations, and the
version pin in `VENDOR.md` is what preserves it. Not exercised under concurrency here.

### 7e. Python pays a 96 MB copy that Rust does not

**Measured.** FITS is big-endian, so astropy hands back `dtype('>f4')` and `sep` rejects
non-native byte order outright. The standard fix — `.astype(np.float32)` — copies the entire
image: **96 MB at 24 MP**, on top of the 96 MB already there.

A Rust binding owns the decode and produces native `f32` directly, so it never pays this. It is a
small, concrete argument for the decision already taken (binding over Python), independent of the
ARC-06 node-placement argument that motivated it.

---

## What this does NOT prove

Bluntly, because the numbers above are strong enough to be over-read:

* **Nothing ran on a Pi.** Every timing is on a Ryzen 9 9950X with 127 GB of RAM. The 4–6x figure
  is the task's assumption carried forward, and §3 argues it may be optimistic for a
  bandwidth-bound workload. The *memory* figure transfers; the *time* figure is a guess with a
  measurement attached to one end.
* **No real star field was extracted.** The accuracy results are all against the simulator, whose
  PSF is a clean Gaussian with no optical aberration, no field rotation, no gradient, no
  nebulosity, no cosmic rays, no satellite trails, no hot pixels — `sky.rs` says so itself. Real
  frames have all of these and every one of them makes extraction harder. The real CR3s available
  contain a room, not stars.
* **Centroid RMS is measured against a renderer that shares its projection code with the truth
  computation.** Both call `Projection::to_pixel`. So this measures SEP's centroiding against the
  simulator's *rendering*, which is the right thing to measure, but it cannot catch an error in
  the projection itself — that would cancel.
* **No plate solve was performed.** This spike produces a star catalogue. Whether astrometry.net
  (or anything else) can solve from that catalogue, and how fast, is untouched and is the actual
  Phase 2a deliverable. SEP is the input to that question, not an answer to it.
* **Thread safety was read, not exercised.** No concurrent extraction was run.
* **Not tested:** noise/mask/segmap planes, non-float dtypes, `SEP_FILTER_MATCHED`,
  windowed positions (`sep_windowed`, which is what one would actually use for sub-pixel guiding
  centroids and may well be *better* than the barycentre measured here), aperture photometry,
  and any frame that is not 6000x4000.
* **The licence conclusion is an engineer reading a licence.** §1 sets out the constraint and the
  routes; it is not legal advice and the static-linking question deserves a real decision.

---

## Reproducing

```
cargo build --release                      # vendored C + Rust, no system deps

./target/release/sep-extraction-spike layout          # FFI struct-layout check + version
./target/release/sep-extraction-spike render out/deep30.fits 30 18
./target/release/sep-extraction-spike once   out/deep30.fits   # production-shape peak RSS
./target/release/sep-extraction-spike bench  out/deep30.fits 100
./target/release/sep-extraction-spike truth  out/deep30.fits   # score vs computed truth
./target/release/sep-extraction-spike pixstack out/deep30.fits # the 44 B/entry sweep
./target/release/sep-extraction-spike cr3plane <a real .cr3>   # real sensor data

python3 -m venv venv && ./venv/bin/pip install sep numpy astropy
./venv/bin/python python/crosscheck.py out/deep30.fits out/deep30.rust-catalog.csv
```

`truth` also works directly on the session's own frames — they carry `SIMSEED`, `RA`, `DEC`,
`XPIXSZ` and `FOCALLEN`, which is everything the truth computation needs.

The workspace is untouched: `spikes/` is excluded in the root manifest, this directory carries
its own `[workspace]` stanza, and `scripts/check.sh` passes exactly as before.
