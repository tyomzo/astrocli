import { useState } from 'react';
import type { ReactNode } from 'react';

import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { Card } from '../ui/Card';

/**
 * Bearer-token entry — SEC-02, and a placeholder for the connect panel of SDD §5.9.
 *
 * TODO(M1-T04): the connect panel replaces this. It owns the node address as well as the token,
 * diagnoses a 401 from `/api/auth/ws-ticket` as "bad credential" rather than as a connection
 * failure, and is reached before any screen rather than sitting on one. See `store/token.ts`.
 *
 * The token is never rendered back. Not because it is secret from the person holding the phone,
 * but because "the field shows a token, so the token must be right" is a comfortable and wrong
 * conclusion — the node is the only thing that can say whether it is right, and it says so in the
 * cards above.
 */
export function TokenCard(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const setToken = useTokenStore((state) => state.set);
  const clearToken = useTokenStore((state) => state.clear);
  const [draft, setDraft] = useState('');

  return (
    <Card title="Credential">
      <p className="mb-3 text-sm text-muted">
        {token === null ? (
          <>
            No token stored. A node with <code className="font-mono">server.auth_token_env</code>{' '}
            set will answer <span className="font-mono">401</span> until one is entered; a node on
            loopback without one serves unauthenticated (SDD §4.5).
          </>
        ) : (
          <>Token stored in this browser. It is sent as a bearer header to the field node only.</>
        )}
      </p>

      <form
        className="flex flex-wrap items-center gap-2"
        onSubmit={(event) => {
          event.preventDefault();
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
    </Card>
  );
}
