/*
 * The operator's clock, corrected against the node's — SDD §5.8.1's skew paragraph, §8.3(4),
 * REL-14.
 *
 * # Why this exists at all
 *
 * Since M1-T10 every command carries `issued_at`, and the node refuses a motion-initiating command
 * whose `issued_at` is older than `server.max_command_age_ms` (2000 ms by default). That timestamp
 * comes from the phone, and a phone's clock is not a fact: a device that has been in a bag since
 * the last time it saw a network can be minutes out. Sending its raw idea of "now" would mean the
 * telescope refuses every goto with `COMMAND_STALE` and the operator has no way to tell that the
 * problem is a clock rather than a link.
 *
 * So the app measures the offset and applies it. §5.8.1: "the PWA offsets `issued_at` by the
 * measured skew, and skew beyond 30 s raises a UI warning". Both halves matter — correcting
 * silently would hide a clock problem that also breaks the operator's file timestamps and their
 * session log, which is what REL-14 is about.
 *
 * # The measurement rides on the ping that already exists
 *
 * `/ws`'s `pong` carries `server_time` (§5.8.3), and `connection.ts` already sends a ping every
 * five seconds and records the round trip. That is the whole measurement: one message the app
 * sends anyway, one field it was already parsing. A second channel — a dedicated skew endpoint, or
 * a poll of a `server_time` header — would be a second thing to keep working on a link whose whole
 * problem is that things stop working.
 *
 * The correction is NTP's, reduced to its simplest form: the request left at `sentAt` and the
 * answer arrived at `receivedAt`, so the node's clock reading applies to a moment approximately
 * halfway between. Halving the round trip removes the part of the difference that is *transit*
 * rather than *offset* — on a 2 s satellite link that part is a second, which is half the staleness
 * budget.
 *
 * # A median, not the latest sample
 *
 * One pong that sat in a retransmit queue for four seconds would otherwise move the estimate by
 * two, and on this link that is a normal Tuesday. The median of the last few samples ignores it
 * without any tuning, and recovers on its own once the link steadies — which matters more than
 * precision here, because the thing being defended is a 2000 ms budget, not a timestamp anyone
 * reads.
 */

/**
 * Skew past which the UI says so — SDD §5.8.1.
 *
 * Well beyond `max_command_age_ms`, deliberately: anything smaller than a few seconds is normal
 * drift that the correction handles invisibly, and a warning the operator sees on a healthy night
 * is a warning they will have learned to ignore by the night it matters.
 */
export const SKEW_WARN_MS = 30_000;

/**
 * Samples kept for the median. Five at the 5 s ping interval is a twenty-five second window: long
 * enough that one delayed pong cannot move it, short enough that a phone whose clock was just
 * corrected by NTP catches up before the operator finishes reading the warning.
 */
const WINDOW = 5;

let samples: number[] = [];
let estimate = 0;

/**
 * Fold one answered ping into the estimate, and return the skew now believed, in milliseconds.
 *
 * Positive means **the node is ahead of this device**, which is the direction the correction is
 * then added in. `null` from a pong with no `server_time` or an unparseable one — a node older
 * than this bundle is a normal condition after an upgrade (§5.8.3), and a clock estimate is not
 * something to guess at.
 */
export function recordServerTime(
  serverTime: string | null,
  sentAt: number,
  receivedAt: number,
): number | null {
  if (serverTime === null) return null;
  const server = Date.parse(serverTime);
  if (Number.isNaN(server)) return null;

  // The node read its clock at some instant between the ping leaving and the pong arriving.
  // Halfway is the best guess available without more round trips than this link can spare.
  const midpoint = sentAt + (receivedAt - sentAt) / 2;
  samples = [...samples, server - midpoint].slice(-WINDOW);
  estimate = median(samples);
  return estimate;
}

/** The offset currently believed, in milliseconds. Zero until the first pong is answered. */
export function skewMs(): number {
  return estimate;
}

/** Whether the operator should be told (SDD §5.8.1: beyond 30 s). */
export function isSkewWorthWarning(skew: number | null): boolean {
  return skew !== null && Math.abs(skew) > SKEW_WARN_MS;
}

/**
 * `issued_at` for a command leaving now — this device's clock, corrected.
 *
 * RFC 3339 with milliseconds and a `Z`, which is SDD §2's one spelling of a timestamp and the
 * shape the node's own responses use.
 */
export function issuedAt(now: number = Date.now()): string {
  return new Date(now + estimate).toISOString();
}

/**
 * A fresh `command_id`.
 *
 * Random rather than a counter, and the reason is two browser tabs: a counter restarts at one in
 * each of them, and the node's ledger would then serve the second tab's first command out of the
 * first tab's answer. `crypto.randomUUID` where it exists (every browser this PWA targets), and a
 * `getRandomValues` fallback for older engines and for the test runner — never `Math.random`,
 * which two tabs opened in the same millisecond can and do agree on.
 */
export function newCommandId(): string {
  const source = globalThis.crypto as Crypto | undefined;
  if (source !== undefined && typeof source.randomUUID === 'function') {
    return source.randomUUID();
  }
  if (source !== undefined && typeof source.getRandomValues === 'function') {
    const bytes = source.getRandomValues(new Uint8Array(16));
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }
  throw new Error(
    'no cryptographic random source: a command id that two tabs can agree on would make the ' +
      'node replay one tab’s answer to the other (SDD §5.8.1)',
  );
}

/** Forget every sample — for tests, and for a credential change that invalidates the session. */
export function resetClock(): void {
  samples = [];
  estimate = 0;
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0
    ? ((sorted[middle - 1] ?? 0) + (sorted[middle] ?? 0)) / 2
    : (sorted[middle] ?? 0);
}
