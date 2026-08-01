# Vendored: SEP (Source Extraction and Photometry)

Everything in `src/` and `licenses/` is upstream, unmodified. Nothing in this directory is ours
except this file.

## Provenance

| | |
|---|---|
| Upstream | https://github.com/kbarbary/sep |
| Version | **1.4.1** (`git describe` → `v1.4.1-1-g93b3ac5`) |
| Commit | `93b3ac52e0f6cb26449204dc8bc8c3cf65602f0f` |
| Commit date | 2025-02-18 |
| Vendored on | 2026-08-01 |
| Files taken | `src/*.c`, `src/*.h`, `src/aperture.i`, `licenses/*`, `AUTHORS.md` |
| Modifications | **none** — byte-for-byte copies |

SHA-256 of the three files most likely to matter in a diff:

```
4cdd8056988bd4ff1ae63c43fef2adbfc27a839960c5d896138b416b437fbebd  src/extract.c
441335da11fd3d7d0a9b7c22af45630c6750cfeeec63ecf038af02bcba36bf6a  src/background.c
beb4465abdcbdbdaac942c280ce2128f5a800c10040a3ba885d35121bcf447e3  src/sep.h
```

## Licence — READ THIS BEFORE SHIPPING

**The library as a whole is LGPLv3.** Per-file:

| Files | Licence | Origin |
|---|---|---|
| `analyse.c` `aperture.c` `background.c` `convolve.c` `deblend.c` `extract.c` `lutz.c` `util.c` `sep.h` `sepcore.h` `extract.h` | **LGPLv3** | derived from Source Extractor |
| `overlap.h` | BSD 3-clause | derived from photutils |
| (`sep.pyx`, not vendored) | MIT | the Python wrapper |

AstroCtl is MIT. Statically linking LGPLv3 code into an MIT binary is the constraint — see
`../../FINDINGS.md` §1, which is where the consequences and the options are worked through.
Do not treat this as a formality; it is the finding most likely to change the recorded decision.

## What is deliberately NOT vendored

* `sep.pyx`, `setup.py`, `pyproject.toml` — the Python packaging. Not needed; we are the wrapper.
* `ctest/`, `test.py`, `bench.py` — upstream's own tests.
* `data/`, `docs/`, `paper/` — documentation and sample data.
* `CMakeLists.txt`, `Makefile` — we build with the `cc` crate; see `../../build.rs`.

## The one non-obvious file

`src/aperture.i` is **not** a SWIG interface despite the extension. It is a C template
`#include`d twice by `aperture.c` (lines 207 and 246), once per data type. It must be vendored,
and it must **not** be added to the `cc` build's source list. Omitting it from the tree gives a
confusing failure at compile time; compiling it as a translation unit gives another.

## Updating

1. `git clone https://github.com/kbarbary/sep && git checkout <tag>`
2. Copy `src/*.c`, `src/*.h`, `src/aperture.i`, `licenses/*`, `AUTHORS.md`.
3. Update the version, commit and SHAs in this file.
4. Update `SEP_VERSION_STRING` in `../../build.rs` — it is a compile-time define with no header
   fallback, so a stale value silently misreports `sep_version_string`.
5. Re-run `sep-extraction-spike layout`. The struct layouts are the thing a version bump breaks,
   and that command is what notices.
