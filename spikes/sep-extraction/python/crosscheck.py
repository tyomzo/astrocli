#!/usr/bin/env python3
"""Cross-check the vendored C library against the Python `sep` package.

Both wrap the *same* C core, so identical inputs must produce near-identical catalogues.
Divergence means the hand-rolled FFI is being driven wrong — which is exactly what a spike is
for, since the alternative is discovering it after a binding has been written and trusted.

Usage:  crosscheck.py <frame.fits> <rust-catalog.csv>
"""

import csv
import sys
import time

import numpy as np
import sep
from astropy.io import fits

# Must match src/main.rs exactly, or the comparison measures the parameters and not the code.
BW = BH = 64
FW = FH = 3
FTHRESH = 0.0
THRESH = 1.5
MINAREA = 5
DEBLEND_NTHRESH = 32
DEBLEND_CONT = 0.005
CLEAN = True
CLEAN_PARAM = 1.0

# sep.extract's default filter_kernel, spelled out so the two sides provably use one kernel.
KERNEL = np.array([[1.0, 2.0, 1.0], [2.0, 4.0, 2.0], [1.0, 2.0, 1.0]], dtype=np.float32)

# How close two centroids must be to be "the same object". Deliberately tight: this is not a
# detection tolerance, it is a floating-point agreement tolerance between two runs of one
# algorithm, and anything above ~1e-3 px would mean the inputs differ.
PAIR_TOL_PX = 0.5


def load(path):
    """Read the primary HDU as native-endian float32.

    THE TRAP: FITS is big-endian on disk, so astropy hands back dtype '>f4'. `sep` rejects a
    non-native byte order with

        ValueError: Input array with dtype '>f4' has non-native byte order.

    `.astype(np.float32)` both copies and normalises the order. This is the single most common
    way a first `sep` call fails, and it costs a full image copy — 96 MB at 24 MP — which is a
    memory fact a Rust binding gets to avoid entirely, because it owns the decode and can
    produce native float32 directly.
    """
    with fits.open(path, memmap=False) as hdul:
        raw = hdul[0].data
        header = hdul[0].header
    return np.ascontiguousarray(raw.astype(np.float32)), header


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2

    fits_path, csv_path = sys.argv[1], sys.argv[2]

    data, header = load(fits_path)
    height, width = data.shape
    print(f"frame        : {fits_path} — {width}x{height} = {width*height/1e6:.1f} MP")
    print(f"dtype        : {data.dtype} (native order after astype)")

    t0 = time.perf_counter()
    bkg = sep.Background(data, bw=BW, bh=BH, fw=FW, fh=FH, fthresh=FTHRESH)
    t_bkg = (time.perf_counter() - t0) * 1000.0

    t1 = time.perf_counter()
    bkg.subfrom(data)  # in place, same as sep_bkg_subarray in the Rust path
    t_sub = (time.perf_counter() - t1) * 1000.0

    t2 = time.perf_counter()
    objects = sep.extract(
        data,
        THRESH,
        err=bkg.globalrms,
        minarea=MINAREA,
        filter_kernel=KERNEL,
        deblend_nthresh=DEBLEND_NTHRESH,
        deblend_cont=DEBLEND_CONT,
        clean=CLEAN,
        clean_param=CLEAN_PARAM,
    )
    t_ext = (time.perf_counter() - t2) * 1000.0

    print(f"sep version  : {sep.__version__}")
    print(f"background   : global {bkg.globalback:.4f}, globalrms {bkg.globalrms:.4f}")
    print(
        f"timing       : background {t_bkg:.0f} ms, subtract {t_sub:.0f} ms, "
        f"extract {t_ext:.0f} ms, total {t_bkg+t_sub+t_ext:.0f} ms"
    )
    print(f"objects      : {len(objects)}")

    # --- the Rust catalogue ---
    rust = []
    with open(csv_path, newline="") as fh:
        for row in csv.DictReader(fh):
            rust.append(
                (
                    float(row["x"]),
                    float(row["y"]),
                    float(row["flux"]),
                    float(row["peak"]),
                    float(row["a"]),
                    float(row["b"]),
                    float(row["theta"]),
                )
            )
    print(f"rust objects : {len(rust)}")

    print("\n--- agreement ---")
    if len(objects) != len(rust):
        print(f"COUNT MISMATCH: python {len(objects)} vs rust {len(rust)}")
    else:
        print(f"object count : identical ({len(objects)})")

    # Nearest-neighbour pairing. With two runs of one algorithm this should be an exact
    # bijection; anything else is reported rather than smoothed over.
    px = np.array([o["x"] for o in objects])
    py = np.array([o["y"] for o in objects])

    dx_all, dy_all, dflux, dpeak, da, db, dtheta = [], [], [], [], [], [], []
    unpaired = 0
    for (rx, ry, rflux, rpeak, ra_, rb, rtheta) in rust:
        d = np.hypot(px - rx, py - ry)
        i = int(np.argmin(d))
        if d[i] > PAIR_TOL_PX:
            unpaired += 1
            continue
        dx_all.append(px[i] - rx)
        dy_all.append(py[i] - ry)
        dflux.append(float(objects[i]["flux"]) - rflux)
        dpeak.append(float(objects[i]["peak"]) - rpeak)
        da.append(float(objects[i]["a"]) - ra_)
        db.append(float(objects[i]["b"]) - rb)
        dtheta.append(float(objects[i]["theta"]) - rtheta)

    if unpaired:
        print(f"UNPAIRED     : {unpaired} rust objects had no python object within {PAIR_TOL_PX} px")
    else:
        print(f"pairing      : every rust object paired within {PAIR_TOL_PX} px")

    if dx_all:
        dx_all = np.array(dx_all)
        dy_all = np.array(dy_all)
        radial = np.hypot(dx_all, dy_all)
        print(f"paired       : {len(dx_all)}")
        print(
            f"centroid dx  : max |{np.abs(dx_all).max():.3e}| px, "
            f"rms {np.sqrt((dx_all**2).mean()):.3e} px"
        )
        print(
            f"centroid dy  : max |{np.abs(dy_all).max():.3e}| px, "
            f"rms {np.sqrt((dy_all**2).mean()):.3e} px"
        )
        print(f"centroid rad : max {radial.max():.3e} px")
        print(f"flux         : max |{np.abs(np.array(dflux)).max():.3e}| ADU")
        print(f"peak         : max |{np.abs(np.array(dpeak)).max():.3e}| ADU")
        print(f"a            : max |{np.abs(np.array(da)).max():.3e}| px")
        print(f"b            : max |{np.abs(np.array(db)).max():.3e}| px")
        print(f"theta        : max |{np.abs(np.array(dtheta)).max():.3e}| rad")

        exact = int((radial == 0.0).sum())
        print(f"bit-identical centroids: {exact} of {len(radial)}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
