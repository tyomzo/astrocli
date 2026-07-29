import { useEffect, useState } from 'react';

/*
 * Screen Wake Lock — SDD §5.9, and the reason EXT-06 was made advisory (PRD §11 change note
 * 1.15.0).
 *
 * The display must not sleep while the operator is watching live view. Their hands are on the
 * mount, not the phone, so nothing generates the touch events that would otherwise keep the
 * screen awake, and a screen that blanks mid-slew is a screen they have to unlock in the dark.
 *
 * Two facts about the API drive the shape of this hook:
 *
 *  * The browser releases the lock itself whenever the document stops being visible, and does not
 *    restore it. Re-acquiring on `visibilitychange` is not an optimisation, it is the difference
 *    between the lock working once and working for a session.
 *  * `request()` rejects — it does not return null — when the tab is hidden or the device is in
 *    battery saver. That is a normal outcome, not an error to log loudly.
 */

export type WakeLockState =
  /**
   * No `navigator.wakeLock`.
   *
   * In practice this is almost never an old browser — it is an **insecure origin**, which is why
   * `DeviceCard` checks `isSecureContext` before it renders this state at all (`secureContext.ts`).
   */
  | 'unsupported'
  /** Held: the screen will not sleep. */
  | 'held'
  /** Released because the document went to the background — expected, and re-acquired on return. */
  | 'released'
  /** The browser refused, e.g. battery saver. The operator should be told rather than left to
      discover it when the screen blanks. */
  | 'denied';

export function useWakeLock(): WakeLockState {
  const [state, setState] = useState<WakeLockState>(() =>
    'wakeLock' in navigator ? 'released' : 'unsupported',
  );

  useEffect(() => {
    if (!('wakeLock' in navigator)) {
      return;
    }

    let sentinel: WakeLockSentinel | null = null;
    let disposed = false;

    const acquire = async (): Promise<void> => {
      if (disposed || sentinel !== null || document.visibilityState !== 'visible') {
        return;
      }
      try {
        const lock = await navigator.wakeLock.request('screen');
        if (disposed) {
          void lock.release();
          return;
        }
        sentinel = lock;
        setState('held');
        lock.addEventListener('release', () => {
          sentinel = null;
          if (!disposed) {
            setState('released');
          }
        });
      } catch {
        setState('denied');
      }
    };

    const onVisibility = (): void => {
      if (document.visibilityState === 'visible') {
        void acquire();
      }
    };

    void acquire();
    document.addEventListener('visibilitychange', onVisibility);

    return () => {
      disposed = true;
      document.removeEventListener('visibilitychange', onVisibility);
      void sentinel?.release();
    };
  }, []);

  return state;
}
