import { useState } from 'react';
import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import type { TrackingMode } from '../lib/commands';
import { mountTracking } from '../lib/commands';
import { selectLink, selectMountStatus, useTelemetryStore } from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { Card } from '../ui/Card';
import { FailureNote } from '../ui/FailureNote';

/**
 * Tracking rate — SDD §5.8.1's `/api/mount/tracking`.
 *
 * # The selection shown is the mount's, never the button that was pressed
 *
 * `mount.status` carries a single `tracking` boolean, not the rate, so this control can show
 * *whether* the mount is tracking but not which rate it settled on. Rather than remember the last
 * button pressed and present it as the current mode — which would be optimistic UI wearing a
 * different hat, and would be wrong after any reconnect — the panel highlights on/off from the
 * event and leaves the rate buttons unhighlighted.
 *
 * This is a real gap in the §4.3 payload and it is worth naming: `mount.status` would need a
 * `tracking_mode` field for the UI to reflect the rate honestly. It is listed in the M1-T04
 * result as something for M1-T03 to consider rather than patched around here, because inventing
 * a field the node does not send is how a frontend ends up with a schema of its own.
 */
const MODES: readonly { id: TrackingMode; label: string }[] = [
  { id: 'sidereal', label: 'Sidereal' },
  { id: 'lunar', label: 'Lunar' },
  { id: 'solar', label: 'Solar' },
  { id: 'off', label: 'Off' },
];

export function TrackingControl(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const status = useTelemetryStore(selectMountStatus);
  const link = useTelemetryStore(selectLink);
  const [failure, setFailure] = useState<RequestFailure | null>(null);

  const tracking = status.state === 'observed' && status.value.tracking;
  const usable =
    link.phase === 'live' &&
    status.state === 'observed' &&
    status.value.state !== 'disconnected';

  return (
    <Card
      title="Tracking"
      accessory={
        <span className="text-sm text-muted">
          <span aria-hidden="true" className={tracking ? 'text-ok' : 'text-faint'}>
            {tracking ? '●' : '○'}
          </span>{' '}
          {status.state === 'unknown' ? 'unknown' : tracking ? 'tracking' : 'not tracking'}
        </span>
      }
    >
      <div className="grid grid-cols-2 gap-2">
        {MODES.map((mode) => (
          <Button
            key={mode.id}
            disabled={!usable}
            onClick={() => {
              setFailure(null);
              void mountTracking(token, mode.id).then((result) => {
                if (!result.ok) setFailure(result.failure);
              });
            }}
          >
            {mode.label}
          </Button>
        ))}
      </div>

      {!usable && (
        <p className="mt-2 text-sm text-muted">
          {link.phase === 'live'
            ? 'The mount is not connected.'
            : 'The event link is down; a rate change could not be confirmed.'}
        </p>
      )}

      {failure !== null && <FailureNote failure={failure} action="tracking" />}
    </Card>
  );
}
