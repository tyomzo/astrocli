# M2-T01 FINDINGS — Canon EOS R10 via the `gphoto2` crate

**Date:** 2026-07-29 · **Hardware:** Canon EOS R10 (`04a9:32f8`), Sigma 23mm F1.4 DC DN, SD card
127.8 GB / 69.5 GB free, battery 100% · **Host:** x86_64 Ubuntu questing, libgphoto2 2.5.31,
`gphoto2` crate 3.4.1, Rust 1.97.1 · **Camera state:** mode dial on **Bulb**, `imageformat=RAW`,
`capturetarget=Internal RAM`

## Verdict: the top risk is retired

**Bulb works through the crate.** No CLI fallback is required for any operation tested.
`camera.ops_via_cli` should ship as `[]`.

ADD §10 named gphoto2 coverage for the R10 — specifically bulb and CR3 download — as the highest
implementation risk in the project. All of it is covered by the bindings.

## Per-operation results

| # | Operation | Result | Measured |
|---|-----------|--------|----------|
| 1 | Autodetect + connect | works | 190–210 ms |
| 1 | Abilities | works | model `Canon EOS R10`, driver `Production`, ops: capture_image, capture_preview, configure, trigger_capture all true |
| 1 | Full config tree | works | 91 entries in 222 ms → `out/config-tree.txt` |
| 2 | Read settings | works | iso (27 choices), shutterspeed, aperture (22), imageformat (23), capturetarget (2). 0.4–10 ms per key |
| 2 | Write setting + restore | works | ISO 1600 → 100 → 1600, 11 ms per write |
| 3 | Timed capture | works | 2.08 s trigger→file-ready |
| 3 | Download to disk | works | **32.0 MB** full RAW |
| 4 | **Bulb** | **works** | `eosremoterelease` → `Press Full` / `Release Full`; camera reported `BulbExposureTime 9` for a 10 s hold; `NewFile` event delivered the CR3 |
| 5 | Live view | works | **58.5 fps**, 133 KB/frame, worst frame 390 ms (first frame, LV startup) |
| 6 | Battery / storage / lens | works | `batterylevel=100%`, storage + free space, lens name reported |
| 7 | Cable-pull recovery | **works, with a caveat** | disconnect detected in-flight; stale handle unrecoverable; fresh context reconnects in 108 ms — *unless gvfs grabs the device on replug*, see below |
| 8 | `rawler` CR3 decode | works | see below |

## rawler on a real R10 file

```
dimensions : 6192 x 4060          (raw incl. masked border; PRD quotes 6000×4000 active)
CFA        : RGGB                 matches rawler's r10.toml profile
black/white: 2047 / 12735
decode     : 69 ms first, 31.9 ms mean over 20 consecutive
peak RSS   : 171 MB -> 172 MB across 20 decodes
```

Flat RSS across repeated decodes is the PRF-05 condition, and it holds. `rawler` is confirmed as
the decoder; nothing about the earlier build-evidence selection needs revisiting.

## Three findings that change the design

**1. `download_to` refuses to overwrite an existing file** — it returns `File exists` rather than
truncating. SDD §5.3.2 downloads to `.tmp_<id>.cr3` before the fsync-rename, so a crash that
leaves a stale tmp file would make the retry fail permanently. **The driver must unlink the tmp
path before every download.** Cheap to do, invisible until it bites.

**2. With `capturetarget=Internal RAM`, the USB transfer happens inside `capture_image()`, not
inside `download_to()`.** Evidence: capture took 2.08 s, then writing 32 MB to disk took 2.67 ms —
roughly 12 GB/s, which is memory bandwidth, not USB. libgphoto2 buffers the whole frame in RAM
first. SDD §5.3.2 describes the download as "streamed to temp file", which is not what happens.
Consequence for PRF-05: a full-size frame (32 MB) is resident inside libgphoto2 during every
capture. That is affordable against the 512 MB budget, but it should be stated rather than
discovered. Setting `capturetarget=Memory card` would change this trade and is worth testing when
the sequencer arrives.

**3. Full RAW is 32 MB, not the ~25 MB the PRD assumes** — and frame size varies hugely with
content: the dark bulb frame compressed to 1.7 MB, the lit frame was 32 MB. PRF-07's transfer
budget and the §8.3(7) bufferbloat rule should use 32 MB as the planning figure.

Minor: with the mode dial on Bulb, `shutterspeed` offers only `bulb` and `Unknown value df00`.
The dial position constrains what the API can set — the driver cannot assume a shutter speed is
settable, and the UI should surface the camera's physical mode.

## Still open

Also untested, deliberately: `capturetarget=Memory card`, mirror-lockup equivalents, long-run
thermal behaviour, and any of this on the actual field node (a Pi's USB stack is not this
workstation's).

## Reproducing

The host had no libgphoto2 at all; it was staged into a local prefix without touching the system
(`apt-get download` + `dpkg -x`, then `PKG_CONFIG_PATH` / `LD_LIBRARY_PATH` / `CAMLIBS` / `IOLIBS`
pointed at it; bindgen additionally needed `LIBCLANG_PATH` and the clang builtin-header include).
For real work, install `libgphoto2-dev` properly.

```sh
cargo +1.97.1 build --release
./target/release/gphoto2-r10-spike 1 2 6     # safe: no shutter
./target/release/gphoto2-r10-spike 4         # bulb, 10 s
./target/release/gphoto2-r10-spike 5         # live view, 15 s
./target/release/gphoto2-r10-spike 8         # decode newest CR3 in out/
```

---

## Step 7 — cable pull / REL-03 recovery (executed 2026-07-29)

Live view as the traffic generator; cable pulled after 927 frames / 31.3 s.

**Disconnect is detected cleanly and — importantly — is a *distinguishable* error:**

| Condition | libgphoto2 error |
|---|---|
| Cable physically removed | `Could not find the requested device on the USB port` |
| Device present but claimed by something else | `Could not claim the USB device` |

The driver can therefore tell "camera unplugged" from "something else has it" and say so, instead
of surfacing one opaque message for both. M2-T04's recovery path should branch on this.

**The old handle never recovers.** Five retries on the existing `Camera` after replug all failed
with the same error. This confirms SDD §5.3.1's design: abandon the thread and context, spawn
fresh. There is no cheaper path.

### gvfs does not merely annoy — it breaks REL-03

After replug, a fresh `Context` + autodetect failed for **80 seconds straight** with
`Could not claim the USB device`. Cause: **gvfs auto-mounted the camera on hotplug**
(`gphoto2:host=Canon_Inc._Canon_Digital_Camera_A0000...`). Releasing the mount produced immediate
recovery — autodetect in **108 ms**.

So the camera itself recovers perfectly. The entire 80-second failure was the desktop environment
taking the device out from under the recovery path.

This also **settles the hotplug question left open earlier**: gvfs was recorded as suppressible in
principle but untested against a real replug. It has now been tested, negatively — gvfs *does*
grab on hotplug, and it does so precisely when the recovery path needs the device.

This escalates the finding. REL-03 ("USB disconnect detection and graceful recovery") is a **Must**
requirement, and on any field node running a desktop session it would fail — not at startup, where
it is merely confusing, but in the middle of a night, after a cable knock, exactly when recovery
matters. The prevention steps recorded in `../skywatcher-heq5/FINDINGS.md` are therefore not
optional hardening; they are a precondition for REL-03 on a non-headless field node.
