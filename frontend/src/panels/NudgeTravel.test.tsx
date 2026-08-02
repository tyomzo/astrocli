import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

import type { MountPosition } from '../lib/link/protocol';
import { TravelReadout } from './NudgeOverlay';
import type { Slot } from '../store/telemetry';

/*
 * The travel readout — M3-T07's third requirement.
 *
 * On 2026-08-02 an operator wound the right-ascension axis 215.6° from home holding this
 * overlay's buttons, with nothing on screen changing to say so. The counter was the only thing
 * that knew and it was not published. These tests are about the number being *there* and being
 * honest when it is not known; whether it is legible at a telescope is not something
 * `renderToStaticMarkup` can answer.
 */

const T0 = Date.parse('2026-08-02T22:15:00.000Z');

function at(ra_travel: number | null, dec_travel: number | null): Slot<MountPosition> {
  return {
    state: 'observed',
    at: T0,
    ts: new Date(T0).toISOString(),
    value: { ra: 5.5, dec: -5.4, alt: 47.2, az: 128.4, pier_side: 'west', ra_travel, dec_travel },
  };
}

describe('TravelReadout', () => {
  it('shows each axis as a number, which is what a colour could not have said', () => {
    const markup = renderToStaticMarkup(<TravelReadout position={at(215.6, 12.0)} />);
    expect(markup).toContain('215.6°');
    expect(markup).toContain('12.0°');
    expect(markup).toContain('RA from home');
    expect(markup).toContain('DEC from home');
  });

  it('does not fold half a turn back toward zero', () => {
    // The whole defect in one assertion: 215.6° and −144.4° are the same mechanical angle and
    // very different amounts of cable. A readout that folded would count *down* while the
    // operator kept winding, which is worse than showing nothing.
    const markup = renderToStaticMarkup(<TravelReadout position={at(215.6, 0)} />);
    expect(markup).toContain('215.6°');
    expect(markup).not.toContain('144.4');
  });

  it('renders an explicit unknown rather than 0.0° when the node reports no travel', () => {
    // A mount with no home reference — an INDI or Alpaca device, or the simulator — sends null.
    // Rendering that as 0.0° would tell the operator the axis is at home, which is the exact
    // false confidence this task exists to remove.
    const markup = renderToStaticMarkup(<TravelReadout position={at(null, null)} />);
    expect(markup).toContain('—');
    expect(markup).not.toContain('0.0°');
  });

  it('renders an explicit unknown before any telemetry has arrived', () => {
    const markup = renderToStaticMarkup(<TravelReadout position={{ state: 'unknown' }} />);
    expect(markup).toContain('—');
    expect(markup).not.toContain('0.0°');
  });

  it('carries a gloss for a screen reader, since the numbers alone do not say what they are', () => {
    const markup = renderToStaticMarkup(<TravelReadout position={at(30, 10)} />);
    expect(markup).toContain('sr-only');
    expect(markup).toContain('home pose');
  });
});
