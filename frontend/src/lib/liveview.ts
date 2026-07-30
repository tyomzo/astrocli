/*
 * The live-view socket's wire format and the image surface's state machine — SDD §5.7, §5.8.3.
 *
 * Everything here is pure. The component does the sockets, the object URLs and the rendering; this
 * file decides what the operator is looking at, which is the part that has to be right and the part
 * a test can hold still.
 *
 * # The rule this file exists for
 *
 * SDD §5.7: "The distinction the code needs is *stream idle because the camera is busy* (fine,
 * driven by `capture.progress`) versus *stream idle because the camera stopped responding* (a
 * wedge). Conflating them produces either spurious alerts during every capture or a missed wedge —
 * both worse than the pause itself."
 *
 * The camera cannot expose and stream live view at the same time — one sensor — so **every**
 * capture blanks the stream for the whole exposure. A panel that called that a stall would cry
 * wolf on every single frame of a normal night, and an operator who learns to ignore the warning
 * has lost the one that matters. [`surfacePhase`] therefore checks the capture explanation
 * *before* it checks the clock, and that ordering is the whole design.
 *
 * The converse is guarded too: a `capture.progress` that stopped advancing does not excuse the
 * gap forever. A camera wedged mid-exposure keeps its last `exposing` event on the wire, and a
 * panel that trusted it without bound would show "capturing" until morning.
 */

import type { CaptureProgress } from './link/protocol';

/** Magic every binary frame starts with — `ACLV`, for AstroCtl LiveView. */
const MAGIC = 0x41434c56;

/** Envelope version this client understands. */
const PROTOCOL_VERSION = 1;

/** magic(4) + version(1) + kind(1) + metaLen(2). */
const HEADER_BYTES = 8;

/**
 * How long a gap in live view may last before it is a stall rather than a slow link.
 *
 * The simulator streams at 5 fps and a real body is slower, so this is generous by design: the
 * cost of being late to call a stall is a few seconds of a stale image, and the cost of being
 * early is the false alarm §5.7 forbids.
 */
export const STALL_AFTER_MS = 4000;

/**
 * How stale a `capture.progress` may be and still explain a dark stream.
 *
 * The node ticks `exposing` once a second for the whole exposure (M1-T08), so a *fresh* tick is
 * positive evidence that the camera is alive and busy — which is precisely the claim being made.
 * Without this bound, one `exposing` event from a camera that then stopped answering would excuse
 * the silence for the rest of the night: the missed wedge of §5.7, arrived at from the other side.
 *
 * Freshness rather than "elapsed vs expected" is what makes this work for a **bulb** frame, whose
 * length nothing on the wire knows. 15 s is fifteen missed ticks — generous for a bad tunnel,
 * far short of a night.
 */
export const CAPTURE_EXPLAINS_GRACE_MS = 15_000;

/** A decoded binary frame. */
export interface LiveFrame {
  /** `live` is the camera's stream; `preview` is a captured frame's render. */
  kind: 'live' | 'preview';
  /** When the node says the image was made — RFC 3339, as every event carries. */
  ts: string;
  /** The frame this previews. Absent on a live-view frame. */
  frameId?: string;
  /**
   * The JPEG itself.
   *
   * The `ArrayBuffer` type argument is not decoration: without it the type widens to
   * `ArrayBufferLike`, which includes `SharedArrayBuffer`, and `new Blob([...])` refuses one. The
   * bytes here always come from a `WebSocket` message, which is never shared.
   */
  jpeg: Uint8Array<ArrayBuffer>;
}

/**
 * Decode one binary WebSocket payload, or `null` if it is not one of ours.
 *
 * Returning `null` rather than throwing: a frame this client cannot read is a reason to skip a
 * frame, never a reason to tear down the operator's image link. A node older than the envelope
 * would send a bare JPEG here, and the honest response is to ignore it and keep the socket.
 */
export function decodeLiveFrame(buffer: ArrayBuffer): LiveFrame | null {
  if (buffer.byteLength < HEADER_BYTES) return null;
  const view = new DataView(buffer);
  if (view.getUint32(0) !== MAGIC) return null;
  if (view.getUint8(4) !== PROTOCOL_VERSION) return null;

  const kind = view.getUint8(5) === 1 ? 'preview' : 'live';
  const metaLength = view.getUint16(6);
  if (buffer.byteLength < HEADER_BYTES + metaLength) return null;

  let meta: { ts?: unknown; frame_id?: unknown } = {};
  try {
    meta = JSON.parse(
      new TextDecoder().decode(new Uint8Array(buffer, HEADER_BYTES, metaLength)),
    ) as typeof meta;
  } catch {
    // A frame whose label did not parse is still an image. Showing it without a timestamp beats
    // dropping it, because the image is what the operator is looking at.
    meta = {};
  }

  return {
    kind,
    ts: typeof meta.ts === 'string' ? meta.ts : '',
    ...(typeof meta.frame_id === 'string' ? { frameId: meta.frame_id } : {}),
    jpeg: new Uint8Array(buffer, HEADER_BYTES + metaLength),
  };
}

/** What the image surface is showing, and why. */
export type SurfacePhase =
  /** The node is not reachable; nothing about the camera can be claimed. */
  | { kind: 'offline'; message: string }
  /** Live view has not been started. Not a fault — the default state of a node. */
  | { kind: 'idle'; message: string }
  /** Started, no frame yet. */
  | { kind: 'starting'; message: string }
  /** Frames are arriving. */
  | { kind: 'streaming' }
  /**
   * The stream is dark because the shutter is open, and that is normal (§5.7).
   *
   * `remainingS` is `null` when the exposure's length is not knowable — a bulb frame, or a
   * shutter token this build cannot parse — in which case the panel counts up instead.
   */
  | { kind: 'capturing'; message: string; elapsedS: number; remainingS: number | null }
  /** The stream stopped and nothing explains it. The one state that is a fault. */
  | { kind: 'stalled'; message: string };

/** Everything [`surfacePhase`] needs to decide. */
export interface SurfaceInput {
  /** Whether the control link is up; `false` makes every camera claim unknowable. */
  linkLive: boolean;
  /** Whether the operator has live view running. */
  streaming: boolean;
  /** `Date.now()` of the last live-view frame, or `null` if none has arrived. */
  lastFrameAt: number | null;
  /** The latest `capture.progress`, or `null`. */
  capture: CaptureProgress | null;
  /** `Date.now()` when that progress arrived. */
  captureAt: number | null;
  /** The configured exposure in seconds, if the panel knows it. */
  exposureSeconds: number | null;
  /** Now. */
  now: number;
}

/**
 * Decide what the surface says.
 *
 * The order of the branches is the specification, not an implementation detail — see the module
 * docs. In particular the capturing check precedes the stall check, because during an exposure
 * both are true and only one of them is the truth.
 */
export function surfacePhase(input: SurfaceInput): SurfacePhase {
  if (!input.linkLive) {
    return {
      kind: 'offline',
      message: "Not connected to the telescope — there's no picture to show from here.",
    };
  }

  const capturing = explainsTheGap(input);
  if (capturing !== null) return capturing;

  if (!input.streaming) {
    return { kind: 'idle', message: 'Live view is off.' };
  }

  if (input.lastFrameAt === null) {
    return { kind: 'starting', message: 'Starting live view…' };
  }

  if (input.now - input.lastFrameAt > STALL_AFTER_MS) {
    return {
      kind: 'stalled',
      message:
        'Live view has stopped and the camera has not said why. Try stopping and starting it; ' +
        'if that does not help, check the camera’s cable and power.',
    };
  }

  return { kind: 'streaming' };
}

/**
 * The capturing branch: is the gap explained?
 *
 * `exposing` and `downloading` are the two states in which the sensor is unavailable — M1-T08's
 * handoff names them as exactly this signal. `saved` and `preview_ready` are the *end* of an
 * exposure, so they explain nothing about a stream that is dark now.
 */
function explainsTheGap(input: SurfaceInput): SurfacePhase | null {
  const { capture, captureAt } = input;
  if (capture === null || captureAt === null) return null;
  if (capture.state !== 'exposing' && capture.state !== 'downloading') return null;

  // A tick that stopped arriving is a camera that stopped answering. This is the bound that makes
  // "the capture explains it" a claim with evidence behind it rather than a memory of one, and it
  // is the only bound that works for a bulb frame — see CAPTURE_EXPLAINS_GRACE_MS.
  if (input.now - captureAt > CAPTURE_EXPLAINS_GRACE_MS) return null;

  // How long the node said the exposure had been running, plus how long ago it said so. The
  // second term matters on a slow link, where the event itself may be seconds old.
  const elapsedS = capture.elapsed_s + (input.now - captureAt) / 1000;

  // A countdown is only meaningful while the shutter is open. Once the node is *reading the frame
  // off the camera* the exposure is over and its length says nothing about how long the download
  // has left — found by running the app, which showed "0s left" sitting under a download that
  // then took another eight seconds. Counting up is the honest answer for a wait whose length
  // nothing knows.
  const remainingS =
    input.exposureSeconds === null || capture.state === 'downloading'
      ? null
      : Math.max(0, input.exposureSeconds - elapsedS);

  // The second bound, and only when the exposure's length is known: a node that kept ticking
  // through an exposure running far past its configured length is describing something other
  // than the exposure the operator asked for.
  if (input.exposureSeconds !== null && elapsedS > input.exposureSeconds + CAPTURE_EXPLAINS_GRACE_MS / 1000) {
    return null;
  }

  return {
    kind: 'capturing',
    message:
      capture.state === 'exposing'
        ? 'Capturing — live view pauses while the shutter is open.'
        : 'Reading the frame off the camera — live view resumes in a moment.',
    elapsedS,
    remainingS,
  };
}

/**
 * Seconds for a shutter token, or `null` if it is not a fixed duration.
 *
 * The camera reports shutter speeds the way a camera does: `30`, `1/250`, `bulb`. Returning
 * `null` for `bulb` is the honest answer rather than a guess — a bulb frame's length is whatever
 * the operator asked for at the moment they asked, and inventing one would produce a countdown
 * that reaches zero while the shutter is still open.
 */
export function exposureSeconds(shutter: string | null | undefined): number | null {
  if (typeof shutter !== 'string') return null;
  const token = shutter.trim();
  if (token === '' || token.toLowerCase() === 'bulb') return null;

  const fraction = /^(\d+)\/(\d+)$/.exec(token);
  if (fraction !== null) {
    const numerator = Number(fraction[1]);
    const denominator = Number(fraction[2]);
    if (denominator === 0) return null;
    return numerator / denominator;
  }

  // `30`, `30.0`, and the `30"` some bodies report for whole seconds.
  const seconds = Number(token.replace(/["s]$/i, ''));
  return Number.isFinite(seconds) && seconds > 0 ? seconds : null;
}

/** `12s` / `1m 04s`, for a countdown an operator reads at a glance in the dark. */
export function formatDuration(seconds: number): string {
  const whole = Math.max(0, Math.round(seconds));
  if (whole < 60) return `${whole}s`;
  const minutes = Math.floor(whole / 60);
  return `${minutes}m ${String(whole % 60).padStart(2, '0')}s`;
}
