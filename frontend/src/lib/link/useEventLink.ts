import { useEffect } from 'react';

import { useTokenStore } from '../../store/token';
import { browserEnvironment, createLink } from './connection';

/**
 * Owns the `/ws` connection for the life of the app — SDD §5.9, REL-10.
 *
 * Mounted once, in `App`. The effect is keyed on the token because a credential change is not a
 * reconfiguration of the existing link, it is a different session: the old socket was authorised
 * by a ticket minted from the old token, and `store/token.ts` has already reset the stores. Tearing
 * down and rebuilding is therefore the correct handling and not a heavy-handed one.
 *
 * React StrictMode runs this effect twice in development, so the app connects, disconnects and
 * reconnects on first load. That is not a defect to work around: it is the same teardown path a
 * credential change and a hot reload take, and if it did not survive being run twice it would not
 * survive the field either.
 */
export function useEventLink(): void {
  const token = useTokenStore((state) => state.token);

  useEffect(() => {
    const link = createLink(browserEnvironment(() => token));
    link.start();

    // A phone that slept comes back with a socket the OS killed without closing. Ask immediately
    // rather than waiting out the silence watchdog — the operator is looking at the screen now.
    const onVisibility = (): void => {
      if (document.visibilityState === 'visible') link.checkNow();
    };
    document.addEventListener('visibilitychange', onVisibility);

    return () => {
      document.removeEventListener('visibilitychange', onVisibility);
      link.stop();
    };
  }, [token]);
}
