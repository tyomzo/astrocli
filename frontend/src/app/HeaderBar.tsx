import type { ReactNode } from 'react';

import { useDeploymentLabel } from '../lib/deployment';
import { clockUtc } from '../lib/format';
import { useNow } from '../lib/useNow';
import type { TelemetryState } from '../store/telemetry';
import {
  selectCameraStatus,
  selectLink,
  selectMountStatus,
  selectStackStatus,
  useTelemetryStore,
} from '../store/telemetry';
import { useUiStore } from '../store/ui';
import type { Status } from '../ui/StatusBadge';
import { StatusBadge } from '../ui/StatusBadge';
import { EStopButton } from './EStopButton';
import { LinkHealthBadge } from './LinkHealthBadge';

/**
 * The header status bar — USB-04, SDD §5.9.
 *
 * Layout is `[ badges | clock ] [ e-stop ]`, and the e-stop's column is a fixed slot that exists
 * whether or not anything else fits. On the narrowest phone the badges wrap and the clock is the
 * first thing to go; the e-stop never moves, because USB-03 is about hitting it without looking.
 *
 * # Why there is a `link` badge the SDD sketch does not draw
 *
 * §5.9 draws `●mnt ●cam ○stk`. Those three describe subsystems the *node* reports on, which makes
 * every one of them a claim that is only as current as the socket carrying it. Without a fourth
 * badge for the socket itself, a dead link renders as three healthy subsystems — the precise
 * failure §8.3(8) means by "degradation is explicit, never silent". M1-T15 hung the RTT and
 * telemetry-age numerals off that badge; it owns its own file now, and its own tap.
 *
 * For the same reason the three subsystem badges go to `unknown` — hollow, "not probed yet" —
 * whenever the link is not live. They are not reporting a subsystem that is down; they are
 * reporting that nobody is currently telling us anything about it, which is a different fact and
 * the honest one.
 *
 * # The strip is two buttons, not one
 *
 * It used to be one, opening the system detour, on the reasoning that the operator who wants to
 * know *why* a badge is hollow is already looking at it. That still holds for `mnt`/`cam`/`stk`:
 * the answer is node health, the credential and the device capabilities, all of which live on that
 * screen. It does not hold for `link`, whose answer is two numbers that fit in the header — so the
 * link badge reveals them in place instead, and since a button cannot nest inside a button, the
 * strip became a row of two. The deployment marker sits outside both because it is not a control.
 */
export function HeaderBar(): ReactNode {
  const link = useTelemetryStore(selectLink);
  const mount = useTelemetryStore(selectMountStatus);
  const camera = useTelemetryStore(selectCameraStatus);
  const stack = useTelemetryStore(selectStackStatus);
  const deployment = useDeploymentLabel();
  const openSystem = useUiStore((state) => state.openSystem);

  const live = link.phase === 'live';

  return (
    <header className="sticky top-0 z-20 border-b border-edge bg-surface/95 backdrop-blur">
      <div className="mx-auto flex w-full max-w-5xl items-start justify-between gap-3 px-3 py-2">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
          {deployment !== null && <DeploymentMarker label={deployment} />}
          <LinkHealthBadge />
          <button
            type="button"
            onClick={openSystem}
            aria-label="Connection detail, credential and device capabilities"
            // Deliberately **not** `flex-1`. Letting this button absorb the leftover width packs
            // it beside the link badge and saves a row while the link is green — and then, when
            // the badge expands its numerals, the leftover width collapses and these three wrap
            // onto three lines. Measured on a 412 px viewport: 97 px green against 185 px amber.
            // A header that grows the moment the link degrades pushes the whole screen down at
            // exactly the moment the operator is reading it, so the fixed two-row shape wins.
            className="flex min-h-touch min-w-0 flex-wrap items-center gap-x-4 gap-y-1 rounded-md px-1 text-left"
          >
            <StatusBadge label="mnt" status={live ? mountStatus(mount) : 'unknown'} />
            <StatusBadge
              label="cam"
              status={live ? connectedStatus(camera, (value) => value.connected) : 'unknown'}
            />
            <StatusBadge
              label="stk"
              status={live ? connectedStatus(stack, (value) => value.connected) : 'unknown'}
            />
            <Clock />
          </button>
        </div>
        <EStopButton />
      </div>
    </header>
  );
}

/**
 * Says out loud that this is not the production node.
 *
 * First in the strip, not tucked at the end: the point is to be seen before anything is touched.
 * It is deliberately the one element here that does not use the muted palette — a marker that
 * blends in is a marker nobody reads, and the failure it guards against is an operator sending
 * commands to the wrong rig because two home-screen icons looked alike.
 */
function DeploymentMarker({ label }: { label: string }): ReactNode {
  return (
    <span className="rounded-sm border border-accent px-1.5 py-0.5 font-mono text-xs font-semibold uppercase tracking-wide text-accent">
      {label}
    </span>
  );
}

function mountStatus(slot: TelemetryState['mountStatus']): Status {
  if (slot.state === 'unknown') return 'unknown';
  switch (slot.value.state) {
    case 'idle':
    case 'slewing':
    case 'parked':
      return 'ok';
    case 'fault':
      return 'down';
    case 'disconnected':
      return 'down';
  }
}

function connectedStatus<T>(
  slot: { state: 'unknown' } | { state: 'observed'; value: T },
  connected: (value: T) => boolean,
): Status {
  if (slot.state === 'unknown') return 'unknown';
  return connected(slot.value) ? 'ok' : 'down';
}

/**
 * UTC wall clock.
 *
 * SDD §5.9's sketches put **LST** here. LST needs the site longitude and the coordinate machinery
 * of `astroctl-planning`, which is Phase 2a — and a header that showed local time labelled as
 * sidereal would be worse than one that shows neither. UTC is what an observing log is written
 * in, so it earns the slot until then.
 */
function Clock(): ReactNode {
  const now = useNow();

  return (
    <span className="tabular font-mono text-sm text-muted">
      {clockUtc(new Date(now))} <span className="text-faint">UTC</span>
    </span>
  );
}
