import type { ReactNode } from 'react';

import { useNow } from '../lib/useNow';
import type { LinkPhase } from '../store/telemetry';
import { selectLink, useTelemetryStore } from '../store/telemetry';
import { useUiStore } from '../store/ui';
import { Button } from '../ui/Button';

/**
 * What the link is doing, whenever that is not "working" — SDD §5.9, §8.3(8), REL-10.
 *
 * Silent when live: a banner that is always there is furniture, and the header badge already
 * carries the steady state.
 *
 * # A bad token is a dead end, and says so
 *
 * §5.9: "If the ticket request itself fails with 401 the token is bad and the UI must say so
 * rather than retrying forever." The `unauthorized` phase is terminal in the store for that
 * reason, so this banner is the only thing that can move the operator forward — hence the button
 * straight to the credential. A spinner here would be the failure the requirement names.
 *
 * # Reconnection is narrated, not hidden
 *
 * The countdown and the attempt number are shown because the alternative is an operator watching
 * a frozen panel deciding whether to reload the app in the dark, mid-session. The reason the last
 * attempt failed is shown for the same reason: "the node refused the ticket" and "nothing
 * answered" send them to different places.
 */
export function LinkBanner(): ReactNode {
  const link = useTelemetryStore(selectLink);
  const openSystem = useUiStore((state) => state.openSystem);
  // Only tick while something is counting down; a live link re-renders nothing here.
  const now = useNow(link.phase === 'retrying' ? 500 : 5000);

  if (link.phase === 'live') return null;

  if (link.phase === 'unauthorized') {
    return (
      <Note tone="danger" glyph="⊘">
        <span>
          The field node refused the credential when asking for a WebSocket ticket: {link.message}.
          Nothing will update until a working token is stored — this is not being retried, because
          a wrong token stays wrong.
        </span>
        <Button className="mt-2" onClick={openSystem}>
          Enter a token
        </Button>
      </Note>
    );
  }

  return (
    <Note tone="warn" glyph="◐">
      {describe(link, now)}
    </Note>
  );
}

function describe(link: LinkPhase, now: number): string {
  switch (link.phase) {
    case 'idle':
      return 'Not connected to the field node.';
    case 'authorizing':
      return `Requesting a WebSocket ticket (attempt ${link.attempt}).`;
    case 'connecting':
      return `Opening the event socket (attempt ${link.attempt}).`;
    case 'syncing':
      return 'Connected; waiting for the state snapshot. Nothing is shown until it arrives.';
    case 'retrying': {
      const seconds = Math.max(0, Math.ceil((link.retryAt - now) / 1000));
      return `Link down after ${link.attempt} attempt${link.attempt === 1 ? '' : 's'} — ${failureText(link)}. Retrying in ${seconds}s.`;
    }
    case 'live':
    case 'unauthorized':
      return '';
  }
}

function failureText(link: Extract<LinkPhase, { phase: 'retrying' }>): string {
  const { failure } = link;
  switch (failure.kind) {
    case 'unauthorized':
      return failure.message;
    case 'api':
      return `${failure.code}: ${failure.message}`;
    case 'transport':
      return failure.message;
  }
}

function Note({
  tone,
  glyph,
  children,
}: {
  tone: 'warn' | 'danger';
  glyph: string;
  children: ReactNode;
}): ReactNode {
  return (
    <div
      role="status"
      className={`mb-3 flex flex-col rounded-md border p-3 text-sm ${
        tone === 'danger' ? 'border-danger text-fg' : 'border-warn text-fg'
      }`}
    >
      <span>
        {/* Glyph first, colour second — the same rule the badges follow, for the same reasons. */}
        <span aria-hidden="true" className={tone === 'danger' ? 'text-danger' : 'text-warn'}>
          {glyph}{' '}
        </span>
        {children}
      </span>
    </div>
  );
}
