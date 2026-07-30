import { useEffect } from 'react';
import type { ReactNode } from 'react';

import { AppShell } from './app/AppShell';
import { useEventLink } from './lib/link/useEventLink';
import { OperatingView } from './screens/OperatingView';
import { SystemScreen } from './screens/SystemScreen';
import { selectLink, useTelemetryStore } from './store/telemetry';
import { useUiStore } from './store/ui';

/**
 * The app: one socket, three destinations, and a system detour.
 *
 * There is still no router. The three destinations of SDD §5.9 are session state rather than
 * addresses — the operator moves between them dozens of times a session, on a device with a back
 * gesture that would otherwise unwind that history one step at a time — and a URL for each buys
 * deep-linking that nobody standing in a field has any use for. It also costs bytes on a load
 * budget USB-10 makes functional. `store/ui.ts` holds the current destination; when there is a
 * reason to address a screen (a shared session view, Phase 3), that is when a router earns its
 * place.
 *
 * `useEventLink` is mounted here and only here: the socket is the app's, not a screen's, so that
 * switching destinations never costs a reconnect — which on this link is a ticket round trip plus
 * a snapshot.
 */
export function App(): ReactNode {
  useEventLink();

  const systemOpen = useUiStore((state) => state.systemOpen);
  const openSystem = useUiStore((state) => state.openSystem);
  const link = useTelemetryStore(selectLink);

  useEffect(() => {
    // A refused credential is the one failure the operator cannot wait out (SDD §5.9: say so
    // rather than retrying forever), so the app brings them to where it is fixed. Keyed on the
    // phase, so leaving the screen with the token still wrong does not drag them back.
    if (link.phase === 'unauthorized') openSystem();
  }, [link.phase, openSystem]);

  return <AppShell>{systemOpen ? <SystemScreen /> : <OperatingView />}</AppShell>;
}
