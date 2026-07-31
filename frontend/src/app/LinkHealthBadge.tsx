import { useState } from 'react';
import type { ReactNode } from 'react';

import type { LinkHealth } from '../lib/link/health';
import { isWorthShowing, linkHealth, linkNumerals } from '../lib/link/health';
import { useNow } from '../lib/useNow';
import {
  selectLastEvent,
  selectLink,
  selectRttMs,
  selectSkewMs,
  useTelemetryStore,
} from '../store/telemetry';
import type { Status } from '../ui/StatusBadge';
import { StatusBadge } from '../ui/StatusBadge';

/**
 * The header's link indicator — SDD §8.3(8), §5.9's link-health surfacing.
 *
 * # Extending the badge rather than adding a second indicator
 *
 * M1-T04 added a `link` badge because §5.9's `●mnt ●cam ○stk` sketch describes three subsystems
 * the *node* reports on, so a dead link renders as three healthy subsystems unless something
 * watches the link itself. §8.3(8) then wants RTT and telemetry age in the header. Those are the
 * same fact at two levels of detail, so they hang off the one badge: two indicators for one link
 * would eventually disagree, at night, on a phone, about whether to trust the screen.
 *
 * # The numbers appear on tap — and, unasked, when they are bad
 *
 * §8.3(8): "degradation is explicit, never silent". So amber and red show `620 ms · 4.2 s` without
 * being asked, and the tap only exists for a green link the operator wants to interrogate anyway —
 * before a goto, say. Numbers that lived *only* behind a tap would make the degraded state
 * something you have to already suspect in order to discover, which inverts the requirement.
 *
 * Green and collapsed is the common case and it stays two glyphs wide, because the header also
 * holds the e-stop's fixed slot (USB-03) and the narrowest phone has no room to spare for a
 * measurement that is currently fine.
 *
 * # Why this is its own button, and the strip is no longer one
 *
 * The strip used to be a single button opening the system detour. That reasoning still holds for
 * `mnt`/`cam`/`stk` — the operator asking why a badge is hollow is already looking at it, and the
 * answer is on that screen. It does not hold here: the answer to "why is link amber" is two
 * numbers that fit in the header, and sending the operator to another screen for them would be a
 * navigation puzzle solved in the dark. A button cannot nest inside a button, so the strip is now
 * a row of two: this, and the rest.
 */
export function LinkHealthBadge(): ReactNode {
  const link = useTelemetryStore(selectLink);
  const rttMs = useTelemetryStore(selectRttMs);
  const lastEvent = useTelemetryStore(selectLastEvent);
  const skewMs = useTelemetryStore(selectSkewMs);
  // One second is the cadence of the thing being aged (§4.3's `mount.position`), so a faster tick
  // would re-render the header for a number that has not changed.
  const now = useNow();

  return (
    <LinkHealthBadgeView
      health={linkHealth({ link, rttMs, lastEvent, nowMs: now, skewMs: skewMs ?? 0 })}
    />
  );
}

/** The markup for one graded link. Split out so a test can name the state it is checking. */
export function LinkHealthBadgeView({ health }: { health: LinkHealth }): ReactNode {
  const [asked, setAsked] = useState(false);
  const showing = asked || isWorthShowing(health);

  return (
    <button
      type="button"
      onClick={() => setAsked((was) => !was)}
      aria-expanded={showing}
      aria-label={`Link health: ${health.wording}. Tap for the round trip and how old the picture is.`}
      className="flex min-h-touch items-center rounded-md px-1"
    >
      <StatusBadge
        label="link"
        status={STATUS[health.grade]}
        detail={health.wording}
        trailing={
          showing ? (
            <span aria-hidden="true" className="tabular ml-1 font-mono text-xs text-muted">
              {linkNumerals(health)}
            </span>
          ) : undefined
        }
      />
    </button>
  );
}

/**
 * Grade → glyph.
 *
 * `idle` is `unknown` rather than `down`: nothing has been tried yet, which is not the same as
 * something having failed, and M1-T04's header already draws that distinction for the three
 * subsystem badges.
 */
const STATUS: Record<LinkHealth['grade'], Status> = {
  good: 'ok',
  degraded: 'degraded',
  starting: 'starting',
  down: 'down',
  idle: 'unknown',
};
