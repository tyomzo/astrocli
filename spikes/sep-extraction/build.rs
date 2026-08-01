//! Vendor-builds libsep with the `cc` crate — question 1 of the spike, answered by this file
//! existing and working.
//!
//! The whole point is that there is no `pkg-config`, no `bindgen`, no system package and no
//! CMake here: eight `.c` files and a compiler. If that claim were false this build script
//! would be where it broke.

fn main() {
    let src = "vendor/sep/src";

    // Every `.c` in the upstream Makefile's OBJS list, and no others. `aperture.i` is NOT in
    // this list and must not be — see the note below.
    let sources = [
        "analyse.c",
        "aperture.c",
        "background.c",
        "convolve.c",
        "deblend.c",
        "extract.c",
        "lutz.c",
        "util.c",
    ];

    let mut build = cc::Build::new();
    build.include(src);

    for file in sources {
        build.file(format!("{src}/{file}"));
    }

    // The spike's own struct-layout probe, compiled against the same `sep.h` the library sees.
    // MIT, ours, and the reason the hand-rolled binding is checkable rather than merely
    // plausible — see src/layout_probe.c.
    build.file("src/layout_probe.c");

    // `sep_version_string` is a compile-time define, not a header constant. Without it
    // `util.c` fails to compile — the symbol is simply undefined. Upstream's Makefile derives
    // it from `git describe`; a vendored copy has no git, so it is pinned to the tag the
    // vendored sources were taken from. See vendor/sep/VENDOR.md.
    build.define("SEP_VERSION_STRING", "\"1.4.1\"");

    // Upstream builds at -O3. `cc` defaults to -O2 for release profiles, and the difference is
    // measurable on a 24 MP frame, so it is set explicitly rather than inherited.
    build.opt_level(3);

    // Upstream passes `-Wall -Wextra -Wcast-qual` with `-Werror` deliberately commented out,
    // which reads like a codebase that has warnings it tolerates. It does not: at exactly those
    // flags, gcc 15.2.0 emits **zero warnings** across all eight translation units (measured,
    // FINDINGS §1). So they stay on — the usual reason a vendored C library gets its warnings
    // silenced does not apply here, and leaving them on means a future version bump that
    // introduces one is visible instead of swallowed.
    build.warnings(true);
    build.flag_if_supported("-Wcast-qual");

    build.compile("sep");

    // libm. glibc keeps the math functions in a separate object from libc; Rust's std happens
    // to pull it in on this target already, but relying on that is relying on an implementation
    // detail of another crate's link line.
    println!("cargo:rustc-link-lib=m");

    // Rerun triggers. `aperture.i` is included here and NOT in `sources`: it is a C template
    // `#include`d twice by aperture.c (once per dtype), not a translation unit. Compiling it
    // as one is an error; omitting it from the vendored tree is a *link* error much later.
    println!("cargo:rerun-if-changed=build.rs");
    for file in sources {
        println!("cargo:rerun-if-changed={src}/{file}");
    }
    for header in [
        "sep.h",
        "sepcore.h",
        "extract.h",
        "overlap.h",
        "aperture.i",
    ] {
        println!("cargo:rerun-if-changed={src}/{header}");
    }
    println!("cargo:rerun-if-changed=src/layout_probe.c");
}
