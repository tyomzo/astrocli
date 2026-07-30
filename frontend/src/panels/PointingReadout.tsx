import { useState } from 'react';
import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import { mountConnect, mountDisconnect } from '../lib/commands';
import { formatDecDegrees, formatDegrees, formatRaHours } from '../lib/coords';
import { age } from '../lib/format';
import { useNow } from '../lib/useNow';
import type { MountStatus } from '../lib/link/protocol';
import type { Slot } from '../store/telemetry';
import {
  selectLink,
  selectMountPosition,
  selectMountStatus,
  useTelemetryStore,
} from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { Card, Field } from '../ui/Card';
import { FailureNote } from '../ui/FailureNote';

/**
 * Where the telescope is pointing, in astronomical notation — USB-05, SDD §5.9's `now …` block.
 *
 * # Freshness is rendered next to the value, always
 *
 * `mount.position` is 1 Hz, so a coordinate that is four seconds old means something went wrong
 * with the link, not with the mount. §8.3 requires the operator to know how stale their picture is
 * *before* issuing a command, so age is part of this readout rather than a diagnostic elsewhere,
 * and the whole block dims when the link is not live.
 *
 * M1-T15 replaces the dimming with the full confirmed/predicted/stale treatment and dead-reckons
 * the displayed RA between updates. This is the honest floor under that: nothing here is
 * extrapolated, so nothing here can be wrong in a way the operator cannot see.
 *
 * # alt/az may be null and are not zero
 *
 * M1-T03 emits `null` for both until M1-T05's topocentric transform lands. A missing altitude
 * rendered as `0.0°` would put the target on the horizon, which is exactly where the altitude
 * limit lives — so the formatter prints an em dash and the operator knows the number is absent
 * rather than alarming.
 */
export function PointingReadout(): ReactNode {
  const position = useTelemetryStore(selectMountPosition);
  const status = useTelemetryStore(selectMountStatus);
  const link = useTelemetryStore(selectLink);
  const token = useTokenStore((state) => state.token);
  const now = useNow();
  const [failure, setFailure] = useState<RequestFailure | null>(null);

  const live = link.phase === 'live';
  const connected = status.state === 'observed' && status.value.state !== 'disconnected';

  return (
    <Card
      title="Pointing"
      accessory={
        <Button
          onClick={() => {
            setFailure(null);
            const command = connected ? mountDisconnect : mountConnect;
            void command(token).then((result) => {
              // Only the failure is kept. The success case changes nothing here on purpose: the
              // panel moves when `mount.status` arrives, which is the only report that the mount
              // itself agrees (SDD §5.9).
              if (!result.ok) setFailure(result.failure);
            });
          }}
        >
          {connected ? 'Disconnect' : 'Connect'}
        </Button>
      }
    >
      {position.state === 'unknown' ? (
        <p className="text-sm text-muted">
          {live
            ? 'The mount is not reporting a position. Connect it to start the 1 Hz stream.'
            : 'No position has been received on this connection.'}
        </p>
      ) : (
        <dl className={live ? '' : 'opacity-60'}>
          <Field label="RA" value={formatRaHours(position.value.ra)} />
          <Field label="DEC" value={formatDecDegrees(position.value.dec)} />
          <Field label="alt" value={formatDegrees(position.value.alt)} />
          <Field label="az" value={formatDegrees(position.value.az)} />
          <Field label="pier" value={position.value.pier_side} />
          <Field
            label={live ? 'observed' : 'last seen'}
            value={
              <span className={live ? 'text-muted' : 'text-warn'}>
                {age(position.at, now)}
                {live ? '' : ' — link down'}
              </span>
            }
          />
        </dl>
      )}

      <p className="mt-2 text-sm text-muted">{describeMount(status)}</p>

      {failure !== null && (
        <FailureNote failure={failure} action={connected ? 'disconnect' : 'connect'} />
      )}
    </Card>
  );
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
      return 'Faulted. Motion is refused until the fault is acknowledged.';
  }
}
