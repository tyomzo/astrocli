import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

import type { LinkHealth } from '../lib/link/health';
import { LinkHealthBadgeView } from './LinkHealthBadge';

/*
 * The header's link indicator — SDD §8.3(8), §5.9.
 *
 * Two properties are asserted here rather than left to a screenshot. The first is the badge
 * vocabulary: four grades, four glyphs that differ in fill and outline, because night mode makes
 * amber and red two shades of the same colour. The second is the one §8.3(8) states outright —
 * "degradation is explicit, never silent" — which means the numbers appear on a bad link *without*
 * being asked for. A version where they only ever appeared on tap would look identical in review
 * and would make the amber state something you have to already suspect in order to find.
 */

function health(over: Partial<LinkHealth> = {}): LinkHealth {
  return { grade: 'good', rttMs: 42, ageMs: 800, wording: 'in touch with the telescope', ...over };
}

const markup = (over: Partial<LinkHealth> = {}): string =>
  renderToStaticMarkup(<LinkHealthBadgeView health={health(over)} />);

describe('what the badge shows without being asked', () => {
  it('stays two glyphs wide while the link is good', () => {
    // The header also holds the e-stop's fixed slot (USB-03) and the narrowest phone has no room
    // to spare for a measurement that is currently fine.
    const good = markup();

    expect(good).not.toContain('42 ms');
    expect(good).toContain('aria-expanded="false"');
  });

  it('shows both numbers on a degraded link, unprompted', () => {
    const slow = markup({ grade: 'degraded', rttMs: 620, ageMs: 4_200 });

    expect(slow).toContain('620 ms · 4.2 s');
    expect(slow).toContain('aria-expanded="true"');
  });

  it('shows how old the picture is on a link that is down, which is when it matters most', () => {
    const down = markup({ grade: 'down', rttMs: null, ageMs: 31_000, wording: 'not in touch' });

    expect(down).toContain('— · 31.0 s');
  });

  it('keeps showing the age while reconnecting over a picture that has gone old', () => {
    // The badge reads "connecting", which is true and would otherwise be the whole story while
    // forty-second-old coordinates sit on the screen behind it.
    const comingBack = markup({ grade: 'starting', rttMs: null, ageMs: 40_000 });

    expect(comingBack).toContain('40.0 s');
  });
});

describe('the glyph vocabulary', () => {
  it('gives every grade a different shape, not only a different colour', () => {
    const glyphs = (['good', 'degraded', 'starting', 'down', 'idle'] as const).map((grade) => {
      const html = markup({ grade });
      return ['●', '◉', '◐', '⊘', '○'].filter((g) => html.includes(g)).join('');
    });

    expect(new Set(glyphs).size).toBe(5);
  });

  it('does not draw a degraded link the same as a starting one', () => {
    // Both are amber. They read identically to a colour-blind operator and mean opposite things:
    // one says wait, the other says distrust what is on the screen.
    expect(markup({ grade: 'degraded' })).toContain('◉');
    expect(markup({ grade: 'starting' })).toContain('◐');
  });

  it('is hollow rather than red before anything has been attempted', () => {
    expect(markup({ grade: 'idle' })).toContain('○');
    expect(markup({ grade: 'idle' })).not.toContain('⊘');
  });
});

describe('what it announces', () => {
  it('says why it is amber, not merely that it is', () => {
    const slow = markup({
      grade: 'degraded',
      wording: 'running slow — the round trip to the telescope is 620ms',
    });

    expect(slow).toContain('the round trip to the telescope is 620ms');
  });

  it('names the telescope rather than the transport in its own label', () => {
    const label = markup();

    expect(label).toContain('aria-label="Link health');
    for (const jargon of ['socket', 'websocket', 'ticket', 'payload']) {
      expect(label.toLowerCase()).not.toContain(jargon);
    }
  });
});
