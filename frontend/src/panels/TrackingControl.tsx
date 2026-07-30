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
 * The highlighted rate comes from `mount.status.tracking_mode`, so it is what the *mount* is
 * doing. Nothing here remembers which button was pressed: that would be optimistic UI wearing a
 * different hat, and it would be wrong in exactly the cases an operator checks this panel for —
 * a driver that refused `lunar` as unsupported, a goto that suspended tracking and resumed it at
 * the previous rate, or any reconnect at all.
 *
 * M1-T04 shipped this control against a payload that carried only `tracking: boolean` and
 * therefore left every rate button unhighlighted, with a comment asking M1-T03 for the field
 * rather than inventing one here. M1-T03 added it (SDD §4.3 change note); this is that comment
 * being paid off, and the discipline that produced it — a frontend that does not invent a schema
 * of its own — is the reason it could be.
 *
 * `tracking_mode` is `null` on a node too old to send it, which renders exactly like tracking
 * being off. The `tracking` boolean beside it is what tells the two apart, and the accessory
 * still reads from that.
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
  // The rate the mount reports, or `null`. `off` is the selected "rate" when the drive is
  // stopped, so the operator can see that off is a state the mount is in rather than the absence
  // of one.
  const rate: TrackingMode | null =
    status.state === 'observed' ? (status.value.tracking_mode ?? (tracking ? null : 'off')) : null;
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
        {MODES.map((mode) => {
          const selected = rate === mode.id;
          return (
            <Button
              key={mode.id}
              disabled={!usable}
              // `aria-pressed` rather than colour alone: the token layer collapses hues toward
              // red in night mode (USB-02), so a highlight that is only a colour is a highlight
              // two operators see differently. The glyph carries it for everyone else.
              aria-pressed={selected}
              className={selected ? 'border-ok text-ok' : undefined}
              onClick={() => {
                setFailure(null);
                void mountTracking(token, mode.id).then((result) => {
                  if (!result.ok) setFailure(result.failure);
                });
              }}
            >
              <span aria-hidden="true">{selected ? '● ' : '○ '}</span>
              {mode.label}
            </Button>
          );
        })}
      </div>

      {!usable && (
        <p className="mt-2 text-sm text-muted">
          {link.phase === 'live'
            ? 'The mount is not connected.'
            : "Not connected to the mount — a rate change can't be confirmed."}
        </p>
      )}

      {failure !== null && <FailureNote failure={failure} action="tracking" />}
    </Card>
  );
}
