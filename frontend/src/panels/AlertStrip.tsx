import { useState } from 'react';
import type { ReactNode } from 'react';

import { age } from '../lib/format';
import { useNow } from '../lib/useNow';
import type { AlertSeverity } from '../lib/link/protocol';
import { selectAlerts, useTelemetryStore } from '../store/telemetry';

/**
 * The newest `alert` event, until the operator dismisses it — SDD §4.3's `alert` topic, USB-04.
 *
 * One at a time, and only recent ones. An alert list on the operating screens would compete with
 * the thing the operator is doing; the full history lives in the system panel. What must not
 * happen is an alert arriving unseen, because the codes that reach this topic are the ones that
 * explain a surprise: `SLEW_TTL_EXPIRED` is why an axis stopped mid-hold, `WORKER_RESTARTED` is
 * why the stack went quiet, `LIMIT_MERIDIAN` is why tracking stopped.
 *
 * Dismissal is by sequence number rather than a boolean, so dismissing one alert does not hide
 * the next one that arrives a second later.
 */
const VISIBLE_FOR_MS = 120_000;

const GLYPH: Record<AlertSeverity, string> = {
  info: '○',
  warning: '◐',
  critical: '⊘',
};

const TONE: Record<AlertSeverity, string> = {
  info: 'border-edge text-muted',
  warning: 'border-warn text-fg',
  critical: 'border-danger text-fg',
};

export function AlertStrip(): ReactNode {
  const alerts = useTelemetryStore(selectAlerts);
  const now = useNow(5000);
  const [dismissedThrough, setDismissedThrough] = useState(0);

  const newest = alerts[0];
  if (newest === undefined || newest.seq <= dismissedThrough || now - newest.at > VISIBLE_FOR_MS) {
    return null;
  }

  return (
    <div
      role="status"
      className={`mb-3 flex items-start justify-between gap-3 rounded-md border p-3 text-sm ${TONE[newest.value.severity]}`}
    >
      <span className="min-w-0">
        <span aria-hidden="true">{GLYPH[newest.value.severity]} </span>
        <span className="font-mono text-muted">{newest.value.code}</span> {newest.value.message}{' '}
        <span className="text-faint">({age(newest.at, now)})</span>
      </span>
      <button
        type="button"
        onClick={() => setDismissedThrough(newest.seq)}
        aria-label="Dismiss this alert"
        className="flex size-touch shrink-0 items-center justify-center text-muted"
      >
        <span aria-hidden="true">✕</span>
      </button>
    </div>
  );
}
