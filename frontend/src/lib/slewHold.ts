import type { RequestFailure } from './api';
import type { SlewAxis, SlewDirection, SlewSpeed } from './commands';
import { SLEW_RENEW_MS, mountSlew, mountSlewStop } from './commands';

/**
 * Press-and-hold slew: the client half of SDD §5.8.1's dead-man's switch.
 *
 * One `/api/mount/slew` call authorises motion for `ttl_ms` and no longer. Holding the D-pad
 * re-sends the *identical* request every `SLEW_RENEW_MS`; the server treats a repeat with the
 * same parameters as a deadline extension rather than a new motor command. Release sends
 * `/api/mount/slew/stop`, which is the primary stop path — TTL expiry is the backstop for the
 * cases where release never happens: a dropped VPN packet, a touch event the browser swallowed, a
 * tab that crashed with a finger down.
 *
 * Two properties are load-bearing and easy to lose in a refactor:
 *
 *  * **Release always sends a stop**, including when the renewal loop has already failed and even
 *    when the hold is being torn down by the component unmounting. There is no path out of this
 *    object that does not attempt it.
 *  * **A failed renewal stops the loop.** If the node answers 403 `LIMIT_ALTITUDE` mid-hold, the
 *    axis is being stopped by the safety layer and continuing to ask is noise on a slow link at
 *    the exact moment it is worst. The failure is handed to the caller to explain.
 */
export interface SlewHold {
  /** Idempotent: calling twice sends one stop. */
  release(): void;
}

export function beginSlewHold(options: {
  token: string | null;
  axis: SlewAxis;
  direction: SlewDirection;
  speed: SlewSpeed;
  /** Called when the node refuses a lease — the caller renders it; this module never does. */
  onRefused: (failure: RequestFailure) => void;
}): SlewHold {
  const { token, axis, direction, speed, onRefused } = options;
  let released = false;
  let timer: number | null = null;

  const renew = async (): Promise<void> => {
    const result = await mountSlew(token, { axis, direction, speed });
    if (released) return;
    if (!result.ok) {
      stopRenewing();
      onRefused(result.failure);
      // Still send the stop: the lease may have been granted and then refused on renewal, and a
      // stop that was not needed costs one request.
      void mountSlewStop(token, axis);
      released = true;
    }
  };

  const stopRenewing = (): void => {
    if (timer !== null) {
      window.clearInterval(timer);
      timer = null;
    }
  };

  void renew();
  timer = window.setInterval(() => void renew(), SLEW_RENEW_MS);

  return {
    release() {
      if (released) return;
      released = true;
      stopRenewing();
      void mountSlewStop(token, axis);
    },
  };
}
