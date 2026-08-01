/* Struct-layout probe — MIT, part of the AstroCtl spike harness, not of libsep.
 *
 * A hand-rolled `extern "C"` binding is a *claim* that the Rust `#[repr(C)]` structs match the
 * C ones. Nothing checks that claim: get a field order wrong and the code still compiles, still
 * links, and returns plausible-looking garbage. `bindgen` would make the claim true by
 * construction; the spike's question is whether bindgen is needed at all, so instead the claim
 * is made *checkable* — the C compiler reports its own sizeof/offsetof and Rust asserts against
 * them.
 *
 * This is the cheap half of what bindgen buys, and it is the half that catches real bugs.
 */

#include <stddef.h>
#include <stdint.h>

#include "sep.h"

size_t astroctl_probe_sizeof_image(void) { return sizeof(sep_image); }
size_t astroctl_probe_sizeof_bkg(void) { return sizeof(sep_bkg); }
size_t astroctl_probe_sizeof_catalog(void) { return sizeof(sep_catalog); }

/* Field offsets, by index. A switch rather than one function per field: there are 40-odd
 * fields across three structs and the Rust side walks them in a loop.
 */
size_t astroctl_probe_offset_image(int field) {
  switch (field) {
  case 0: return offsetof(sep_image, data);
  case 1: return offsetof(sep_image, noise);
  case 2: return offsetof(sep_image, mask);
  case 3: return offsetof(sep_image, segmap);
  case 4: return offsetof(sep_image, dtype);
  case 5: return offsetof(sep_image, ndtype);
  case 6: return offsetof(sep_image, mdtype);
  case 7: return offsetof(sep_image, sdtype);
  case 8: return offsetof(sep_image, segids);
  case 9: return offsetof(sep_image, idcounts);
  case 10: return offsetof(sep_image, numids);
  case 11: return offsetof(sep_image, w);
  case 12: return offsetof(sep_image, h);
  case 13: return offsetof(sep_image, noiseval);
  case 14: return offsetof(sep_image, noise_type);
  case 15: return offsetof(sep_image, gain);
  case 16: return offsetof(sep_image, maskthresh);
  default: return (size_t)-1;
  }
}

size_t astroctl_probe_offset_bkg(int field) {
  switch (field) {
  case 0: return offsetof(sep_bkg, w);
  case 1: return offsetof(sep_bkg, h);
  case 2: return offsetof(sep_bkg, bw);
  case 3: return offsetof(sep_bkg, bh);
  case 4: return offsetof(sep_bkg, nx);
  case 5: return offsetof(sep_bkg, ny);
  case 6: return offsetof(sep_bkg, n);
  case 7: return offsetof(sep_bkg, global);
  case 8: return offsetof(sep_bkg, globalrms);
  case 9: return offsetof(sep_bkg, back);
  case 10: return offsetof(sep_bkg, dback);
  case 11: return offsetof(sep_bkg, sigma);
  case 12: return offsetof(sep_bkg, dsigma);
  default: return (size_t)-1;
  }
}

size_t astroctl_probe_offset_catalog(int field) {
  switch (field) {
  case 0: return offsetof(sep_catalog, nobj);
  case 1: return offsetof(sep_catalog, thresh);
  case 2: return offsetof(sep_catalog, npix);
  case 3: return offsetof(sep_catalog, tnpix);
  case 4: return offsetof(sep_catalog, xmin);
  case 5: return offsetof(sep_catalog, xmax);
  case 6: return offsetof(sep_catalog, ymin);
  case 7: return offsetof(sep_catalog, ymax);
  case 8: return offsetof(sep_catalog, x);
  case 9: return offsetof(sep_catalog, y);
  case 10: return offsetof(sep_catalog, x2);
  case 11: return offsetof(sep_catalog, y2);
  case 12: return offsetof(sep_catalog, xy);
  case 13: return offsetof(sep_catalog, errx2);
  case 14: return offsetof(sep_catalog, erry2);
  case 15: return offsetof(sep_catalog, errxy);
  case 16: return offsetof(sep_catalog, a);
  case 17: return offsetof(sep_catalog, b);
  case 18: return offsetof(sep_catalog, theta);
  case 19: return offsetof(sep_catalog, cxx);
  case 20: return offsetof(sep_catalog, cyy);
  case 21: return offsetof(sep_catalog, cxy);
  case 22: return offsetof(sep_catalog, cflux);
  case 23: return offsetof(sep_catalog, flux);
  case 24: return offsetof(sep_catalog, cpeak);
  case 25: return offsetof(sep_catalog, peak);
  case 26: return offsetof(sep_catalog, xcpeak);
  case 27: return offsetof(sep_catalog, ycpeak);
  case 28: return offsetof(sep_catalog, xpeak);
  case 29: return offsetof(sep_catalog, ypeak);
  case 30: return offsetof(sep_catalog, flag);
  case 31: return offsetof(sep_catalog, pix);
  case 32: return offsetof(sep_catalog, objectspix);
  default: return (size_t)-1;
  }
}
