import { useEffect, useState } from 'react';
import type { ReactNode } from 'react';

import { clockUtc } from '../lib/format';
import type { NodeSlot } from '../store/nodes';
import { selectNode, useNodesStore } from '../store/nodes';
import type { Status } from '../ui/StatusBadge';
import { StatusBadge } from '../ui/StatusBadge';
import { EStopButton } from './EStopButton';

/**
 * The header status bar — USB-04, SDD §5.9.
 *
 * Layout is `[ badges | clock ] [ e-stop ]`, and the e-stop's column is a fixed slot that exists
 * whether or not anything else fits. On the narrowest phone the badges wrap and the clock is the
 * first thing to go; the e-stop never moves, because USB-03 is about hitting it without looking.
 *
 * M0 shows the two nodes that exist. M1-T04 adds mount and camera badges into the same strip —
 * `●mnt ●cam ○stk` in the SDD sketch — which is why the badges are a wrapping flex row rather
 * than a three-column grid sized to today's contents.
 */
export function HeaderBar(): ReactNode {
  const field = useNodesStore(selectNode('field'));
  const stack = useNodesStore(selectNode('stack'));

  return (
    <header className="sticky top-0 z-20 border-b border-edge bg-surface/95 backdrop-blur">
      <div className="mx-auto flex w-full max-w-5xl items-start justify-between gap-3 px-3 py-2">
        <div className="flex min-w-0 flex-wrap items-center gap-x-4 gap-y-1 pt-1">
          <StatusBadge label="field" status={badgeStatus(field)} />
          <StatusBadge label="stack" status={badgeStatus(stack)} />
          <Clock />
        </div>
        <EStopButton />
      </div>
    </header>
  );
}

function badgeStatus(slot: NodeSlot): Status {
  switch (slot.observation.state) {
    case 'unknown':
      return 'unknown';
    case 'unavailable':
      return 'down';
    case 'observed':
      return slot.observation.health.status === 'ok' ? 'ok' : 'starting';
  }
}

/**
 * UTC wall clock.
 *
 * SDD §5.9's sketches put **LST** here. LST needs the site longitude and the coordinate machinery
 * of `astroctl-planning`, which is M1 — and a header that showed local time labelled as sidereal
 * would be worse than one that shows neither. UTC is what an observing log is written in, so it
 * earns the slot until M1 replaces it.
 */
function Clock(): ReactNode {
  const [now, setNow] = useState(() => new Date());

  useEffect(() => {
    const timer = window.setInterval(() => setNow(new Date()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  return (
    <span className="tabular font-mono text-sm text-muted">
      {clockUtc(now)} <span className="text-faint">UTC</span>
    </span>
  );
}
