import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';

import { NudgeBadge } from './NudgeBadge';

/*
 * The badge's encoding is a requirement, not a style — SDD §5.9.
 *
 * "The state must be encoded redundantly — a filled badge when available, hollow with a slash when
 * it is not — so the distinction survives both night mode and the operator." A change that made
 * the two states differ only in colour would be invisible in review, would look correct on a
 * laptop, and would be unreadable in exactly the conditions this app exists for. So the property
 * is asserted where a reviewer can see it fail.
 *
 * Rendered through `react-dom/server`, which needs no DOM: this is about the markup the component
 * produces, not about clicking it.
 */

const markup = (available: boolean): string =>
  renderToStaticMarkup(
    <NudgeBadge
      availability={available ? { available: true } : { available: false, reason: 'mount is down' }}
      onSummon={() => {}}
      onExplain={() => {}}
    />,
  );

describe('nudge badge encoding', () => {
  it('uses a different glyph for each state, not only a different colour', () => {
    expect(markup(true)).toContain('⊕');
    expect(markup(true)).not.toContain('⊘');

    expect(markup(false)).toContain('⊘');
    expect(markup(false)).not.toContain('⊕');
  });

  it('uses a different border style too, so the shape survives a monochrome theme', () => {
    expect(markup(false)).toContain('border-dashed');
    expect(markup(true)).not.toContain('border-dashed');
  });

  it('stays clickable when unavailable, because tapping it must explain why', () => {
    // `disabled` would fire no event at all, and the operator would not learn whether they missed.
    const html = markup(false);
    expect(html).toContain('aria-disabled="true"');
    expect(html).not.toMatch(/\sdisabled(=|\s|>)/);
  });

  it('labels both states for a screen reader', () => {
    expect(markup(true)).toContain('aria-label="Show the nudge controls"');
    expect(markup(false)).toContain('aria-label="Why nudging is unavailable"');
  });
});
