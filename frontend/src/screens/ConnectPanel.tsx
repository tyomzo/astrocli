import { useState } from 'react';
import type { ReactNode } from 'react';

import type { LinkPhase } from '../store/telemetry';
import { selectLink, useTelemetryStore } from '../store/telemetry';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { Card } from '../ui/Card';

/**
 * The connect panel — SDD §5.9's first Phase 1 screen. Replaces M0-T06's `TokenCard`.
 *
 * # Why there is no address field
 *
 * `store/token.ts` and `TokenCard` both carried a `TODO(M1-T04)` saying the connect panel would
 * own "the node's address as well as the token". It does not, and the reason is structural rather
 * than an omission: this bundle is served by the field node itself (ARC-02, `include_dir!`), so
 * the node's address is the origin the app was loaded from. `lib/api.ts` refuses an absolute URL
 * at runtime to keep it that way (ADR-07 — the stack node is reached through `/stack/*`, because
 * the operator's phone may have no route to it at all). Reaching a different node means opening a
 * different URL, which is a browser action, not a field in a form.
 *
 * # Diagnosis, not a spinner
 *
 * The credential's state is only knowable by asking the node, so this panel reports what the link
 * has actually learned. The distinction §5.9 insists on is between "the token is wrong" — a 401
 * from `/api/auth/ws-ticket`, which no amount of retrying fixes — and "nothing answered", which
 * retrying fixes on its own. They send the operator to different places: one to their notes, the
 * other to the VPN.
 *
 * The token is never rendered back. Not because it is secret from the person holding the phone,
 * but because "the field shows a token, so the token must be right" is a comfortable and wrong
 * conclusion — the node is the only thing that can say whether it is right, and it says so here.
 */
export function ConnectPanel(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const setToken = useTokenStore((state) => state.set);
  const clearToken = useTokenStore((state) => state.clear);
  const link = useTelemetryStore(selectLink);
  const [draft, setDraft] = useState('');

  return (
    <Card title="Connection">
      <p className="mb-3 text-sm text-muted">{diagnose(link, token !== null)}</p>

      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          // Storing resets both stores and restarts the link with a ticket minted from the new
          // credential — see `store/token.ts` and `lib/link/useEventLink.ts`.
          setToken(draft);
          setDraft('');
        }}
      >
        <label className="sr-only" htmlFor="token">
          Bearer token
        </label>
        <input
          id="token"
          name="token"
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={draft}
          onChange={(event) => setDraft(event.target.value)}
          placeholder="bearer token"
          className="min-h-touch min-w-0 flex-1 rounded-md border border-edge-strong bg-surface px-3 font-mono text-sm text-fg placeholder:text-faint"
        />
        <Button type="submit" disabled={draft.trim() === ''}>
          Store
        </Button>
        {token !== null && (
          <Button
            onClick={() => {
              clearToken();
              setDraft('');
            }}
          >
            Forget
          </Button>
        )}
      </form>

      <p className="mt-3 text-sm text-faint">
        Sent as a bearer header to this origin only, and stored in this browser so an app killed by
        Android and relaunched mid-session does not need it typed again in the dark. It never
        appears in the socket URL: SDD §4.5&rsquo;s single-use ticket exists so the long-lived
        credential stays out of the node&rsquo;s access log.
      </p>
    </Card>
  );
}

function diagnose(link: LinkPhase, stored: boolean): string {
  switch (link.phase) {
    case 'unauthorized':
      return stored
        ? `The node refused this token when asking for a WebSocket ticket: ${link.message}. Nothing is being retried — enter the right token.`
        : `The node requires a bearer token: ${link.message}.`;
    case 'live':
      return stored
        ? 'Connected. The stored token is accepted and the event stream is live.'
        : 'Connected without a token — this node is serving under the loopback exception (SDD §4.5).';
    case 'retrying':
      return 'The node is not answering. The credential may well be fine; the app is retrying on its own.';
    case 'idle':
      return stored ? 'A token is stored. Not connected.' : 'No token stored. Not connected.';
    case 'authorizing':
    case 'connecting':
    case 'syncing':
      return 'Connecting…';
  }
}
