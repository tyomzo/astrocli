import { describe, expect, it } from 'vitest';

import {
  CAPTURE_EXPLAINS_GRACE_MS,
  decodeLiveFrame,
  exposureSeconds,
  formatDuration,
  STALL_AFTER_MS,
  surfacePhase,
} from './liveview';
import type { SurfaceInput } from './liveview';
import type { CaptureProgress } from './link/protocol';

const NOW = 1_800_000_000_000;

function input(overrides: Partial<SurfaceInput> = {}): SurfaceInput {
  return {
    linkLive: true,
    streaming: true,
    lastFrameAt: NOW - 100,
    capture: null,
    captureAt: null,
    exposureSeconds: 30,
    now: NOW,
    ...overrides,
  };
}

function progress(state: CaptureProgress['state'], elapsed_s = 0): CaptureProgress {
  return { frame_id: 'light_00042', state, elapsed_s };
}

/** Build the envelope the node writes, so the decoder is tested against the real layout. */
function envelope(kind: 0 | 1, meta: object, jpeg: number[]): ArrayBuffer {
  const metaBytes = new TextEncoder().encode(JSON.stringify(meta));
  const buffer = new ArrayBuffer(8 + metaBytes.length + jpeg.length);
  const view = new DataView(buffer);
  view.setUint32(0, 0x41434c56); // "ACLV"
  view.setUint8(4, 1);
  view.setUint8(5, kind);
  view.setUint16(6, metaBytes.length);
  new Uint8Array(buffer, 8, metaBytes.length).set(metaBytes);
  new Uint8Array(buffer, 8 + metaBytes.length).set(jpeg);
  return buffer;
}

describe('surfacePhase', () => {
  it('calls a gap during an exposure a capture, not a stall', () => {
    // SDD §5.7's whole point. The stream has been dark for far longer than the stall threshold —
    // a 30 s exposure blanks it for 30 s — and it must NOT be a fault, because the camera has one
    // sensor and told us it is using it.
    const phase = surfacePhase(
      input({
        lastFrameAt: NOW - 20_000,
        capture: progress('exposing', 20),
        captureAt: NOW,
      }),
    );
    expect(phase.kind).toBe('capturing');
    expect(NOW - 20_000).toBeLessThan(NOW - STALL_AFTER_MS);
  });

  it('counts down the remaining exposure when it knows the length', () => {
    const phase = surfacePhase(
      input({ capture: progress('exposing', 8), captureAt: NOW, exposureSeconds: 30 }),
    );
    if (phase.kind !== 'capturing') throw new Error(`expected capturing, got ${phase.kind}`);
    expect(phase.remainingS).toBeCloseTo(22, 5);
    expect(phase.elapsedS).toBeCloseTo(8, 5);
  });

  it('counts up instead of inventing a countdown for a bulb frame', () => {
    // A countdown that reaches zero while the shutter is still open is worse than no countdown:
    // the operator concludes the node has lost track of the exposure.
    const phase = surfacePhase(
      input({ capture: progress('exposing', 45), captureAt: NOW, exposureSeconds: null }),
    );
    if (phase.kind !== 'capturing') throw new Error(`expected capturing, got ${phase.kind}`);
    expect(phase.remainingS).toBeNull();
    expect(phase.elapsedS).toBeCloseTo(45, 5);
  });

  it('adds the age of the progress event to the elapsed time it reported', () => {
    // On a slow link the event itself is seconds old. A panel that showed `elapsed_s` verbatim
    // would run visibly behind the shutter it is describing.
    const phase = surfacePhase(
      input({ capture: progress('exposing', 5), captureAt: NOW - 3000, exposureSeconds: 30 }),
    );
    if (phase.kind !== 'capturing') throw new Error(`expected capturing, got ${phase.kind}`);
    expect(phase.elapsedS).toBeCloseTo(8, 5);
  });

  it('counts up rather than down once the shutter has closed', () => {
    // Found by running the app: the surface showed "0s left" under a download that then took
    // another eight seconds. The exposure's length is a countdown for the *exposure* and says
    // nothing about a download, so past the shutter there is nothing honest to count down to.
    const phase = surfacePhase(
      input({ capture: progress('downloading', 5), captureAt: NOW, exposureSeconds: 5 }),
    );
    if (phase.kind !== 'capturing') throw new Error(`expected capturing, got ${phase.kind}`);
    expect(phase.remainingS).toBeNull();
    expect(phase.elapsedS).toBeCloseTo(5, 5);
  });

  it('treats downloading as an explained gap too', () => {
    // M1-T08's handoff names both: `exposing` and `downloading` are the states in which the
    // sensor is unavailable. Only reading `exposing` would produce a false stall in the seconds
    // it takes a 32 MB frame to come off the body.
    const phase = surfacePhase(
      input({ lastFrameAt: NOW - 10_000, capture: progress('downloading', 30), captureAt: NOW }),
    );
    expect(phase.kind).toBe('capturing');
  });

  it('stops excusing the gap when the progress ticks stop arriving', () => {
    // The bound that works for a bulb frame, whose length nothing on the wire knows. The node
    // ticks `exposing` once a second; a tick that is fifteen seconds old is a camera that stopped
    // answering, not a long exposure.
    const phase = surfacePhase(
      input({
        lastFrameAt: NOW - 60_000,
        capture: progress('exposing', 90),
        captureAt: NOW - CAPTURE_EXPLAINS_GRACE_MS - 1000,
        exposureSeconds: null,
      }),
    );
    expect(phase.kind).toBe('stalled');
  });

  it('keeps excusing a long bulb frame while its ticks keep arriving', () => {
    // The converse, and the reason freshness is the bound rather than elapsed time: a 300 s bulb
    // frame is a completely normal thing to be doing.
    const phase = surfacePhase(
      input({
        lastFrameAt: NOW - 200_000,
        capture: progress('exposing', 240),
        captureAt: NOW - 500,
        exposureSeconds: null,
      }),
    );
    expect(phase.kind).toBe('capturing');
  });

  it('stops excusing the gap once a known exposure is long overdue', () => {
    // The missed wedge, from the other side: a camera that stopped answering mid-exposure leaves
    // its last `exposing` event on the wire, and a panel that trusted it without bound would say
    // "capturing" until morning.
    const overdue = 30 + CAPTURE_EXPLAINS_GRACE_MS / 1000 + 5;
    const phase = surfacePhase(
      input({
        lastFrameAt: NOW - 60_000,
        capture: progress('exposing', overdue),
        captureAt: NOW,
        exposureSeconds: 30,
      }),
    );
    expect(phase.kind).toBe('stalled');
  });

  it('calls an unexplained gap a stall', () => {
    // The other half of §5.7. Nothing says the camera is busy, and the frames stopped.
    const phase = surfacePhase(input({ lastFrameAt: NOW - STALL_AFTER_MS - 1, capture: null }));
    expect(phase.kind).toBe('stalled');
  });

  it('does not call a finished capture an explanation', () => {
    // `saved` and `preview_ready` are the END of an exposure. If they excused a dark stream, a
    // camera that wedged right after saving a frame would never be reported.
    for (const state of ['saved', 'preview_ready'] as const) {
      const phase = surfacePhase(
        input({ lastFrameAt: NOW - STALL_AFTER_MS - 1, capture: progress(state, 30), captureAt: NOW }),
      );
      expect(phase.kind, state).toBe('stalled');
    }
  });

  it('is streaming while frames keep arriving', () => {
    expect(surfacePhase(input({ lastFrameAt: NOW - 200 })).kind).toBe('streaming');
  });

  it('is starting between the request and the first frame', () => {
    expect(surfacePhase(input({ lastFrameAt: null })).kind).toBe('starting');
  });

  it('is idle when live view was never started', () => {
    expect(surfacePhase(input({ streaming: false, lastFrameAt: null })).kind).toBe('idle');
  });

  it('claims nothing about the camera when the link is down', () => {
    // A panel that said "live view has stalled" while the *node* is unreachable would send the
    // operator to the camera when the problem is the tunnel.
    const phase = surfacePhase(input({ linkLive: false, lastFrameAt: NOW - 60_000 }));
    expect(phase.kind).toBe('offline');
  });

  it('explains a capture even before live view was started', () => {
    // The surface shows previews whether or not the stream is running, so the capturing state has
    // to be reachable without it — otherwise a capture on a node with live view off would show
    // "Live view is off" while an exposure was visibly in progress in the capture strip.
    const phase = surfacePhase(
      input({ streaming: false, lastFrameAt: null, capture: progress('exposing', 3), captureAt: NOW }),
    );
    expect(phase.kind).toBe('capturing');
  });
});

describe('the operator reads sentences, not state names', () => {
  it('never puts a wire state name on screen', () => {
    // The copy test M1-T04 established. `exposing`, `preview_ready` and the rest are protocol
    // vocabulary; an operator in a field at 2 a.m. reads English.
    const phases = [
      surfacePhase(input({ linkLive: false })),
      surfacePhase(input({ streaming: false, lastFrameAt: null })),
      surfacePhase(input({ lastFrameAt: null })),
      surfacePhase(input({ capture: progress('exposing', 1), captureAt: NOW })),
      surfacePhase(input({ capture: progress('downloading', 1), captureAt: NOW })),
      surfacePhase(input({ lastFrameAt: NOW - 60_000 })),
    ];
    for (const phase of phases) {
      if (!('message' in phase)) continue;
      for (const jargon of ['exposing', 'downloading', 'preview_ready', 'liveview', 'ws', 'null']) {
        expect(phase.message.toLowerCase()).not.toContain(jargon);
      }
      expect(phase.message.length).toBeGreaterThan(10);
    }
  });

  it('tells the operator what to do about a stall', () => {
    const phase = surfacePhase(input({ lastFrameAt: NOW - 60_000 }));
    if (phase.kind !== 'stalled') throw new Error('expected stalled');
    expect(phase.message).toMatch(/cable|power|stopping and starting/i);
  });
});

describe('exposureSeconds', () => {
  it('reads the shutter tokens a camera reports', () => {
    expect(exposureSeconds('30')).toBe(30);
    expect(exposureSeconds('1/250')).toBeCloseTo(0.004, 6);
    expect(exposureSeconds('2.5')).toBe(2.5);
    expect(exposureSeconds('30"')).toBe(30);
  });

  it('returns null rather than guessing at a bulb frame', () => {
    expect(exposureSeconds('bulb')).toBeNull();
    expect(exposureSeconds('Bulb')).toBeNull();
    expect(exposureSeconds(null)).toBeNull();
    expect(exposureSeconds(undefined)).toBeNull();
    expect(exposureSeconds('')).toBeNull();
    expect(exposureSeconds('1/0')).toBeNull();
    expect(exposureSeconds('nonsense')).toBeNull();
  });
});

describe('formatDuration', () => {
  it('reads at a glance in the dark', () => {
    expect(formatDuration(0)).toBe('0s');
    expect(formatDuration(12.4)).toBe('12s');
    expect(formatDuration(64)).toBe('1m 04s');
    expect(formatDuration(-5)).toBe('0s');
  });
});

describe('decodeLiveFrame', () => {
  it('reads a preview frame and its frame id', () => {
    const frame = decodeLiveFrame(
      envelope(1, { ts: '2026-07-30T21:04:05.123Z', frame_id: 'light_00042' }, [0xff, 0xd8, 0xd9]),
    );
    expect(frame).not.toBeNull();
    expect(frame?.kind).toBe('preview');
    expect(frame?.frameId).toBe('light_00042');
    expect(frame?.ts).toBe('2026-07-30T21:04:05.123Z');
    expect(Array.from(frame?.jpeg ?? [])).toEqual([0xff, 0xd8, 0xd9]);
  });

  it('reads a live frame, which names no frame', () => {
    const frame = decodeLiveFrame(envelope(0, { ts: '2026-07-30T21:04:05.123Z' }, [0xff, 0xd8]));
    expect(frame?.kind).toBe('live');
    expect(frame?.frameId).toBeUndefined();
  });

  it('skips a frame it cannot read rather than failing', () => {
    // A node older than the envelope would send a bare JPEG. Ignoring it keeps the socket; a
    // throw here would tear down the operator's image link over one frame.
    expect(decodeLiveFrame(new Uint8Array([0xff, 0xd8, 0xff, 0xd9]).buffer)).toBeNull();
    expect(decodeLiveFrame(new ArrayBuffer(0))).toBeNull();

    const wrongVersion = envelope(1, { ts: 'x' }, [0xff]);
    new DataView(wrongVersion).setUint8(4, 99);
    expect(decodeLiveFrame(wrongVersion)).toBeNull();
  });

  it('keeps the image when the label does not parse', () => {
    // The picture is what the operator is looking at; a broken timestamp is not a reason to
    // withhold it.
    const metaBytes = new TextEncoder().encode('{not json');
    const buffer = new ArrayBuffer(8 + metaBytes.length + 2);
    const view = new DataView(buffer);
    view.setUint32(0, 0x41434c56);
    view.setUint8(4, 1);
    view.setUint8(5, 0);
    view.setUint16(6, metaBytes.length);
    new Uint8Array(buffer, 8, metaBytes.length).set(metaBytes);
    new Uint8Array(buffer, 8 + metaBytes.length).set([0xff, 0xd8]);

    const frame = decodeLiveFrame(buffer);
    expect(frame?.ts).toBe('');
    expect(Array.from(frame?.jpeg ?? [])).toEqual([0xff, 0xd8]);
  });
});
