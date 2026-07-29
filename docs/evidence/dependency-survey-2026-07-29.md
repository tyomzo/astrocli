# Dependency survey — build evidence

**Date:** 2026-07-29 · **Host:** x86_64 Ubuntu workstation · **Method:** each crate added to an
isolated `cargo new` project and `cargo check`ed, so no failure masks another. Metadata claims
were then confirmed against the fetched crate sources.

This closes the *non-hardware* half of the M2-T01 spike and the erfa question outright. It does
**not** close M2-T01 or M3 — see "Still open" below.

## Verdicts

| Question | Verdict | Evidence |
|----------|---------|----------|
| Toolchain pin | **1.97.1**, not 1.94.0 | 1.94.0 (2026-03-02) is the installed default; current stable is 1.97.1 (2026-07-14). `rusqlite` 0.40.1 → `libsqlite3-sys` 0.38.1 **fails on 1.94.0** — its build script uses the unstable `cfg_select!` (E0658). Builds cleanly on 1.97.1. *Note added after M0-T01: with `rust-version = "1.97"` declared in the workspace manifest, cargo refuses earlier and more clearly — `rustc 1.94.0 is not supported by the following package` — so nobody working in this repo will actually meet the E0658. The finding stands; it was measured in an isolated probe without a manifest MSRV.* |
| ERFA binding | **`erfars` 0.2.0** | Vendors the real ERFA C source — 251 `.c` files under `external/`, compiled via `cc::Build`, ships `LICENSE-ERFA.txt`. Same library astropy wraps, **and no system dependency** |
| The `erfa` crate | **rejected, as suspected** | 0 C files, 17 Rust files; `src/lib.rs` opens "A pure-Rust equivalent to the ERFA C library". A reimplementation, not a binding |
| `erfa-sys` | not needed | Requires system liberfa; `erfars` supersedes it by vendoring |
| RAW decoder | **`rawler` 0.7.2** | Dedicated `src/decoders/cr3.rs` + `src/decompressors/crx`; camera profile `data/cameras/canon/r10.toml` (RGGB, A and D65 colour matrices); **R10 regression fixtures** for RAW/CRAW/BURST at ISO 100/800/32000 with digest files. Pure Rust, no system libraries |
| `rawloader` | **eliminated** | Zero CR3/CRX references anywhere in the crate |
| `libraw` / `libraw-sys` | **not needed** | Requires system `libraw_r`; both at 0.1.1. `rawler` removes the need entirely |
| Version conflicts | none | All 15 planned crates resolve together in one manifest |

## System libraries — measured, not assumed

Confirmed by isolated build failure:

| Crate | Requires | First needed |
|-------|----------|--------------|
| `serialport` 4.9 | **`libudev`** (via `libudev-sys` 0.1.4) — *previously undocumented* | M3 |
| `fitsio` 0.21 | `cfitsio` | M1 |
| `gphoto2` 3.4 | `libgphoto2` | M2 |

Confirmed to need **nothing**: `tokio`, `axum`, `serde`, `thiserror`, `tracing`, `rusqlite`
(with `bundled`), `erfars`, `rawler`, `rawloader`.

Net effect on PRD §7: **`liberfa-dev` and `libraw-dev` come off** the list, **`libudev-dev` goes
on**. M2's only system dependency is now `libgphoto2-dev`.

`serialport` can be built without udev via `default-features = false`, but that removes port
enumeration by USB VID/PID — which is exactly what MNT-01 auto-detection and M3-T02's
`/dev/serial/by-id/*` scan rely on. Keep udev.

## Still open — hardware required

Nothing below can be settled on this machine: `lsusb` shows no Canon device (no `04a9:*`) and
there is no `/dev/ttyUSB*`. This is a workstation, not the field node.

- **M2-T01, the actual top risk:** bulb via PTP remote release on a real R10. Unchanged and
  unanswered — crate *maturity* is now evidenced, per-operation *coverage* is not
- CR3 download timing, live-view fps and latency, cable-pull wedge behaviour
- `rawler` decode timing and peak RSS on a real R10 file — the half-size decode must hold the
  PRF-05 line (transient buffers only, no growth across repeated decodes). Its R10 fixtures make
  correctness very likely; they say nothing about speed on a Pi
- All of M3: no mount, no USB-serial adapter

## Reproducing

```sh
rustup toolchain install 1.97.1
cargo new probe && cd probe
cargo add tokio --features full
cargo add axum serde thiserror tracing erfars rawler
cargo add rusqlite --features bundled
cargo +1.97.1 check          # expect: clean
```
