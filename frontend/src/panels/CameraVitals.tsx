import type { ReactNode } from 'react';

import { selectCameraStatus, selectLink, useTelemetryStore } from '../store/telemetry';
import { Card, Field } from '../ui/Card';

/**
 * Battery and card space — SDD §4.3's `camera.status`, the two fields §5.8.1 also serves as routes.
 *
 * From the event stream rather than from `/api/camera/battery`: the node publishes the same numbers
 * on change and every 60 s, so a panel that fetched them would be a poll producing a reading that
 * is up to a minute stale *and* a request the operator's tunnel has to carry. The routes exist for
 * a caller that has no socket.
 *
 * # `null` is a value here, not a missing one
 *
 * A disconnected camera reports `battery_pct: null`, and §4.3 is explicit about why it is not `0`:
 * a zeroed battery renders as an empty gauge, which is the one reading an operator would act on by
 * ending the session. So an unknown charge says "unknown".
 *
 * # This is the camera's card, not the node's disk
 *
 * They answer different questions and only the second one governs whether capture may continue
 * (REL-12, `system.health.disk_free_gb`). With the reference body shooting to internal RAM the card
 * never fills at all, so labelling this "space left" would be reassuring and wrong — hence
 * "camera card", which is what it is.
 */
export function CameraVitals(): ReactNode {
  const camera = useTelemetryStore(selectCameraStatus);
  const link = useTelemetryStore(selectLink);

  return (
    <Card
      title="Camera"
      accessory={
        <span className="text-sm text-muted">
          <span
            aria-hidden="true"
            className={
              camera.state === 'observed' && camera.value.connected ? 'text-ok' : 'text-faint'
            }
          >
            {camera.state === 'observed' && camera.value.connected ? '●' : '○'}
          </span>{' '}
          {camera.state === 'unknown'
            ? 'unknown'
            : camera.value.connected
              ? 'connected'
              : 'not connected'}
        </span>
      }
    >
      {camera.state === 'unknown' ? (
        <p className="text-sm text-muted">
          {link.phase === 'live'
            ? 'The telescope has not reported the camera yet.'
            : 'No camera reading on this connection.'}
        </p>
      ) : (
        <dl className={link.phase === 'live' ? '' : 'opacity-60'}>
          <Field
            label="battery"
            value={
              camera.value.battery_pct === null ? 'unknown' : `${camera.value.battery_pct}%`
            }
          />
          <Field label="power" value={camera.value.charging ? 'external' : 'battery'} />
          <Field
            label="camera card"
            value={
              camera.value.storage_free_mb === null
                ? 'unknown'
                : `${(camera.value.storage_free_mb / 1000).toFixed(1)} GB free`
            }
          />
        </dl>
      )}
    </Card>
  );
}
