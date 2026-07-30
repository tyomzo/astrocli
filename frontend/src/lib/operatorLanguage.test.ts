import { describe, expect, it } from 'vitest';

// `?raw` is Vite's own text import, typed by `vite/client` (already in tsconfig's `types`). Used
// rather than `node:fs` so this test needs no `@types/node` and no path arithmetic — the module
// graph resolves the files, which also means a rename cannot leave the guard silently pointing at
// nothing.
import estopButton from '../app/EStopButton.tsx?raw';
import cameraVitals from '../panels/CameraVitals.tsx?raw';
import captureStrip from '../panels/CaptureStrip.tsx?raw';
import clockSkewNote from '../panels/ClockSkewNote.tsx?raw';
import failureNote from '../ui/FailureNote.tsx?raw';
import linkBanner from '../panels/LinkBanner.tsx?raw';
import manualTargetChooser from '../panels/ManualTargetChooser.tsx?raw';
import sessionFrames from '../panels/SessionFrames.tsx?raw';
import trackingControl from '../panels/TrackingControl.tsx?raw';
import { nudgeAvailability } from './nudge';
import nudgeSource from './nudge.ts?raw';

/*
 * The words the operator reads are part of the product — M1-T03 decision 3.
 *
 * An operator read "The event link is down" and asked what an event link was. It is a fair
 * question: the phrase names a WebSocket carrying `Event` frames, which is true, is on no screen
 * they can see, and tells them nothing they can do. What they needed to know was that the app was
 * no longer in touch with the telescope, so the readings were stale and a command might not
 * arrive.
 *
 * Rewording is cheap; keeping it reworded is not, because the next person to add a failure branch
 * will reach for the vocabulary they are holding in their head, which is the implementation's.
 * These tests are the thing that objects.
 *
 * # Why a source scan and not a render
 *
 * The strings live in three shapes — JSX text, template literals in `describe()` helpers, and
 * plain returns — and only the last is callable without a store and a DOM. Reading the source is
 * coarse: it cannot tell a rendered string from a comment, so the check is scoped to the *text
 * between JSX tags and inside quoted strings*, and each file's doc comment is stripped first.
 * That is the right trade for a guard whose alternative is nothing at all.
 */

/** Words that describe the implementation rather than the telescope. */
const IMPLEMENTATION_VOCABULARY = [
  'event link',
  'websocket',
  'socket',
  'ws ticket',
  'bearer header',
  'state snapshot',
  'endpoint',
  'payload',
  'envelope',
  'reducer',
  'topic',
  'correlation',
];

const OPERATOR_FACING: readonly [string, string][] = [
  // The e-stop's copy is the most consequential in the app: it is read at the moment something is
  // going wrong, by someone who is not going to parse a sentence about topics and payloads.
  ['app/EStopButton.tsx', estopButton],
  ['panels/LinkBanner.tsx', linkBanner],
  // The clock warning is about a fact of the operator's *device*. Every honest way to describe
  // its cause names something they cannot see — a pong, an offset, a command envelope — so this
  // is the copy most likely to drift back into the implementation's vocabulary.
  ['panels/ClockSkewNote.tsx', clockSkewNote],
  ['panels/ManualTargetChooser.tsx', manualTargetChooser],
  ['panels/TrackingControl.tsx', trackingControl],
  // The capture controls are read in the dark by someone deciding whether to press a button that
  // opens a shutter for ten minutes. "The capture orchestrator is in the Faulted phase" is a
  // sentence about this program; what they need is what the camera did and what to do next.
  ['panels/CaptureStrip.tsx', captureStrip],
  ['panels/SessionFrames.tsx', sessionFrames],
  ['panels/CameraVitals.tsx', cameraVitals],
  ['ui/FailureNote.tsx', failureNote],
  ['lib/nudge.ts', nudgeSource],
];

/**
 * The parts of a file a reader sees on screen: quoted strings and JSX text, with comments and
 * import lines removed.
 *
 * Deliberately approximate. It over-reports (a `className` is a quoted string) rather than
 * under-reports, and the vocabulary list above is chosen so that over-reporting costs nothing —
 * no Tailwind utility contains the word "websocket".
 */
function visibleText(source: string): string {
  return source
    .replace(/\/\*[\s\S]*?\*\//g, ' ')
    .replace(/^\s*\/\/.*$/gm, ' ')
    .replace(/^\s*import .*$/gm, ' ')
    .toLowerCase();
}

describe('operator-facing copy', () => {
  for (const [name, source] of OPERATOR_FACING) {
    it(`${name} names the telescope, not the transport`, () => {
      const text = visibleText(source);
      for (const term of IMPLEMENTATION_VOCABULARY) {
        expect(text, `"${term}" is implementation vocabulary in ${name}`).not.toContain(term);
      }
    });
  }
});

describe('the nudge explanations', () => {
  it('say what is true of the telescope and what the app is doing about it', () => {
    // These are the only operator-facing strings in this family that are a pure function, so
    // they can be asserted directly rather than scanned for.
    const down = nudgeAvailability({ phase: 'idle' }, { state: 'unknown' });
    expect(down.available).toBe(false);
    if (down.available) return;

    expect(down.reason).toContain('Not connected to the mount');
    expect(down.reason).toContain('Reconnecting');
    expect(down.reason.toLowerCase()).not.toContain('link');
    expect(down.reason.toLowerCase()).not.toContain('node');
  });
});
