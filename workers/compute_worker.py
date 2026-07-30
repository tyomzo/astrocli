"""The supervised compute worker (SDD §5.12.4).

M1 scope is deliberately trivial: handshake, health, and one job kind — `preview`, which reads
a frame, applies an asinh stretch and writes a JPEG beside it. **No stacking math, no GPU, no
accumulator.** The point is that everything *around* the compute — spawn, handshake, ping,
cancel, restart, job round-trip, protocol versioning — is exercised end to end from the first
milestone with compute too simple to hide a bug in. When real stacking arrives in Phase 2b,
only the inside of `_preview` changes.

# Why this file has threads

The supervisor pings every `workers.health_ping_seconds` and kills a worker that misses three
in a row (SDD §5.12.3). A worker that read its stdin only between jobs would therefore be
SIGKILLed by its own health check partway through any job longer than three ping intervals —
which, for real stacking, is every job. So the main thread does nothing but read frames and
answer them, and compute runs on a separate thread. That also makes `cancel` deliverable while
a job is running, which is the only time it means anything.

# Running it by hand

Line framing exists so this works (SDD §5.12.1):

    printf '%s\\n' '{"type":"hello","proto_version":1}' \\
                   '{"type":"ping","nonce":1}' \\
                   '{"type":"shutdown"}' | python3 -u workers/compute_worker.py

Imports resolve because CPython puts a script's own directory on `sys.path`, and the supervisor
always launches this file by path.
"""

from __future__ import annotations

import importlib.metadata
import math
import os
import threading
import traceback

import astroctl_ipc as ipc

# Defaults for `preview` params. Chosen for a linear DSLR/CMOS light frame: the black point sits
# just under the sky background and the white point just under the brightest stars, so the asinh
# curve spends its dynamic range on nebulosity rather than on hot pixels.
DEFAULT_SOFTENING = 10.0
DEFAULT_BLACK_POINT_PCT = 0.5
DEFAULT_WHITE_POINT_PCT = 99.5
DEFAULT_MAX_DIMENSION = 2048
DEFAULT_QUALITY = 85

# FITS: 2880-byte blocks of 80-character cards (the primary-HDU subset this stub needs).
_FITS_BLOCK = 2880
_FITS_CARD = 80
_FITS_DTYPES = {8: ">u1", 16: ">i2", 32: ">i4", 64: ">i8", -32: ">f4", -64: ">f8"}


class WorkerFailure(Exception):
    """A job failure with a code from the closed SDD §4.2 vocabulary.

    Anything raised as one of these reaches the operator as a structured `result`, which is what
    separates "this frame is missing" from "the worker crashed".
    """

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


class _Cancelled(Exception):
    """The supervisor asked for this job to be abandoned."""


# ---------------------------------------------------------------------------------------------
# Frame reading — pure standard library plus numpy
# ---------------------------------------------------------------------------------------------


def _load_imaging():
    """Import the compute dependencies, late.

    Deliberately not imported at module level. The handshake, the health pings and the protocol
    version check must all work on a bare `python3` so that a stacking server whose venv is not
    populated fails with "install workers/requirements.txt" on the first job — an error naming
    its own fix — rather than with a worker that never starts and a supervisor reporting a
    failed handshake.
    """
    try:
        import numpy
        from PIL import Image
    except ImportError as error:
        raise WorkerFailure(
            "INTERNAL",
            f"the worker environment is incomplete ({error}). Install "
            f"workers/requirements.txt into the interpreter named by "
            f"`workers.python_interpreter`",
        ) from error
    return numpy, Image


def _read_fits(numpy, path):
    """Read the primary HDU of a FITS file into a float32 array.

    Hand-rolled rather than astropy: SDD §5.12.4 asks that `workers/requirements.txt` not
    acquire a large dependency tree before anything needs it, and the primary HDU of a 2-D image
    is a header of fixed-width cards followed by big-endian samples. Phase 2b, which has to read
    what real cameras and calibration libraries write, is where astropy earns its place.
    """
    header = {}
    with open(path, "rb") as handle:
        while True:
            block = handle.read(_FITS_BLOCK)
            if len(block) < _FITS_BLOCK:
                raise WorkerFailure("VALIDATION", f"{path} ends inside its FITS header")
            if _parse_header_block(block, header):
                break
            if len(header) > 1000:
                raise WorkerFailure("VALIDATION", f"{path} has no FITS END card")

        naxis = _header_int(header, path, "NAXIS")
        if naxis != 2:
            raise WorkerFailure(
                "UNSUPPORTED",
                f"{path} has NAXIS={naxis}; the M1 preview worker reads 2-D images only",
            )
        bitpix = _header_int(header, path, "BITPIX")
        if bitpix not in _FITS_DTYPES:
            raise WorkerFailure("UNSUPPORTED", f"{path} has an unknown BITPIX={bitpix}")
        width = _header_int(header, path, "NAXIS1")
        height = _header_int(header, path, "NAXIS2")
        if width <= 0 or height <= 0:
            raise WorkerFailure(
                "VALIDATION", f"{path} declares a {width}x{height} image"
            )

        wanted = width * height * abs(bitpix) // 8
        raw = handle.read(wanted)
        if len(raw) < wanted:
            raise WorkerFailure(
                "VALIDATION",
                f"{path} is truncated: {len(raw)} of {wanted} data bytes",
            )

    samples = numpy.frombuffer(raw, dtype=_FITS_DTYPES[bitpix]).reshape(height, width)
    # BZERO/BSCALE carry the unsigned-16-bit convention every DSLR-to-FITS converter uses
    # (BITPIX 16 with BZERO 32768). Ignoring them halves the sky and inverts the highlights.
    scale = _header_float(header, "BSCALE", 1.0)
    zero = _header_float(header, "BZERO", 0.0)
    # FITS rows run bottom-up; an image raster runs top-down. Without this flip every preview
    # this worker renders is mirrored in declination — which looks entirely plausible on a star
    # field and disagrees with the field node's preview of the same frame. Found by M1-T09
    # matching the two pipelines sample for sample.
    return numpy.flipud(samples.astype("float32") * scale + zero)


def _parse_header_block(block, header):
    """Fold one 2880-byte header block into `header`. Returns True at the END card."""
    text = block.decode("ascii", errors="replace")
    for start in range(0, _FITS_BLOCK, _FITS_CARD):
        card = text[start : start + _FITS_CARD]
        keyword = card[:8].strip()
        if keyword == "END":
            return True
        # Columns 9-10 hold the value indicator; a card without it is a comment or padding.
        if card[8:10] != "= ":
            continue
        header[keyword] = card[10:].split("/", 1)[0].strip()
    return False


def _header_int(header, path, keyword):
    if keyword not in header:
        raise WorkerFailure("VALIDATION", f"{path} has no FITS {keyword} card")
    try:
        return int(header[keyword])
    except ValueError as error:
        raise WorkerFailure(
            "VALIDATION", f"{path} has a non-numeric {keyword}: {header[keyword]!r}"
        ) from error


def _header_float(header, keyword, fallback):
    try:
        return float(header.get(keyword, fallback))
    except ValueError:
        return fallback


# ---------------------------------------------------------------------------------------------
# The preview itself
# ---------------------------------------------------------------------------------------------


def _asinh_stretch(numpy, pixels, softening, black_pct, white_pct):
    """Map a linear frame onto 8 bits with an asinh curve.

    asinh rather than a gamma or a log: it is linear near zero and logarithmic above it, so the
    sky background keeps its noise structure — which is what an operator is judging focus and
    tracking by — while the stars stop clipping to white.
    """
    finite = pixels[numpy.isfinite(pixels)]
    if finite.size == 0:
        raise WorkerFailure("VALIDATION", "the frame contains no finite samples")
    black = float(numpy.percentile(finite, black_pct))
    white = float(numpy.percentile(finite, white_pct))
    if white <= black:
        # A flat frame — a dark with the cap on, or a blown exposure. Not an error; it just has
        # nothing to stretch, and a zero-width window would divide by zero.
        white = black + 1.0
    normalized = numpy.clip((pixels - black) / (white - black), 0.0, 1.0)
    curve = numpy.arcsinh(normalized * softening) / math.asinh(softening)
    return (numpy.clip(curve, 0.0, 1.0) * 255.0 + 0.5).astype("uint8")


def _write_jpeg(image_module, pixels, target, max_dimension, quality):
    """Write the preview and return its (width, height)."""
    image = image_module.fromarray(pixels, mode="L")
    if max_dimension and max(image.size) > max_dimension:
        image.thumbnail((max_dimension, max_dimension))

    directory, name = os.path.split(target)
    scratch = os.path.join(directory, f".tmp_{name}")
    try:
        image.save(scratch, format="JPEG", quality=quality)
        # Renamed rather than written in place, for the reason ingest does the same thing (SDD
        # §5.11.2): a worker SIGKILLed mid-write must not leave a half-written JPEG at the path
        # it is about to report, where the API would serve it as a finished preview.
        os.replace(scratch, target)
    except BaseException:
        # Includes the SIGTERM/SIGINT path: a scratch file left in a session directory is
        # indistinguishable from a frame to anything that lists it later.
        if os.path.exists(scratch):
            os.unlink(scratch)
        raise
    return image.size


def _preview_path(source):
    root, _ = os.path.splitext(source)
    return root + ".jpg"


def _param(params, name, fallback, low, high):
    value = params.get(name, fallback)
    try:
        value = float(value)
    except (TypeError, ValueError) as error:
        raise WorkerFailure(
            "VALIDATION", f"preview.{name} is not a number: {value!r}"
        ) from error
    if not math.isfinite(value) or not low <= value <= high:
        raise WorkerFailure(
            "VALIDATION",
            f"preview.{name}={value} is out of range; expected {low}..{high}",
        )
    return value


# ---------------------------------------------------------------------------------------------
# The worker
# ---------------------------------------------------------------------------------------------


class ComputeWorker:
    """The protocol state machine. One job at a time; pings answered throughout."""

    def __init__(self, channel):
        self._channel = channel
        self._lock = threading.Lock()
        self._running = None
        self._cancelled = set()

    def run(self):
        for message in self._channel.frames():
            kind = message["type"]
            if kind == "ping":
                self._channel.send(ipc.pong(message["nonce"]))
            elif kind == "hello":
                self._on_hello(message)
            elif kind == "job":
                self._on_job(message)
            elif kind == "cancel":
                with self._lock:
                    self._cancelled.add(message["id"])
            elif kind == "shutdown":
                return 0
        # stdin closed: the backbone is gone, and nothing this worker computes has a reader.
        return 0

    def _on_hello(self, message):
        their_version = message["proto_version"]
        if their_version != ipc.PROTO_VERSION:
            # The supervisor is what refuses the mismatch (SDD §5.12.2); saying so here is for
            # whoever is driving this worker from a shell and wondering why it was killed.
            self._channel.send(
                ipc.log(
                    "error",
                    f"the backbone speaks protocol v{their_version}, this worker speaks "
                    f"v{ipc.PROTO_VERSION}",
                )
            )
        self._channel.send(ipc.hello(self._capabilities()))

    def _capabilities(self):
        """What this worker can do, read from installed distributions.

        `importlib.metadata` reports versions without importing anything, so a handshake stays
        milliseconds even where CuPy and its CUDA runtime are installed. `gpu` is always False
        in the stub: it has no GPU code path to report on, and claiming otherwise would make the
        backbone's log say something untrue about the machine.
        """
        libs = {}
        for distribution in ("numpy", "pillow", "cupy"):
            try:
                libs[distribution] = importlib.metadata.version(distribution)
            except importlib.metadata.PackageNotFoundError:
                continue
        return ipc.capabilities(gpu=False, libs=libs)

    def _on_job(self, message):
        job_id = message["id"]
        with self._lock:
            if self._running is not None:
                # The supervisor sends one job at a time, so this is a bug somewhere rather
                # than a queue. Refusing is honest; queueing would hide it.
                self._channel.send(
                    ipc.result_error(
                        job_id, "BUSY", f"job {self._running} is already running"
                    )
                )
                return
            self._running = job_id
        # Daemon: a `shutdown` must not wait for a half-finished stretch, and the supervisor
        # closes stdin behind it anyway.
        threading.Thread(target=self._execute, args=(message,), daemon=True).start()

    def _execute(self, message):
        job_id = message["id"]
        try:
            if message["kind"] != "preview":
                raise WorkerFailure(
                    "UNSUPPORTED",
                    f"this worker implements `preview` only, not {message['kind']!r}",
                )
            self._channel.send(ipc.result_ok(job_id, self._preview(message)))
        except _Cancelled:
            # No SDD §4.2 code means "cancelled"; the supervisor has already decided this job
            # timed out and discards whatever arrives, so the code here only reaches a log.
            self._channel.send(
                ipc.result_error(job_id, "INTERNAL", f"job {job_id} cancelled")
            )
        except WorkerFailure as failure:
            self._channel.send(ipc.result_error(job_id, failure.code, str(failure)))
        except Exception:  # noqa: BLE001 - see below; the breadth is the requirement
            # SDD §5.12 crash resilience: "worker exception → structured error result". The
            # blanket catch *is* the specification. Narrowing it to the exceptions imagined
            # today would let the one nobody imagined kill the process, and the supervisor would
            # restart it and hand the same frame back, which is the crash loop §5.12.3 forbids.
            # The traceback goes to a log frame so the operator's result stays one sentence.
            self._channel.send(ipc.log("error", traceback.format_exc()))
            summary = traceback.format_exc().strip().splitlines()[-1]
            self._channel.send(
                ipc.result_error(
                    job_id, "INTERNAL", f"the compute worker failed: {summary}"
                )
            )
        finally:
            with self._lock:
                self._running = None
                self._cancelled.discard(job_id)

    def _preview(self, message):
        job_id = message["id"]
        params = message["params"] if isinstance(message["params"], dict) else {}
        paths = message["paths"]
        if len(paths) != 1:
            raise WorkerFailure(
                "VALIDATION", f"preview takes exactly one frame, got {len(paths)}"
            )
        source = paths[0]
        if not os.path.isfile(source):
            raise WorkerFailure("NOT_FOUND", f"no such frame: {source}")

        softening = _param(params, "softening", DEFAULT_SOFTENING, 0.1, 10000.0)
        black_pct = _param(
            params, "black_point_pct", DEFAULT_BLACK_POINT_PCT, 0.0, 100.0
        )
        white_pct = _param(
            params, "white_point_pct", DEFAULT_WHITE_POINT_PCT, 0.0, 100.0
        )
        max_dimension = int(
            _param(params, "max_dimension", DEFAULT_MAX_DIMENSION, 0.0, 65535.0)
        )
        quality = int(_param(params, "quality", DEFAULT_QUALITY, 1.0, 95.0))
        if black_pct >= white_pct:
            raise WorkerFailure(
                "VALIDATION",
                f"preview.black_point_pct ({black_pct}) must be below white_point_pct "
                f"({white_pct})",
            )

        numpy, image_module = _load_imaging()

        self._checkpoint(job_id, 10)
        pixels = _read_fits(numpy, source)
        self._checkpoint(job_id, 45)
        eight_bit = _asinh_stretch(numpy, pixels, softening, black_pct, white_pct)
        self._checkpoint(job_id, 75)
        target = _preview_path(source)
        width, height = _write_jpeg(
            image_module, eight_bit, target, max_dimension, quality
        )
        self._checkpoint(job_id, 95)

        return {
            "preview_path": target,
            "source_path": source,
            "width": width,
            "height": height,
        }

    def _checkpoint(self, job_id, pct):
        """Report progress, and abandon the job here if `cancel` arrived."""
        with self._lock:
            cancelled = job_id in self._cancelled
        if cancelled:
            raise _Cancelled
        self._channel.send(ipc.progress(job_id, pct))


def main():
    channel = ipc.Channel.open()
    try:
        return ComputeWorker(channel).run()
    finally:
        channel.close()


if __name__ == "__main__":
    raise SystemExit(main())
