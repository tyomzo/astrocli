import { create } from 'zustand';

import { dispatch as dispatchNodes } from './nodes';
import { dispatch as dispatchTelemetry } from './telemetry';

/*
 * The operator's bearer token (SDD §4.5, SEC-02).
 *
 * `screens/ConnectPanel.tsx` is the UI over it (the connect panel SDD §5.9 lists first among the
 * Phase 1 screens). The node *address* the M0-T06 TODO expected this store to grow is not here
 * and will not be: the bundle is served by the field node, so the address is the origin the app
 * was loaded from — see the panel for the full reasoning.
 *
 * Changing the credential resets **both** event-fed stores, because everything either of them
 * holds was learned under the old one, and `lib/link/useEventLink.ts` then tears down the socket
 * and reconnects with a ticket minted from the new token.
 *
 * `localStorage`, not `sessionStorage` or a cookie. It has to survive the app being launched from
 * the home screen, killed by Android's memory manager and relaunched mid-session — the operator is
 * outdoors in the dark and re-typing a 32-byte base64 token is not a thing they can do. It is not
 * a cookie because nothing on this origin should be sent automatically: SDD §4.5's credential
 * belongs in an `Authorization` header the app puts there deliberately.
 *
 * This store is exempt from the "only observations write state" rule of `nodes.ts`, and the
 * distinction is worth stating so it does not get cited as precedent: the token is client-owned
 * configuration, not a mirror of anything the backbone knows. There is no event that could
 * confirm it, so there is nothing to be optimistic about.
 */

const STORAGE_KEY = 'astroctl.token';

interface TokenStore {
  token: string | null;
  set: (token: string) => void;
  clear: () => void;
}

function read(): string | null {
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    return stored !== null && stored.trim() !== '' ? stored : null;
  } catch {
    // Chrome throws on localStorage access when the operator has blocked site data. The app is
    // still usable against a loopback node running under the SDD §4.5 exception, so this
    // degrades to "no token" rather than failing to start.
    return null;
  }
}

function write(token: string | null): void {
  try {
    if (token === null) {
      window.localStorage.removeItem(STORAGE_KEY);
    } else {
      window.localStorage.setItem(STORAGE_KEY, token);
    }
  } catch {
    // As above: the token still works for this session, it just will not survive a relaunch.
  }
}

export const useTokenStore = create<TokenStore>()((set) => ({
  token: read(),
  set: (token) => {
    const trimmed = token.trim();
    write(trimmed === '' ? null : trimmed);
    // Everything already in either store was observed under the previous credential.
    reset();
    set({ token: trimmed === '' ? null : trimmed });
  },
  clear: () => {
    write(null);
    reset();
    set({ token: null });
  },
}));

function reset(): void {
  dispatchNodes({ type: 'session/reset' });
  dispatchTelemetry({ type: 'session/reset' });
}

export const currentToken = (): string | null => useTokenStore.getState().token;
