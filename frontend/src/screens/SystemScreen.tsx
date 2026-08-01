import { useEffect } from 'react';
import type { ReactNode } from 'react';

import { FIELD_HEALTH, STACK_HEALTH } from '../lib/api';
import { age } from '../lib/format';
import { probeAll } from '../lib/health';
import { useNow } from '../lib/useNow';
import { selectAlerts, useTelemetryStore } from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { useUiStore } from '../store/ui';
import { Button } from '../ui/Button';
import { Card } from '../ui/Card';
import { PanelGrid } from '../ui/PanelGrid';
import { BUILD, useUpdateWaiting } from '../lib/pwa/updateReady';
import { ConnectPanel } from './ConnectPanel';
import { DeviceCard } from './DeviceCard';
import { NodeCard } from './NodeCard';

/**
 * The detour behind the header status strip: credential, node health, device capabilities, and
 * the alert history.
 *
 * Not one of SDD §5.9's three destinations, on purpose — those are session concerns and this is
 * everything the operator needs when something is wrong with the machinery underneath one. It is
 * reached by tapping the badges, which is where they were already looking.
 *
 * **There is still no poller.** §5.9 says the store is fed by the event stream and the connect
 * snapshot, "no REST polling". The two health probes here are one-shot observations at the three
 * moments that are not polling: opening this screen, a change of credential, and the tab becoming
 * visible again. Live telemetry comes from the socket; these two cards exist because
 * `/api/system/health` reports things the event schema does not carry — TLS expiry (SEC-07) and
 * the stack node's reachability through the proxy.
 */
export function SystemScreen(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const closeSystem = useUiStore((state) => state.closeSystem);

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
        <h1 className="text-lg font-semibold">System</h1>
        <div className="flex gap-2">
          <Button
            onClick={() => {
              void probeAll(token);
            }}
          >
            Refresh
          </Button>
          <Button onClick={closeSystem}>Back</Button>
        </div>
      </div>

      <ConnectPanel />

      <PanelGrid>
        <NodeCard node="field" title="Field node" path={FIELD_HEALTH} />
        <NodeCard node="stack" title="Stacking server (via field proxy)" path={STACK_HEALTH} />
      </PanelGrid>

      <PanelGrid>
        <DeviceCard />
        <AlertHistory />
      </PanelGrid>

      <BuildLine />
    </div>
  );
}

/**
 * Which bundle is actually running, and whether a newer one is waiting.
 *
 * Small and permanent rather than a toast: the operator's question — "am I on the version I was
 * promised?" — recurs every deploy, and a dismissable notice answers it exactly once.
 */
function BuildLine(): ReactNode {
  const waiting = useUpdateWaiting();
  return (
    <p className="text-center font-mono text-xs text-faint">
      build {BUILD}
      {waiting && (
        <span className="ml-2 text-warn">
          — an update is ready: close the app fully, then open it again
        </span>
      )}
    </p>
  );
}

/**
 * Every alert this connection has seen, newest first.
 *
 * Bounded by the store (§4.3 gives `alert` no cadence bound, so a fault loop must not become
 * unbounded memory on a phone). It is here rather than on an operating screen because a history
 * is what you consult after the fact; `AlertStrip` is what interrupts you during.
 */
function AlertHistory(): ReactNode {
  const alerts = useTelemetryStore(selectAlerts);
  const now = useNow(5000);

  return (
    <Card title="Alerts">
      {alerts.length === 0 ? (
        <p className="text-sm text-muted">No alerts on this connection.</p>
      ) : (
        <ul className="flex flex-col gap-2">
          {alerts.map((alert) => (
            <li key={alert.seq} className="text-sm">
              <span className="font-mono text-muted">{alert.value.code}</span>{' '}
              <span className="text-faint">({alert.value.severity})</span> {alert.value.message}{' '}
              <span className="text-faint">{age(alert.at, now)}</span>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}
