import type { ReactNode } from 'react';

import { useNow } from '../lib/useNow';
import type { LinkPhase } from '../store/telemetry';
import { selectLink, useTelemetryStore } from '../store/telemetry';
import { useUiStore } from '../store/ui';
import { Button } from '../ui/Button';

/**
 * What the connection is doing, whenever that is not "working" — SDD §5.9, §8.3(8), REL-10.
 *
 * Silent when live: a banner that is always there is furniture, and the header badge already
 * carries the steady state.
 *
 * # The words here are the operator's, not ours
 *
 * This banner used to open with "The event link is down". An operator read it and asked what an
 * event link was — a fair question, because it names an implementation detail (a WebSocket
 * carrying `Event` frames) rather than anything they can act on. What they need to know is that
 * the app is no longer in touch with the telescope, so what is on screen may be stale and a
 * command may not arrive.
 *
 * The rule the strings below follow: say what is true of *the telescope*, then what it means for
 * *commands*, then what the app is doing about it. Nothing names a transport, a ticket, a
 * socket or a frame. The register is plain rather than reassuring — an operator standing in a
 * field at 2 a.m. is not helped by cheerfulness, and "reconnecting" with a countdown is more
 * informative than "oops!".
 *
 * # A bad token is a dead end, and says so
 *
 * §5.9: "If the ticket request itself fails with 401 the token is bad and the UI must say so
 * rather than retrying forever." The `unauthorized` phase is terminal in the store for that
 * reason, so this banner is the only thing that can move the operator forward — hence the button
 * straight to the credential. A spinner here would be the failure the requirement names.
 *
 * "Token" survives the rewording, deliberately: it is the label on the field the operator typed
 * it into (`ConnectPanel`), so renaming it here would make the message point at a control that
 * no longer matches it.
 *
 * # Reconnection is narrated, not hidden
 *
 * The countdown and the attempt number are shown because the alternative is an operator watching
 * a frozen panel deciding whether to reload the app in the dark, mid-session. The reason the last
 * attempt failed is shown for the same reason: "the token was refused" and "nothing answered"
 * send them to different places.
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
          The telescope refused this token, so nothing is connected and no reading on this screen
          is current. Nothing is being retried — a wrong token stays wrong until it is replaced.
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
      return 'Not connected to the mount.';
    // The two connecting phases read the same to an operator on purpose. They differ by which
    // half of the handshake is in flight, which is a distinction only a developer can act on —
    // and showing two different sentences a second apart looks like something going wrong.
    case 'authorizing':
    case 'connecting':
      return `Connecting to the mount (attempt ${link.attempt})…`;
    case 'syncing':
      return 'Connected. Waiting for the mount to report its state — nothing is shown until it does.';
    case 'retrying': {
      const seconds = Math.max(0, Math.ceil((link.retryAt - now) / 1000));
      return (
        `Not connected to the mount — commands can't be confirmed and nothing on screen is ` +
        `current. ${failureText(link)} Retrying in ${seconds}s (attempt ${link.attempt}).`
      );
    }
    case 'live':
    case 'unauthorized':
      return '';
  }
}

/**
 * Why the last attempt failed, in one sentence an operator can act on.
 *
 * The three kinds send them to genuinely different places — fix the token, go look at the
 * machine, or wait — so they are worth distinguishing. What is *not* worth showing is the
 * underlying text: a close code, a path, or "no frame for 12 s" tells an operator nothing they
 * can do anything about, and reads as though the app is malfunctioning rather than the link.
 */
function failureText(link: Extract<LinkPhase, { phase: 'retrying' }>): string {
  switch (link.failure.kind) {
    case 'unauthorized':
      return 'The stored token was refused.';
    case 'api':
      return 'The telescope answered with an error.';
    case 'transport':
      return 'Nothing is answering — check power and the network link to the telescope.';
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
