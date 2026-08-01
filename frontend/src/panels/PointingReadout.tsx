import type { ReactNode } from 'react';

import { formatDecDegrees, formatDegrees, formatRaHours } from '../lib/coords';
import type { Reckoning } from '../lib/predict';
import { PREDICT_TICK_MS, reckon } from '../lib/predict';
import { useNow } from '../lib/useNow';
import type { MountPosition, MountStatus } from '../lib/link/protocol';
import type { Slot } from '../store/telemetry';
import {
  selectMountPosition,
  selectMountStatus,
  selectSkewMs,
  useTelemetryStore,
} from '../store/telemetry';
import { Card, Field } from '../ui/Card';

/**
 * Where the telescope is pointing, in astronomical notation — USB-05, SDD §5.9's `now …` block.
 *
 * # Four ways to read a number, and the shape says which
 *
 * §5.9 requires a visually distinct "aging" style and §8.3(8) requires that degradation is never
 * silent. `lib/predict.ts` decides which of the four states applies; this file decides what each
 * one looks like, and the rule it follows is the nudge badge's: **shape and typography carry the
 * state, colour agrees with it.** Night mode collapses every hue toward red, so a distinction made
 * only in colour is correct on a laptop and invisible in the field.
 *
 *   * **confirmed** — plain, full brightness. A report landed within the cadence.
 *   * **predicted** — a leading `~` and reduced opacity. Nothing has arrived for longer than the
 *     cadence and the app is carrying the last report forward.
 *   * **in motion** — a leading `→` and reduced opacity. The mount is slewing, so the numbers are
 *     *held*: nothing is carried forward, because a goto outruns any model within a second.
 *   * **stale** — struck through, amber. The guardrail tripped; nothing here is current, and it is
 *     drawn so that it cannot be mistaken for a live reading at a glance.
 *
 * The markers sit to the left of the value and the values are right-aligned, so the digits do not
 * shift when the state changes — a column of coordinates that jumps sideways once a second is
 * harder to read than one that does not.
 *
 * # alt and az are never carried forward
 *
 * Both change under a tracking mount as well as an idle one, and computing them needs the site
 * coordinates, which the node has and this app does not (§5.2.3). So they wear the aging opacity
 * with the rest of the block but never the tilde, and the sentence under the block says why. A
 * second, worse implementation of the number a slew is refused on is the last thing this readout
 * should contain.
 *
 * # Why the block ticks faster than the data
 *
 * `useNow(PREDICT_TICK_MS)` is five renders a second of one card. It is not a poller and asks the
 * node for nothing (§5.9's "no REST polling" is about requests, not about clocks): it exists
 * because a dead-reckoned coordinate that only moved when an event arrived would not be a
 * prediction, it would be the event with extra steps.
 */
export function PointingReadout(): ReactNode {
  const position = useTelemetryStore(selectMountPosition);
  const status = useTelemetryStore(selectMountStatus);
  const skewMs = useTelemetryStore(selectSkewMs);
  const now = useNow(PREDICT_TICK_MS);


  return (
    <Card title="Pointing">
      <PointingBlock
        position={position}
        status={status}
        nowMs={now}
        skewMs={skewMs ?? 0}
        describeState={describeMount(status)}
      />
    </Card>
  );
}

/**
 * The coordinates and their freshness. Split from the card so a test can name the state it checks
 * without a store, a token or a clock.
 */
export function PointingBlock({
  position,
  status,
  nowMs,
  skewMs,
  describeState,
}: {
  position: Slot<MountPosition>;
  status: Slot<MountStatus>;
  nowMs: number;
  skewMs: number;
  describeState: string;
}): ReactNode {
  if (position.state === 'unknown') {
    return (
      <p className="text-sm text-muted">
        The mount has not reported a position. {describeState}
      </p>
    );
  }

  const reckoning = reckon({
    position,
    status: status.state === 'observed' ? status.value : null,
    nowMs,
    skewMs,
  });

  return (
    <>
      <dl>
        <Field label="RA" value={<Reading r={reckoning} carried>{formatRaHours(reckoning.ra)}</Reading>} />
        <Field
          label="DEC"
          value={<Reading r={reckoning} carried>{formatDecDegrees(reckoning.dec)}</Reading>}
        />
        <Field label="alt" value={<Reading r={reckoning}>{formatDegrees(position.value.alt)}</Reading>} />
        <Field label="az" value={<Reading r={reckoning}>{formatDegrees(position.value.az)}</Reading>} />
        <Field label="pier" value={<Reading r={reckoning}>{position.value.pier_side}</Reading>} />
      </dl>

      <p className={`mt-2 text-sm ${reckoning.kind === 'stale' ? 'text-warn' : 'text-muted'}`}>
        {freshnessSentence(reckoning)}
      </p>
      <p className="mt-1 text-sm text-muted">{describeState}</p>
    </>
  );
}

/**
 * One value, wearing the treatment its state earns.
 *
 * `carried` marks the two numbers the model actually moves. Without it a tilde would appear on an
 * altitude nobody extrapolated, which is a promise of freshness the app cannot keep.
 */
function Reading({
  r,
  carried = false,
  children,
}: {
  r: Reckoning;
  carried?: boolean;
  children: ReactNode;
}): ReactNode {
  switch (r.kind) {
    case 'confirmed':
      return <span>{children}</span>;
    case 'predicted':
      return (
        <span className="opacity-70">
          {carried && <span aria-hidden="true">~</span>}
          {children}
          <span className="sr-only">
            {carried ? ' (worked out, not reported)' : ' (as last reported)'}
          </span>
        </span>
      );
    case 'moving':
      return (
        <span className="opacity-70">
          <span aria-hidden="true">→</span>
          {children}
          <span className="sr-only"> (the mount is moving)</span>
        </span>
      );
    case 'stale':
      return (
        <span className="text-warn opacity-70">
          {/* A line through it, not merely a dimmer colour: a frozen number that still looks like
              a number is the failure §8.3(8) calls silent degradation. */}
          <s className="decoration-2">{children}</s>
          <span className="sr-only"> (not current)</span>
        </span>
      );
  }
}

/**
 * The sentence under the block.
 *
 * It exists because the markers say *that* something is different and an operator standing in a
 * field deserves to be told *what*. Each one names how old the reading is, because §8.3 is about
 * knowing that before issuing a command rather than after.
 */
function freshnessSentence(r: Reckoning): string {
  const seconds = (r.ageMs / 1000).toFixed(1);
  switch (r.kind) {
    case 'confirmed':
      return `Reported ${seconds}s ago.`;
    case 'predicted':
      return (
        `Nothing reported for ${seconds}s. RA and DEC are worked out from the last report and ` +
        `the rate the mount is running at; alt and az are as last reported.`
      );
    case 'moving':
      return (
        `Moving. These are the numbers from ${seconds}s ago — nothing is worked out ahead while ` +
        `the mount is still turning.`
      );
    case 'stale':
      return (
        `Nothing reported for ${seconds}s, so the numbers above have been left where they were ` +
        `and are not current. They are struck out for that reason.`
      );
  }
}

function describeMount(status: Slot<MountStatus>): string {
  if (status.state === 'unknown') {
    return 'The mount has not reported its state.';
  }
  const { state, tracking, slewing, parked } = status.value;
  switch (state) {
    case 'disconnected':
      return 'Not connected. Startup never connects hardware on its own — nothing moves because a machine rebooted.';
    case 'idle':
      return tracking
        ? 'Connected, tracking.'
        : 'Connected, tracking off — the sky is drifting past.';
    case 'slewing':
      return slewing ? 'Slewing.' : 'Slewing (reported without the flag set).';
    case 'parked':
      return parked ? 'Parked. Motion is refused until it is unparked.' : 'Parked.';
    case 'fault':
      // Not "until it is acknowledged": there is no acknowledgement, here or on the node, and a
      // sentence that invents one sends the operator looking for a button. What they need is that
      // this app has stopped commanding — and that the app stopping is not the mount stopping,
      // which is the one thing REL-02's `MOUNT_LINK_LOST` alert is raised to say.
      return (
        'Faulted. Motion commands are withheld until the mount says otherwise — and a mount ' +
        'that has stopped answering may still be moving.'
      );
  }
}
