import type { ReactNode } from 'react';

import type { RequestFailure } from '../lib/api';

/**
 * How the node's refusal of a command is shown — SDD §4.2's envelope, rendered.
 *
 * This is the one thing a command is allowed to put on screen (see `lib/commands.ts`): it is the
 * reply to *this* request, not a claim about the observatory. The mount's state still comes from
 * events, and this note sits beside it rather than changing it.
 *
 * The machine-readable `code` is shown next to the sentence deliberately. `LIMIT_ALTITUDE` is
 * what the operator will type into a search or read down a phone line; the prose is what tells
 * them what to do about it, and neither substitutes for the other.
 */
export function FailureNote({
  failure,
  action,
}: {
  failure: RequestFailure;
  /** What was being attempted, e.g. "goto" — the sentence reads "goto was refused". */
  action: string;
}): ReactNode {
  return (
    <p
      role="status"
      className="mt-2 rounded-md border border-danger/50 bg-overlay p-2 text-sm text-fg"
    >
      <span aria-hidden="true" className="mr-1 text-danger">
        ⊘
      </span>
      {describe(failure, action)}
    </p>
  );
}

function describe(failure: RequestFailure, action: string): string {
  switch (failure.kind) {
    case 'unauthorized':
      return failure.presented
        ? `${action} was refused: the node rejected the stored token. Enter the right one from the header status strip.`
        : `${action} was refused: this node requires a bearer token and none is stored.`;
    case 'api':
      return `${action} was refused — ${failure.code}: ${failure.message}`;
    case 'transport':
      return `${action} did not reach the node: ${failure.message}`;
  }
}
