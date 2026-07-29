import { useState } from 'react';
import type { ReactNode } from 'react';

/**
 * The e-stop — USB-03, USB-12, SDD §5.9.
 *
 * Three properties are fixed from M0 and must not be renegotiated by a later panel:
 *
 *  * **Top-right of the header, on every screen.** SDD §5.9 lists it among the things that are
 *    fixed rather than slotted. Constant position is what lets it be hit without looking, which is
 *    the entire requirement (USB-03) — a button that moves is a button you have to find.
 *  * **Larger than any other control** (`size-estop`, above the 60–70 px band the D-pad and
 *    capture use). It is also diagonally distant from the nudge badge in the image surface's
 *    bottom-right corner: the frequent control should be easy to reach and the irreversible one
 *    should take a deliberate stretch.
 *  * **Its unavailability is visible.** `/api/mount/estop` does not exist until M1-T03, so in M0
 *    this button cannot stop anything. Rendering it as if it could is the worst possible failure
 *    for this particular control, so it carries the hollow-plus-slash treatment SDD §5.9 defines
 *    for the nudge badge — shape first, colour second — and says why when tapped.
 *
 * Tapping it rather than `disabled`-ing it is deliberate: SDD §5.9 requires an unavailable
 * affordance to explain itself rather than silently doing nothing, and a `disabled` button fires
 * no event at all. It is also the only way the operator learns the difference between "the e-stop
 * is not wired up in this build" and "I missed".
 */
export function EStopButton(): ReactNode {
  const [explained, setExplained] = useState(false);

  // M1-T03 lands `POST /api/mount/estop` (SDD §5.8.1) and this becomes a real command.
  const armed = false;

  return (
    <div className="relative">
      <button
        type="button"
        aria-disabled={!armed}
        aria-label="Emergency stop"
        onClick={() => setExplained((shown) => !shown)}
        className="flex size-estop flex-col items-center justify-center rounded-lg border-4 border-danger text-danger transition-colors active:bg-danger active:text-on-danger"
      >
        <span className="text-lg leading-none font-black tracking-tight">STOP</span>
        {!armed && (
          // The redundant channel: a slash, so the state survives night mode and a red-green
          // deficiency both.
          <span aria-hidden="true" className="mt-1 text-xl leading-none">
            ⊘
          </span>
        )}
      </button>

      {explained && !armed && (
        <p
          role="status"
          className="absolute top-full right-0 z-10 mt-2 w-64 rounded-md border border-edge bg-overlay p-3 text-sm text-fg"
        >
          The e-stop is not connected in this build. The mount facade and{' '}
          <code className="font-mono text-muted">/api/mount/estop</code> arrive with M1-T03 — until
          then nothing in this app can move or stop a motor.
        </p>
      )}
    </div>
  );
}
