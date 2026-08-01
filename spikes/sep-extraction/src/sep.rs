//! A thin hand-rolled `extern "C"` layer over libsep — question 2 of the spike.
//!
//! No bindgen. The judgement, recorded because the task asked for it: the API this system needs
//! is **two calls and three structs**. bindgen would add a build-time dependency on libclang
//! (which the M2-T01 spike already found is a real cost on a bare machine — it needed
//! `LIBCLANG_PATH` and the clang builtin-header include staged by hand) in exchange for
//! generating about 90 lines. That trade is bad here and would be bad in production.
//!
//! What bindgen *does* buy is that the struct layouts are correct by construction. That is
//! replaced by `assert_layout()`, which asks the C compiler for its own `sizeof`/`offsetof` and
//! compares. See `layout_probe.c`.

use std::ffi::{c_char, c_int, c_short, c_void};
use std::fmt;

// ---------------------------------------------------------------------------------------------
// Constants (sep.h)
// ---------------------------------------------------------------------------------------------

/// `SEP_TFLOAT` — the only dtype this spike uses. Extraction wants float32 and the pipeline
/// produces float32, so the int and double paths are never exercised.
pub const SEP_TFLOAT: c_int = 42;

/// Threshold expressed in units of the background RMS.
pub const SEP_THRESH_REL: c_int = 0;

/// `SEP_NOISE_STDDEV` — `noiseval` is a standard deviation, not a variance. Getting this wrong
/// squares the effective threshold and is invisible except as a suspiciously short catalogue.
pub const SEP_NOISE_STDDEV: c_short = 1;

/// Object was produced by deblending a larger one.
pub const SEP_OBJ_MERGED: c_short = 0x0001;
/// Object touches the image boundary — its centroid and flux are both truncated.
pub const SEP_OBJ_TRUNC: c_short = 0x0002;

// ---------------------------------------------------------------------------------------------
// Structs (sep.h) — mirrored by hand, verified by `assert_layout`
// ---------------------------------------------------------------------------------------------

/// `sep_image`: the input. Data plus optional noise/mask/segmap planes.
#[repr(C)]
#[derive(Debug)]
pub struct SepImage {
    pub data: *const c_void,
    pub noise: *const c_void,
    pub mask: *const c_void,
    pub segmap: *const c_void,
    pub dtype: c_int,
    pub ndtype: c_int,
    pub mdtype: c_int,
    pub sdtype: c_int,
    pub segids: *mut i64,
    pub idcounts: *mut i64,
    pub numids: i64,
    pub w: i64,
    pub h: i64,
    pub noiseval: f64,
    pub noise_type: c_short,
    pub gain: f64,
    pub maskthresh: f64,
}

impl SepImage {
    /// An image with no noise plane, no mask and no segmap — the ordinary case.
    ///
    /// `data` must outlive the `SepImage` and every call made with it. That lifetime obligation
    /// is the whole unsafety of this binding and is the one thing a production version must
    /// encode in the type system rather than in a doc comment.
    pub fn mono_f32(data: &[f32], width: i64, height: i64) -> Self {
        assert_eq!(
            data.len() as i64,
            width * height,
            "SepImage::mono_f32: {} samples for a {width}x{height} frame",
            data.len()
        );
        Self {
            data: data.as_ptr().cast::<c_void>(),
            noise: std::ptr::null(),
            mask: std::ptr::null(),
            segmap: std::ptr::null(),
            dtype: SEP_TFLOAT,
            ndtype: 0,
            mdtype: 0,
            sdtype: 0,
            segids: std::ptr::null_mut(),
            idcounts: std::ptr::null_mut(),
            numids: 0,
            w: width,
            h: height,
            noiseval: 0.0,
            noise_type: SEP_NOISE_STDDEV,
            gain: 0.0,
            maskthresh: 0.0,
        }
    }
}

/// `sep_bkg`: the spline representation of a spatially varying background.
#[repr(C)]
#[derive(Debug)]
pub struct SepBkg {
    pub w: i64,
    pub h: i64,
    pub bw: i64,
    pub bh: i64,
    pub nx: i64,
    pub ny: i64,
    pub n: i64,
    pub global: f32,
    pub globalrms: f32,
    pub back: *mut f32,
    pub dback: *mut f32,
    pub sigma: *mut f32,
    pub dsigma: *mut f32,
}

/// `sep_catalog`: the output. A struct of arrays, each `nobj` long.
#[repr(C)]
#[derive(Debug)]
pub struct SepCatalog {
    pub nobj: c_int,
    pub thresh: *mut f32,
    pub npix: *mut i64,
    pub tnpix: *mut i64,
    pub xmin: *mut i64,
    pub xmax: *mut i64,
    pub ymin: *mut i64,
    pub ymax: *mut i64,
    pub x: *mut f64,
    pub y: *mut f64,
    pub x2: *mut f64,
    pub y2: *mut f64,
    pub xy: *mut f64,
    pub errx2: *mut f64,
    pub erry2: *mut f64,
    pub errxy: *mut f64,
    pub a: *mut f32,
    pub b: *mut f32,
    pub theta: *mut f32,
    pub cxx: *mut f32,
    pub cyy: *mut f32,
    pub cxy: *mut f32,
    pub cflux: *mut f32,
    pub flux: *mut f32,
    pub cpeak: *mut f32,
    pub peak: *mut f32,
    pub xcpeak: *mut i64,
    pub ycpeak: *mut i64,
    pub xpeak: *mut i64,
    pub ypeak: *mut i64,
    pub flag: *mut c_short,
    pub pix: *mut *mut i64,
    pub objectspix: *mut i64,
}

// ---------------------------------------------------------------------------------------------
// The C entry points — the whole subset this system needs
// ---------------------------------------------------------------------------------------------

extern "C" {
    fn sep_background(
        image: *const SepImage,
        bw: i64,
        bh: i64,
        fw: i64,
        fh: i64,
        fthresh: f64,
        bkg: *mut *mut SepBkg,
    ) -> c_int;

    fn sep_bkg_global(bkg: *const SepBkg) -> f32;
    fn sep_bkg_globalrms(bkg: *const SepBkg) -> f32;
    fn sep_bkg_subarray(bkg: *const SepBkg, arr: *mut c_void, dtype: c_int) -> c_int;
    fn sep_bkg_free(bkg: *mut SepBkg);

    #[allow(clippy::too_many_arguments)]
    fn sep_extract(
        image: *const SepImage,
        thresh: f32,
        thresh_type: c_int,
        minarea: c_int,
        conv: *const f32,
        convw: i64,
        convh: i64,
        filter_type: c_int,
        deblend_nthresh: c_int,
        deblend_cont: f64,
        clean_flag: c_int,
        clean_param: f64,
        catalog: *mut *mut SepCatalog,
    ) -> c_int;

    fn sep_catalog_free(catalog: *mut SepCatalog);

    fn sep_set_extract_pixstack(val: usize);
    fn sep_get_extract_pixstack() -> usize;

    fn sep_get_errmsg(status: c_int, errtext: *mut c_char);

    static sep_version_string: *const c_char;

    // The spike's own probe, not libsep's.
    fn astroctl_probe_sizeof_image() -> usize;
    fn astroctl_probe_sizeof_bkg() -> usize;
    fn astroctl_probe_sizeof_catalog() -> usize;
    fn astroctl_probe_offset_image(field: c_int) -> usize;
    fn astroctl_probe_offset_bkg(field: c_int) -> usize;
    fn astroctl_probe_offset_catalog(field: c_int) -> usize;
}

// ---------------------------------------------------------------------------------------------
// Safe-ish wrappers
// ---------------------------------------------------------------------------------------------

/// A libsep non-zero status, carrying the library's own message.
#[derive(Debug, Clone)]
pub struct SepError {
    pub status: c_int,
    pub message: String,
}

impl fmt::Display for SepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sep error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for SepError {}

fn check(status: c_int) -> Result<(), SepError> {
    if status == 0 {
        return Ok(());
    }
    // sep.h documents the buffer as 512 bytes ("errtext must be at least 60 characters" for
    // errmsg, 512 for errdetail). 512 is used for both.
    let mut buf = vec![0_i8; 512];
    // SAFETY: buffer is 512 bytes, which is what sep_get_errmsg is documented to need.
    unsafe { sep_get_errmsg(status, buf.as_mut_ptr()) };
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as u8)
        .collect();
    Err(SepError {
        status,
        message: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// The library's own version string.
pub fn version() -> String {
    // SAFETY: `sep_version_string` is a static NUL-terminated string in util.c.
    unsafe {
        let ptr = sep_version_string;
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), len);
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The extraction pixel stack, in pixels. See `Background::pixstack_for`.
pub fn pixstack() -> usize {
    // SAFETY: plain getter over a static.
    unsafe { sep_get_extract_pixstack() }
}

/// Sets the extraction pixel stack.
///
/// **This is process-global mutable state inside the C library**, which is a real constraint on
/// where a binding may be called from. Recorded in FINDINGS §7.
pub fn set_pixstack(val: usize) {
    // SAFETY: plain setter over a static.
    unsafe { sep_set_extract_pixstack(val) }
}

/// An owned `sep_bkg`, freed on drop.
pub struct Background {
    ptr: *mut SepBkg,
}

impl Background {
    /// `sep_background` with Source Extractor's defaults for tile and filter size.
    pub fn estimate(
        image: &SepImage,
        bw: i64,
        bh: i64,
        fw: i64,
        fh: i64,
        fthresh: f64,
    ) -> Result<Self, SepError> {
        let mut ptr: *mut SepBkg = std::ptr::null_mut();
        // SAFETY: `image` is a valid &SepImage whose `data` outlives this call; `ptr` is a
        // valid out-parameter.
        let status = unsafe { sep_background(image, bw, bh, fw, fh, fthresh, &mut ptr) };
        check(status)?;
        assert!(!ptr.is_null(), "sep_background returned 0 and a null bkg");
        Ok(Self { ptr })
    }

    /// Global background level (a mode estimate, not a plain mean).
    pub fn global(&self) -> f32 {
        // SAFETY: `self.ptr` is non-null for the lifetime of `self`.
        unsafe { sep_bkg_global(self.ptr) }
    }

    /// Global background RMS.
    pub fn global_rms(&self) -> f32 {
        // SAFETY: as above.
        unsafe { sep_bkg_globalrms(self.ptr) }
    }

    /// Subtracts the background from `data` **in place**.
    ///
    /// In place is the entire reason this is affordable on a 512 MB node: the alternative
    /// (`sep_bkg_array` into a second buffer) costs another full float32 image — 96 MB at
    /// 24 MP. See FINDINGS §3.
    pub fn subtract_in_place(&self, data: &mut [f32]) -> Result<(), SepError> {
        // SAFETY: `data` is the same size as the image the background was computed from —
        // asserted by the caller through `SepImage::mono_f32`.
        let status = unsafe {
            sep_bkg_subarray(self.ptr, data.as_mut_ptr().cast::<c_void>(), SEP_TFLOAT)
        };
        check(status)
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` came from sep_background and is freed exactly once.
            unsafe { sep_bkg_free(self.ptr) };
            self.ptr = std::ptr::null_mut();
        }
    }
}

/// One detected object, copied out of the C catalogue.
///
/// Copied rather than borrowed on purpose: the C arrays live until `sep_catalog_free`, and a
/// binding that handed out references into them would make the free unsafe to schedule.
#[derive(Debug, Clone, Copy)]
pub struct Object {
    pub x: f64,
    pub y: f64,
    pub flux: f32,
    pub peak: f32,
    pub a: f32,
    pub b: f32,
    pub theta: f32,
    pub npix: i64,
    pub flag: c_short,
}

/// An owned `sep_catalog`, freed on drop.
pub struct Catalog {
    ptr: *mut SepCatalog,
}

impl Catalog {
    /// `sep_extract` with Source Extractor's defaults, except as passed.
    ///
    /// `conv` is the detection filter. Passing `None` means *no filtering at all*, which is not
    /// the Source Extractor default — the default is a 3x3 Gaussian. The distinction matters for
    /// the Python cross-check, where `sep.extract`'s `filter_kernel` argument defaults to that
    /// same 3x3 kernel; comparing a filtered catalogue against an unfiltered one would look like
    /// an FFI bug and would not be one.
    #[allow(clippy::too_many_arguments)]
    pub fn extract(
        image: &SepImage,
        thresh: f32,
        thresh_type: c_int,
        minarea: c_int,
        conv: Option<(&[f32], i64, i64)>,
        deblend_nthresh: c_int,
        deblend_cont: f64,
        clean: bool,
        clean_param: f64,
    ) -> Result<Self, SepError> {
        let (conv_ptr, convw, convh) = match conv {
            Some((k, w, h)) => {
                assert_eq!(k.len() as i64, w * h, "convolution kernel size mismatch");
                (k.as_ptr(), w, h)
            }
            None => (std::ptr::null(), 0, 0),
        };
        let mut ptr: *mut SepCatalog = std::ptr::null_mut();
        // SAFETY: `image` and the kernel outlive the call; `ptr` is a valid out-parameter.
        let status = unsafe {
            sep_extract(
                image,
                thresh,
                thresh_type,
                minarea,
                conv_ptr,
                convw,
                convh,
                0, // SEP_FILTER_CONV
                deblend_nthresh,
                deblend_cont,
                c_int::from(clean),
                clean_param,
                &mut ptr,
            )
        };
        check(status)?;
        assert!(!ptr.is_null(), "sep_extract returned 0 and a null catalog");
        Ok(Self { ptr })
    }

    /// How many objects were found.
    pub fn len(&self) -> usize {
        // SAFETY: `ptr` is non-null for the lifetime of `self`.
        unsafe { (*self.ptr).nobj as usize }
    }

    /// Every object, copied out.
    pub fn objects(&self) -> Vec<Object> {
        let n = self.len();
        let mut out = Vec::with_capacity(n);
        // SAFETY: every array in the catalogue is `nobj` long by the struct's contract.
        unsafe {
            let c = &*self.ptr;
            for i in 0..n {
                out.push(Object {
                    x: *c.x.add(i),
                    y: *c.y.add(i),
                    flux: *c.flux.add(i),
                    peak: *c.peak.add(i),
                    a: *c.a.add(i),
                    b: *c.b.add(i),
                    theta: *c.theta.add(i),
                    npix: *c.npix.add(i),
                    flag: *c.flag.add(i),
                });
            }
        }
        out
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` came from sep_extract and is freed exactly once.
            unsafe { sep_catalog_free(self.ptr) };
            self.ptr = std::ptr::null_mut();
        }
    }
}

/// Source Extractor's default 3x3 detection kernel, and `sep.extract`'s default `filter_kernel`.
///
/// Upstream writes it unnormalised as `{1 2 1 2 4 2 1 2 1}`; the Python package's default is the
/// same kernel. Both are used *unnormalised* by `sep_extract`, which normalises internally.
pub const DEFAULT_CONV: [f32; 9] = [1.0, 2.0, 1.0, 2.0, 4.0, 2.0, 1.0, 2.0, 1.0];

// ---------------------------------------------------------------------------------------------
// Layout verification
// ---------------------------------------------------------------------------------------------

/// Asserts every Rust struct mirror matches what the C compiler says.
///
/// Panics with the offending field index. Run first thing in `main`, so no measurement can ever
/// be reported from a binding whose layout is wrong.
pub fn assert_layout() -> Vec<String> {
    let mut report = Vec::new();

    // SAFETY: every probe is a pure function over `sizeof`/`offsetof`.
    unsafe {
        let checks: [(&str, usize, usize); 3] = [
            (
                "sep_image",
                std::mem::size_of::<SepImage>(),
                astroctl_probe_sizeof_image(),
            ),
            (
                "sep_bkg",
                std::mem::size_of::<SepBkg>(),
                astroctl_probe_sizeof_bkg(),
            ),
            (
                "sep_catalog",
                std::mem::size_of::<SepCatalog>(),
                astroctl_probe_sizeof_catalog(),
            ),
        ];
        for (name, rust, c) in checks {
            assert_eq!(rust, c, "sizeof({name}): Rust {rust} != C {c}");
            report.push(format!("  sizeof({name}) = {c} bytes, Rust agrees"));
        }

        let image_fields = [
            ("data", offset_of_image_data()),
            ("noise", offset_of!(SepImage, noise)),
            ("mask", offset_of!(SepImage, mask)),
            ("segmap", offset_of!(SepImage, segmap)),
            ("dtype", offset_of!(SepImage, dtype)),
            ("ndtype", offset_of!(SepImage, ndtype)),
            ("mdtype", offset_of!(SepImage, mdtype)),
            ("sdtype", offset_of!(SepImage, sdtype)),
            ("segids", offset_of!(SepImage, segids)),
            ("idcounts", offset_of!(SepImage, idcounts)),
            ("numids", offset_of!(SepImage, numids)),
            ("w", offset_of!(SepImage, w)),
            ("h", offset_of!(SepImage, h)),
            ("noiseval", offset_of!(SepImage, noiseval)),
            ("noise_type", offset_of!(SepImage, noise_type)),
            ("gain", offset_of!(SepImage, gain)),
            ("maskthresh", offset_of!(SepImage, maskthresh)),
        ];
        for (i, (name, rust)) in image_fields.iter().enumerate() {
            let c = astroctl_probe_offset_image(i as c_int);
            assert_eq!(*rust, c, "offsetof(sep_image, {name}): Rust {rust} != C {c}");
        }
        report.push(format!(
            "  sep_image: {} field offsets match",
            image_fields.len()
        ));

        let bkg_fields = [
            ("w", offset_of!(SepBkg, w)),
            ("h", offset_of!(SepBkg, h)),
            ("bw", offset_of!(SepBkg, bw)),
            ("bh", offset_of!(SepBkg, bh)),
            ("nx", offset_of!(SepBkg, nx)),
            ("ny", offset_of!(SepBkg, ny)),
            ("n", offset_of!(SepBkg, n)),
            ("global", offset_of!(SepBkg, global)),
            ("globalrms", offset_of!(SepBkg, globalrms)),
            ("back", offset_of!(SepBkg, back)),
            ("dback", offset_of!(SepBkg, dback)),
            ("sigma", offset_of!(SepBkg, sigma)),
            ("dsigma", offset_of!(SepBkg, dsigma)),
        ];
        for (i, (name, rust)) in bkg_fields.iter().enumerate() {
            let c = astroctl_probe_offset_bkg(i as c_int);
            assert_eq!(*rust, c, "offsetof(sep_bkg, {name}): Rust {rust} != C {c}");
        }
        report.push(format!(
            "  sep_bkg: {} field offsets match",
            bkg_fields.len()
        ));

        let catalog_fields = [
            ("nobj", offset_of!(SepCatalog, nobj)),
            ("thresh", offset_of!(SepCatalog, thresh)),
            ("npix", offset_of!(SepCatalog, npix)),
            ("tnpix", offset_of!(SepCatalog, tnpix)),
            ("xmin", offset_of!(SepCatalog, xmin)),
            ("xmax", offset_of!(SepCatalog, xmax)),
            ("ymin", offset_of!(SepCatalog, ymin)),
            ("ymax", offset_of!(SepCatalog, ymax)),
            ("x", offset_of!(SepCatalog, x)),
            ("y", offset_of!(SepCatalog, y)),
            ("x2", offset_of!(SepCatalog, x2)),
            ("y2", offset_of!(SepCatalog, y2)),
            ("xy", offset_of!(SepCatalog, xy)),
            ("errx2", offset_of!(SepCatalog, errx2)),
            ("erry2", offset_of!(SepCatalog, erry2)),
            ("errxy", offset_of!(SepCatalog, errxy)),
            ("a", offset_of!(SepCatalog, a)),
            ("b", offset_of!(SepCatalog, b)),
            ("theta", offset_of!(SepCatalog, theta)),
            ("cxx", offset_of!(SepCatalog, cxx)),
            ("cyy", offset_of!(SepCatalog, cyy)),
            ("cxy", offset_of!(SepCatalog, cxy)),
            ("cflux", offset_of!(SepCatalog, cflux)),
            ("flux", offset_of!(SepCatalog, flux)),
            ("cpeak", offset_of!(SepCatalog, cpeak)),
            ("peak", offset_of!(SepCatalog, peak)),
            ("xcpeak", offset_of!(SepCatalog, xcpeak)),
            ("ycpeak", offset_of!(SepCatalog, ycpeak)),
            ("xpeak", offset_of!(SepCatalog, xpeak)),
            ("ypeak", offset_of!(SepCatalog, ypeak)),
            ("flag", offset_of!(SepCatalog, flag)),
            ("pix", offset_of!(SepCatalog, pix)),
            ("objectspix", offset_of!(SepCatalog, objectspix)),
        ];
        for (i, (name, rust)) in catalog_fields.iter().enumerate() {
            let c = astroctl_probe_offset_catalog(i as c_int);
            assert_eq!(
                *rust, c,
                "offsetof(sep_catalog, {name}): Rust {rust} != C {c}"
            );
        }
        report.push(format!(
            "  sep_catalog: {} field offsets match",
            catalog_fields.len()
        ));
    }

    report
}

/// `data` is the first field, so its offset is 0 — spelled out rather than macro'd because
/// `offset_of!` on the first field reads oddly.
fn offset_of_image_data() -> usize {
    0
}

/// `core::mem::offset_of!` is stable since 1.77 and this toolchain is 1.97.1, so this is just a
/// short alias.
macro_rules! offset_of {
    ($t:ty, $f:ident) => {
        core::mem::offset_of!($t, $f)
    };
}
use offset_of;
