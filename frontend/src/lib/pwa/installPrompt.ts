import { useSyncExternalStore } from 'react';

/*
 * `beforeinstallprompt` — USB-09, and one of the three things targeting Android buys (SDD §5.9).
 *
 * Chrome fires this event once, early, and only if the page is installable. It fires before React
 * has mounted more often than not, so the listener is installed at module scope and the event is
 * stashed; a hook that registers the listener in `useEffect` misses it and the install button
 * never appears. That failure is silent and intermittent, which is why this is not a hook.
 *
 * iOS has no equivalent and is not a target (EXT-06 is advisory, PRD §11 change note 1.15.0).
 * There is deliberately no share-sheet instruction fallback.
 */

interface BeforeInstallPromptEvent extends Event {
  prompt: () => Promise<void>;
  readonly userChoice: Promise<{ outcome: 'accepted' | 'dismissed' }>;
}

export type InstallState =
  /** Chrome has not offered an install prompt: already installed, or not installable. */
  | 'unavailable'
  /** An install prompt is held and can be shown. */
  | 'available'
  /** The operator accepted; the app is on the home screen. */
  | 'installed';

let deferred: BeforeInstallPromptEvent | null = null;
let state: InstallState = 'unavailable';
const listeners = new Set<() => void>();

function publish(next: InstallState): void {
  state = next;
  for (const listener of listeners) {
    listener();
  }
}

if (typeof window !== 'undefined') {
  window.addEventListener('beforeinstallprompt', (event) => {
    // Suppressing Chrome's own mini-infobar is what makes this a real install flow rather than a
    // browser affordance the operator has to notice (USB-09).
    event.preventDefault();
    deferred = event as BeforeInstallPromptEvent;
    publish('available');
  });

  window.addEventListener('appinstalled', () => {
    deferred = null;
    publish('installed');
  });
}

export function useInstallState(): InstallState {
  return useSyncExternalStore(
    (onChange) => {
      listeners.add(onChange);
      return () => listeners.delete(onChange);
    },
    () => state,
    // Third argument so the component tree can be rendered outside a browser at all — React
    // throws without it. Nothing here is server-rendered, but "render the app and assert on the
    // output" is how this tree gets checked without a device, and one argument is a cheap price
    // for not having a component that only exists inside Chrome.
    () => 'unavailable' as InstallState,
  );
}

/** Show the held prompt. Returns what the operator chose, or `null` if there was none to show. */
export async function promptInstall(): Promise<'accepted' | 'dismissed' | null> {
  const event = deferred;
  if (event === null) {
    return null;
  }
  // The event is single-use: Chrome will not accept a second `prompt()` on the same one.
  deferred = null;
  await event.prompt();
  const { outcome } = await event.userChoice;
  publish(outcome === 'accepted' ? 'installed' : 'unavailable');
  return outcome;
}

/** Whether the app is running from the home screen rather than in a browser tab. */
export function isStandalone(): boolean {
  return window.matchMedia('(display-mode: standalone)').matches;
}
