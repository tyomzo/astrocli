import { useState } from 'react';
import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';
import { mountConnect, mountDisconnect } from '../lib/commands';
import { selectMountStatus, useTelemetryStore } from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { FailureNote } from '../ui/FailureNote';

/*
 * Connect/disconnect for the mount, at the head of the Target card.
 *
 * It lived on the Pointing card first, and the operator called that placement misleading —
 * rightly: pointing is telemetry, a report of where the mount is, and gluing the connection
 * switch to it implied connecting is something you do *to the readout*. Connecting is the first
 * act of a session, and the session starts by choosing a target, so the switch sits where the
 * workflow begins. (The dead-reckoned coordinates, the state line and everything else on the
 * Pointing card remain pure observation, which is tidier too.)
 */
export function MountConnect(): ReactNode {
  const status = useTelemetryStore(selectMountStatus);
  const token = useTokenStore((state) => state.token);
  const [failure, setFailure] = useState<RequestFailure | null>(null);

  const connected = status.state === 'observed' && status.value.state !== 'disconnected';

  return (
    <div className="flex flex-col items-end gap-1">
      <Button
        onClick={() => {
          setFailure(null);
          const command = connected ? mountDisconnect : mountConnect;
          void command(token).then((result) => {
            // Only the failure is kept. Success changes nothing here on purpose: the UI moves
            // when `mount.status` arrives, the only report that the mount itself agrees.
            if (!result.ok) setFailure(result.failure);
          });
        }}
      >
        {connected ? 'Disconnect' : 'Connect'}
      </Button>
      {failure !== null && (
        <FailureNote failure={failure} action={connected ? 'disconnect' : 'connect'} />
      )}
    </div>
  );
}
