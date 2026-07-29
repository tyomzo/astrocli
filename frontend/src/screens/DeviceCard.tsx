import type { ReactNode } from 'react';

import { isStandalone, promptInstall, useInstallState } from '../lib/pwa/installPrompt';
import { useWakeLock } from '../lib/pwa/useWakeLock';
import { Button } from '../ui/Button';
import { Card, Field } from '../ui/Card';

/**
 * The two Android capabilities SDD §5.9 spends EXT-06 on, surfaced so they can be *verified*.
 *
 * Both are otherwise invisible: an install prompt that never fires and a wake lock that was never
 * granted look exactly like ones that work, right up until the screen blanks during a slew. Neither
 * can be checked from a headless build, so the state is on screen where the person doing the
 * manual gate can read it off the device.
 */
export function DeviceCard(): ReactNode {
  const install = useInstallState();
  const wakeLock = useWakeLock();

  return (
    <Card title="Device">
      <dl>
        <Field label="display mode" value={isStandalone() ? 'standalone (installed)' : 'browser tab'} />
        <Field label="screen wake lock" value={WAKE_LOCK_WORDING[wakeLock]} />
        <Field label="install prompt" value={INSTALL_WORDING[install]} />
      </dl>

      {install === 'available' && (
        <Button
          className="mt-3"
          variant="primary"
          onClick={() => {
            void promptInstall();
          }}
        >
          Add to home screen
        </Button>
      )}
    </Card>
  );
}

const WAKE_LOCK_WORDING = {
  held: 'held — the screen will not sleep',
  released: 'released (app in the background)',
  denied: 'refused by the browser (battery saver?)',
  unsupported: 'not supported by this browser',
} as const;

const INSTALL_WORDING = {
  available: 'ready',
  installed: 'installed',
  // Chrome withholds `beforeinstallprompt` when the app is already installed, and also when the
  // manifest or service worker do not satisfy its installability rules — so this line is the
  // first place a broken manifest shows up.
  unavailable: 'not offered by this browser',
} as const;
