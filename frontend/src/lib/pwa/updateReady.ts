import { useEffect, useState } from 'react';

// Injected by vite.config.ts's `define` at build time.
declare const __ASTROCTL_BUILD__: string;

/** The identity of the bundle that is actually running — sha · build time. */
export const BUILD = typeof __ASTROCTL_BUILD__ === 'string' ? __ASTROCTL_BUILD__ : 'dev';

/**
 * True when a newer bundle is installed and waiting for the next cold start.
 *
 * The service worker deliberately never swaps code mid-session (its own docs say why: changing
 * the JavaScript under a running capture is self-inflicted breakage). The cost is that "close
 * the app and reopen" becomes an instruction the operator has to *receive* — and until this
 * hook, nothing delivered it. An operator who cannot tell the stale copy from the deploy they
 * were promised is blind in exactly the way a field tool must not make them.
 */
export function useUpdateWaiting(): boolean {
  const [waiting, setWaiting] = useState(false);

  useEffect(() => {
    if (!('serviceWorker' in navigator)) return;
    let live = true;
    void navigator.serviceWorker.getRegistration().then((registration) => {
      if (!registration || !live) return;
      const check = () => {
        if (live) setWaiting(registration.waiting !== null);
      };
      check();
      registration.addEventListener('updatefound', () => {
        const incoming = registration.installing;
        incoming?.addEventListener('statechange', check);
      });
    });
    return () => {
      live = false;
    };
  }, []);

  return waiting;
}
