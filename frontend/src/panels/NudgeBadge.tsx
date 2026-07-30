import type { ReactNode } from 'react';

import type { NudgeAvailability } from '../lib/nudge';

/**
 * The nudge affordance — SDD §5.9, bottom-right of the image surface.
 *
 * # Position
 *
 * Bottom-right is where a thumb rests on a phone, which suits the most frequent control in the
 * app. That it is diagonally as far as possible from the header e-stop is the point, not a
 * coincidence: the frequent control should be easy and the irreversible one should take a
 * deliberate reach.
 *
 * # State is encoded twice, and shape comes first
 *
 * Filled `⊕` on a solid border when nudging is available; hollow `⊘` on a dashed border when it is
 * not. Colour agrees — `--ok` and `--danger` — but carries none of the meaning on its own, because
 * night mode collapses every hue toward red (the two tokens become two shades of the same colour)
 * and roughly 8% of men have a red-green deficiency in any lighting. A badge that distinguished
 * its states by colour would be correct on a laptop and unreadable in the field, which is the
 * worst combination available.
 *
 * # Tapping an unavailable badge explains why
 *
 * It is never inert and it is never `disabled`, because a `disabled` button fires no event at all
 * and the operator learns nothing — including whether they missed. §5.9 requires the explanation;
 * `lib/nudge.ts` produces it, so the sentence is decided next to the condition rather than here.
 */
export function NudgeBadge({
  availability,
  onSummon,
  onExplain,
}: {
  availability: NudgeAvailability;
  onSummon: () => void;
  onExplain: () => void;
}): ReactNode {
  const available = availability.available;

  return (
    <button
      type="button"
      onClick={available ? onSummon : onExplain}
      aria-label={available ? 'Show the nudge controls' : 'Why nudging is unavailable'}
      aria-disabled={!available}
      className={`absolute right-3 bottom-3 flex size-control items-center justify-center rounded-full bg-surface/80 text-3xl leading-none backdrop-blur ${
        available ? 'border-2 border-ok text-ok' : 'border-2 border-dashed border-danger text-danger'
      }`}
    >
      <span aria-hidden="true">{available ? '⊕' : '⊘'}</span>
    </button>
  );
}
