import type { ReactNode } from 'react';

import { AppShell } from './app/AppShell';
import { HealthScreen } from './screens/HealthScreen';

/**
 * M0 has one screen, so there is no router. M1-T04 introduces the three phone destinations of
 * SDD §5.9 (Target / Frame / Stack) and picks a routing approach then, with three real
 * destinations to choose it against rather than one hypothetical one.
 */
export function App(): ReactNode {
  return (
    <AppShell>
      <HealthScreen />
    </AppShell>
  );
}
