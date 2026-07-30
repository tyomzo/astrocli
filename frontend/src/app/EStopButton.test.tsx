import { renderToStaticMarkup } from 'react-dom/server';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { dispatch, useTelemetryStore, EMPTY } from '../store/telemetry';
import { EStopButton } from './EStopButton';
import buttonSource from './EStopButton.tsx?raw';

/*
 * The e-stop is the one control where a comfortable UI decision is a safety defect — MNT-08,
 * REL-01, SDD §5.9.
 *
 * Two properties are asserted here and neither is cosmetic:
 *
 *  1. **The button never draws the mount as stopped because it was pressed.** SDD §5.9's
 *     "commands optimistically do nothing" is a general rule; here it is the difference between
 *     an operator walking away from a telescope that has halted and one that has not. The tap
 *     produces "sent"; only an `EMERGENCY_STOP` event from the node produces "stopped".
 *  2. **A stop that did not arrive says so, and says what to do instead.** The app has nothing
 *     left to offer at that point, and "cut power" is more use than a spinner.
 *
 * Rendered through `react-dom/server`, like `NudgeBadge.test.tsx`: this is about what the
 * component puts on screen for a given state, which needs no DOM and no click simulation. The tap
 * itself is driven by calling the command layer the button uses, which is the seam the test can
 * reach without one.
 */

function stopAlert(): void {
  dispatch({
    type: 'link/event',
    at: Date.now(),
    event: {
      topic: 'alert',
      ts: new Date().toISOString(),
      data: {
        severity: 'critical',
        code: 'EMERGENCY_STOP',
        message: 'emergency stop: all motion halted and tracking is off',
      },
    },
  });
}

beforeEach(() => {
  useTelemetryStore.setState(EMPTY);
});

afterEach(() => {
  useTelemetryStore.setState(EMPTY);
});

describe('the e-stop button', () => {
  it('is armed, with no "not connected in this build" explanation left', () => {
    const markup = renderToStaticMarkup(<EStopButton />);
    expect(markup).toContain('STOP');
    expect(markup).not.toContain('aria-disabled');
    // The M1-T04 placeholder said the route did not exist. It does now, and a button that still
    // apologised for itself would be the more dangerous of the two lies.
    expect(markup).not.toContain('not connected in this build');
    expect(markup).not.toContain('/api/mount/estop');
  });

  it('goes through the command layer rather than calling fetch itself', () => {
    // The `keepalive` flag that lets a stop survive the document going away lives in
    // `commands.ts` and is asserted there. What matters here is that this button uses that path:
    // a `fetch` written inline would silently lose the flag, and the loss would only show up on
    // a phone that locked mid-stop.
    expect(buttonSource).toContain("import { mountEstop } from '../lib/commands'");
    expect(buttonSource).not.toMatch(/\bfetch\s*\(/);
  });

  it('renders "stopped" only from the event stream, never from its own request', () => {
    // The store carries an EMERGENCY_STOP alert, which is what the node publishes when the stop
    // lands. With no tap having happened, the button still says nothing — the confirmation is
    // about *this* request, so a stale alert from earlier in the night must not stand in for one.
    stopAlert();
    const markup = renderToStaticMarkup(<EStopButton />);
    expect(markup).not.toContain('has stopped');
    expect(markup).not.toContain('Stop sent');
  });

  it('speaks about the telescope rather than about the request', () => {
    // The vocabulary guard of `operatorLanguage.test.ts` covers this file too; this asserts the
    // positive half — the words that should be there.
    stopAlert();
    const markup = renderToStaticMarkup(<EStopButton />);
    expect(markup.toLowerCase()).not.toContain('http');
    expect(markup.toLowerCase()).not.toContain('202');
  });
});
