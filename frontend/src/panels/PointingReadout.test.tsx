import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

import type { MountPosition, MountStatus } from '../lib/link/protocol';
import { CONFIRMED_MS, PREDICT_LIMIT_MS } from '../lib/predict';
import type { Slot } from '../store/telemetry';
import { PointingBlock } from './PointingReadout';

/*
 * The confirmed / predicted / in-motion / stale treatment is a requirement, not a style — SDD §5.9,
 * §8.3(8).
 *
 * The failure it guards is precise and would pass any review that looked at a laptop: a version
 * where the four states differ only in colour. Night mode collapses every hue toward red (`--warn`
 * and `--fg` become two shades of the same thing) and ~8% of men have a red-green deficiency in
 * any lighting, so a colour-only distinction is correct on the workstation and unreadable in
 * exactly the conditions this app exists for. The same argument the nudge badge is built on.
 *
 * So each state has to carry a mark a monochrome squint can find: a tilde, an arrow, a strike, or
 * nothing at all. Rendered through `react-dom/server`, which needs no DOM.
 */

const T0 = Date.parse('2026-07-30T21:04:05.000Z');

const POSITION: Slot<MountPosition> = {
  state: 'observed',
  at: T0,
  ts: new Date(T0).toISOString(),
  value: {
    ra: 5.588_25,
    dec: -5.391,
    alt: 47.2,
    az: 128.4,
    pier_side: 'west',
    ra_travel: null,
    dec_travel: null,
  },
};

function mount(over: Partial<MountStatus> = {}): Slot<MountStatus> {
  return {
    state: 'observed',
    at: T0,
    ts: new Date(T0).toISOString(),
    value: {
      state: 'idle',
      tracking: false,
      tracking_mode: null,
      slewing: false,
      parked: false,
      ...over,
    },
  };
}

function markup(ageMs: number, status: Slot<MountStatus> = mount()): string {
  return renderToStaticMarkup(
    <PointingBlock
      position={POSITION}
      status={status}
      nowMs={T0 + ageMs}
      skewMs={0}
      describeState="Connected, tracking off — the sky is drifting past."
    />,
  );
}

describe('how a coordinate says how much to trust it', () => {
  it('marks a worked-out value with a tilde, and an in-cadence one with nothing', () => {
    expect(markup(0)).not.toContain('~');
    expect(markup(CONFIRMED_MS + 1)).toContain('~');
  });

  it('strikes a stale value through rather than leaving a number that looks live', () => {
    const stale = markup(PREDICT_LIMIT_MS + 1);

    expect(stale).toContain('<s ');
    // And the tilde is gone with it: a struck-out tilde would claim the app is still working the
    // number out, which is the one thing the guardrail exists to stop.
    expect(stale).not.toContain('~');
  });

  it('marks a slewing mount as moving, without a tilde, at any age', () => {
    const slewing = markup(2_000, mount({ state: 'slewing', slewing: true }));

    expect(slewing).toContain('→');
    expect(slewing).not.toContain('~');
    expect(slewing).not.toContain('<s ');
  });

  it('gives each state a mark that survives a monochrome squint', () => {
    // The four marks are distinct as *shapes*: nothing, a tilde, an arrow, a strike. If a change
    // ever collapses two of them into one glyph plus a colour, this is where it fails.
    const marks = [
      markup(0),
      markup(CONFIRMED_MS + 1),
      markup(2_000, mount({ state: 'slewing', slewing: true })),
      markup(PREDICT_LIMIT_MS + 1),
    ].map((html) => [html.includes('~'), html.includes('→'), html.includes('<s ')].join('/'));

    expect(new Set(marks).size).toBe(4);
  });

  it('never puts a tilde on altitude or azimuth, which nothing extrapolates', () => {
    // They change under a tracking mount as well as an idle one and computing them needs the site
    // coordinates the node has and this app does not (§5.2.3). A tilde there would promise a
    // freshness the app cannot deliver.
    const predicted = markup(CONFIRMED_MS + 1);

    expect(predicted.match(/~/g)).toHaveLength(2); // RA and DEC only.
    expect(predicted).toContain('as last reported');
  });
});

describe('the sentence under the coordinates', () => {
  it('says how old the reading is in every state, because §8.3 is about knowing before acting', () => {
    expect(markup(400)).toContain('Reported 0.4s ago.');
    expect(markup(2_400)).toContain('Nothing reported for 2.4s.');
    expect(markup(8_000)).toContain('Nothing reported for 8.0s');
    expect(markup(600, mount({ state: 'slewing', slewing: true }))).toContain('Moving.');
  });

  it('explains that alt and az are not being worked out, rather than leaving it to be inferred', () => {
    expect(markup(2_400)).toContain('alt and az are as last reported');
  });

  it('says outright that a stale reading is not current', () => {
    expect(markup(PREDICT_LIMIT_MS + 1)).toContain('not current');
  });

  it('does not pretend to a position it never received', () => {
    const nothing = renderToStaticMarkup(
      <PointingBlock
        position={{ state: 'unknown' }}
        status={mount({ state: 'disconnected' })}
        nowMs={T0}
        skewMs={0}
        describeState="Not connected."
      />,
    );

    expect(nothing).toContain('has not reported a position');
    expect(nothing).not.toContain('~');
  });
});
