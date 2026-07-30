import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

import type { CaptureProgress } from '../lib/link/protocol';
import { CaptureStripView } from './CaptureStrip';
import captureStripSource from './CaptureStrip.tsx?raw';

/*
 * The capture strip renders the exposure from events and from nothing else — SDD §5.9.
 *
 * This is the rule that is *comfortable* to break: the app knows it just posted a capture, so
 * flipping the button to "exposing" on the reply feels responsive and takes one line. Over a
 * tunnel where a `202` can be lost, or where the node refused after answering, it means the strip
 * says the shutter is open when it is closed — and the operator is standing in the dark deciding
 * whether to wait or press again.
 *
 * Rendered through `react-dom/server`, like `NudgeBadge.test.tsx`: this is about the markup a
 * given store state produces, not about clicking. Effects do not run, which suits the subject —
 * every assertion below is about what the *store* put on screen.
 */

function render(props: {
  live?: boolean;
  connected?: boolean;
  capture?: CaptureProgress | null;
}): string {
  return renderToStaticMarkup(
    <CaptureStripView
      live={props.live ?? true}
      connected={props.connected ?? false}
      capture={props.capture ?? null}
    />,
  );
}

describe('what the strip shows before an exposure', () => {
  it('says the telescope is out of reach rather than offering a dead capture button', () => {
    const html = render({ live: false });
    // `>Capture<` rather than `Capture`: the section's own accessible name is "Capture", and it
    // is there in every state.
    expect(html).not.toContain('>Capture<');
    expect(html).toContain('can’t be reached');
  });

  it('offers to connect the camera when the node says it is not connected', () => {
    const html = render({ connected: false });
    expect(html).toContain('Connect camera');
    expect(html).not.toContain('Stop exposure');
  });

  it('offers capture once the camera reports itself connected', () => {
    const html = render({ connected: true });
    expect(html).toContain('Capture');
    expect(html).toContain('ISO');
    expect(html).toContain('Shutter');
    expect(html).toContain('Format');
  });
});

describe('what the strip shows during an exposure', () => {
  it('renders the running exposure from capture.progress, naming the frame', () => {
    const html = render({
      connected: true,
      capture: { frame_id: 'light_00042', state: 'exposing', elapsed_s: 17.4 },
    });

    expect(html).toContain('light_00042');
    // The state in the operator's words, not the wire token.
    expect(html).toContain('shutter open');
    expect(html).not.toContain('>exposing<');
    // CAM-04's countdown: whole seconds, because a jittering decimal is unreadable at arm's
    // length and the number is only meaningful against the exposure the 202 reported.
    expect(html).toContain('17s');
  });

  it('replaces capture with a stop control while the shutter is open', () => {
    // Not a disabled capture button: an operator who wants to end a ten-minute bulb needs the
    // control to be *there*, and a greyed-out Capture tells them the app has stopped listening.
    const html = render({
      connected: true,
      capture: { frame_id: 'light_00042', state: 'exposing', elapsed_s: 1 },
    });
    expect(html).toContain('Stop exposure');
    expect(html).not.toContain('>Capture<');
  });

  it('keeps the stop control through the download, which is still an exposure in flight', () => {
    const html = render({
      connected: true,
      capture: { frame_id: 'light_00042', state: 'downloading', elapsed_s: 30 },
    });
    expect(html).toContain('reading off the camera');
    expect(html).toContain('Stop exposure');
  });

  it('returns to capture once the frame is stored, and says which frame', () => {
    const html = render({
      connected: true,
      capture: { frame_id: 'light_00042', state: 'saved', elapsed_s: 30 },
    });
    expect(html).toContain('light_00042');
    expect(html).toContain('stored');
    expect(html).toContain('Capture');
    expect(html).not.toContain('Stop exposure');
  });

  it('says nothing has been captured rather than showing an empty readout', () => {
    const html = render({ connected: true });
    expect(html).toContain('No exposure taken');
  });
});

describe('the events-only rule', () => {
  it('reads the capture state from the store and never from a command reply', () => {
    // The structural half, in the same shape as `commands.test.ts`'s import check. A future edit
    // that set an `exposing` flag from the `cameraCapture` result would have to add a state
    // setter fed by that promise — which this makes a visible act rather than a quiet one.
    const captureCall = captureStripSource.slice(captureStripSource.indexOf('cameraCapture(token'));
    const statement = captureCall.slice(0, captureCall.indexOf('\n\n'));
    expect(statement).not.toMatch(/setExposing|setCapture\(/);

    // And the state that drives the button comes from the selector, not from local state.
    expect(captureStripSource).toContain('useTelemetryStore(selectCaptureProgress)');
  });
});
