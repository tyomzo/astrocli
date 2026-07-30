import type { ReactNode } from 'react';

import { SKEW_WARN_MS, isSkewWorthWarning } from '../lib/clock';
import { selectSkewMs, useTelemetryStore } from '../store/telemetry';

/**
 * "This device's clock is wrong" — SDD §5.8.1's skew warning, REL-14.
 *
 * # Why the operator is told about something the app already fixed
 *
 * Every command's `issued_at` is corrected by the measured offset before it leaves (`lib/clock.ts`),
 * so a device thirty seconds out still drives the telescope perfectly. Warning anyway is deliberate,
 * for two reasons the correction cannot address:
 *
 *  * **The correction only covers this app.** The same wrong clock is on the photographs the
 *    operator will pull off the phone, on the notes they write, and on any log they correlate with
 *    the session by hand. REL-14 is about clock discipline across the system, not about whether one
 *    HTTP header lands inside a 2 s budget.
 *  * **A clock that is wrong by half a minute is usually a symptom.** A phone that has been out of
 *    signal all night, or one whose automatic time was turned off, is a phone that may be about to
 *    surprise its owner in other ways.
 *
 * # Separate from `LinkBanner`, and that is the point
 *
 * `LinkBanner` renders nothing while the link is live. A clock warning is only *measurable* while
 * the link is live — the estimate comes from `pong` — so folding it in there would produce a
 * warning that can never be shown. They also mean opposite things: the banner says the app is out
 * of touch with the telescope, this says the two are in touch and disagree about the time.
 */
export function ClockSkewNote(): ReactNode {
  return <ClockSkewNoteView skewMs={useTelemetryStore(selectSkewMs)} />;
}

/** The markup for one measured offset. Split out so a test can name the number it is checking. */
export function ClockSkewNoteView({ skewMs }: { skewMs: number | null }): ReactNode {
  if (!isSkewWorthWarning(skewMs)) return null;

  // Non-null by the guard above; restated for the type checker rather than asserted away.
  const skew = skewMs ?? 0;
  const seconds = Math.round(Math.abs(skew) / 1000);
  // "behind" when the node is ahead: the sentence is about *this device's* clock, which is the
  // one the operator can do something about.
  const direction = skew > 0 ? 'behind' : 'ahead of';

  return (
    <div
      role="status"
      className="mb-3 flex flex-col rounded-md border border-warn p-3 text-sm text-fg"
    >
      <span>
        <span aria-hidden="true" className="text-warn">
          ◷{' '}
        </span>
        {`This device's clock is about ${seconds}s ${direction} the telescope's. Commands are still ` +
          `being sent with the corrected time, so nothing is blocked — but photograph timestamps ` +
          `and anything else this device dates will be off by the same amount until its clock is ` +
          `set.`}
      </span>
    </div>
  );
}

/** Re-exported so a panel test can name the threshold it is checking. */
export { SKEW_WARN_MS };
