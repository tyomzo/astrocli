import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';

import { nudgeAvailability } from '../lib/nudge';
import { selectLink, selectMountStatus, useTelemetryStore } from '../store/telemetry';
import { NudgeBadge } from './NudgeBadge';
import { NudgeOverlay } from './NudgeOverlay';

/**
 * The persistent centre of the app — SDD §5.9.
 *
 * §5.9 calls `FRAME` and `STACK` "two sources sharing one image surface", not two panels: live
 * view answers "is this framed and focused", the stack answers "is this working", and they are
 * the same rectangle at different times. **This component is that rectangle.** M1-T09 fills it
 * with live view and M1-T14 with the stack preview; until then it says so, because a black box
 * with no explanation is indistinguishable from a crashed panel — which §5.9 spends a paragraph
 * warning about for exactly this widget.
 *
 * What M1-T04 owes it now is the furniture that has to be right from the start: the aspect box,
 * the night-mode image filter hook (`.image-surface`, so the filter is a theme decision and not a
 * panel decision), and the nudge affordance in the bottom-right corner with the D-pad that
 * overlays rather than displaces.
 *
 * The overlay closes itself when nudging stops being possible — the mount disconnecting, a goto
 * starting, the link dropping. Leaving a D-pad up over an image when none of its buttons can do
 * anything is the "tap and nothing happens" failure the badge exists to prevent, reintroduced one
 * level down.
 */
export function ImageSurface(): ReactNode {
  const link = useTelemetryStore(selectLink);
  const mount = useTelemetryStore(selectMountStatus);
  const availability = nudgeAvailability(link, mount);

  const [summoned, setSummoned] = useState(false);
  const [explanation, setExplanation] = useState<string | null>(null);

  const available = availability.available;
  useEffect(() => {
    if (!available) setSummoned(false);
  }, [available]);

  return (
    <section className="flex flex-col gap-2">
      <div className="relative overflow-hidden rounded-lg border border-edge bg-raised">
        <div className="image-surface flex aspect-[4/3] w-full items-center justify-center p-6 text-center">
          <p className="text-sm text-faint">
            No image source in this build. Live view arrives with M1-T09 and the stack preview with
            M1-T14; both render here, on this surface, rather than in panels of their own.
          </p>
        </div>

        {summoned ? (
          <NudgeOverlay onDismiss={() => setSummoned(false)} />
        ) : (
          <NudgeBadge
            availability={availability}
            onSummon={() => {
              setExplanation(null);
              setSummoned(true);
            }}
            onExplain={() =>
              setExplanation(availability.available ? null : availability.reason)
            }
          />
        )}
      </div>

      {explanation !== null && (
        <p role="status" className="rounded-md border border-edge bg-overlay p-3 text-sm text-fg">
          {explanation}
        </p>
      )}
    </section>
  );
}
