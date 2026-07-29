import { useEffect } from 'react';
import type { ReactNode } from 'react';

import { FIELD_HEALTH, STACK_HEALTH } from '../lib/api';
import { probeAll } from '../lib/health';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { PanelGrid } from '../ui/PanelGrid';
import { DeviceCard } from './DeviceCard';
import { NodeCard } from './NodeCard';
import { TokenCard } from './TokenCard';

/**
 * The only screen in M0: both nodes' health, the credential that reaches them, and the two device
 * capabilities that need a human to verify.
 *
 * **There is no poller**, and that is a decision rather than an omission. SDD §5.9 says the store
 * is fed by the event stream and the connect snapshot, "no REST polling" — a 5-second `setInterval`
 * here would be the exact pattern M1-T04 has to delete, and it would be load-bearing by then.
 * Observations are taken at the three moments that are not polling: first render, a change of
 * credential, and the tab becoming visible again.
 */
export function HealthScreen(): ReactNode {
  const token = useTokenStore((state) => state.token);

  useEffect(() => {
    void probeAll(token);

    const onVisible = (): void => {
      if (document.visibilityState === 'visible') {
        void probeAll(token);
      }
    };
    document.addEventListener('visibilitychange', onVisible);
    return () => document.removeEventListener('visibilitychange', onVisible);
  }, [token]);

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-3">
        <h1 className="text-lg font-semibold">Node health</h1>
        <Button
          onClick={() => {
            void probeAll(token);
          }}
        >
          Refresh
        </Button>
      </div>

      <PanelGrid>
        <NodeCard node="field" title="Field node" path={FIELD_HEALTH} />
        <NodeCard node="stack" title="Stacking server (via field proxy)" path={STACK_HEALTH} />
      </PanelGrid>

      <PanelGrid>
        <TokenCard />
        <DeviceCard />
      </PanelGrid>
    </div>
  );
}
