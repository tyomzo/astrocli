import { telemetryAgeMs } from '../predict';
import type { LinkPhase } from '../../store/telemetry';

/*
 * How good the link is, in the two numbers §8.3(8) asks the header to show — RTT and telemetry age.
 *
 * # Both numbers already exist; this module only grades them
 *
 * `lib/link/connection.ts` has pinged every five seconds since M1-T04, because silence rather than
 * a close event is how this link usually dies, and it has published the round trip it measures ever
 * since. `store/telemetry.ts` records when the node last said anything. Nothing new is measured
 * here — which matters, because a second measurement path would be a second thing to keep working
 * on a link whose entire problem is that things stop working.
 *
 * # Why two thresholds and not one
 *
 * They fail independently and they mean different things to someone about to press a button.
 *
 *   * **RTT past 500 ms** says a command will take a noticeable moment to land. It does not mean
 *     the picture is wrong. §5.8.1's staleness budget is 2000 ms of one-way age, so half a second
 *     of round trip is a quarter of it spent before the request is even written.
 *   * **Telemetry age past 3 s** says the picture is behind — three cadences of `mount.position`
 *     with nothing arriving. The link can be answering pings promptly and still be delivering no
 *     events, which is precisely what a hub that dropped this client as a slow consumer looks
 *     like (§5.8.3).
 *
 * A single combined grade would hide which one tripped, and the two send the operator to different
 * places: one is "wait for it", the other is "what you are looking at is not now".
 *
 * # Degradation is shown without being asked for
 *
 * §8.3(8): "degradation is explicit, never silent". The numerals are therefore *always* on screen
 * once either threshold trips, and the tap only exists for the operator who wants them while the
 * link is green. A design where the numbers are only ever behind a tap would make the amber state
 * something you have to already suspect in order to find.
 */

/** §8.3/§5.9: past this round trip the header goes amber. */
export const RTT_AMBER_MS = 500;

/** §8.3/§5.9: past this telemetry age the header goes amber — three `mount.position` cadences. */
export const AGE_AMBER_MS = 3_000;

export type LinkGrade = 'good' | 'degraded' | 'starting' | 'down' | 'idle';

export interface LinkHealth {
  grade: LinkGrade;
  /** Round trip from the last answered ping, milliseconds. `null` before the first one. */
  rttMs: number | null;
  /** How stale the picture is, skew-corrected. `null` when the node has said nothing yet. */
  ageMs: number | null;
  /**
   * What the operator is told, in their words rather than the transport's.
   *
   * One clause, because it is read as a badge tooltip and announced by a screen reader, not as a
   * banner — `LinkBanner` is what explains a connection at length.
   */
  wording: string;
}

export function linkHealth(input: {
  link: LinkPhase;
  rttMs: number | null;
  lastEvent: { at: number; ts: string } | null;
  nowMs: number;
  skewMs?: number;
}): LinkHealth {
  const { link, rttMs, lastEvent, nowMs, skewMs = 0 } = input;
  const ageMs = lastEvent === null ? null : telemetryAgeMs(lastEvent, nowMs, skewMs);

  switch (link.phase) {
    case 'idle':
      return { grade: 'idle', rttMs, ageMs, wording: 'not connected to the telescope' };
    case 'authorizing':
    case 'connecting':
    case 'syncing':
      return { grade: 'starting', rttMs, ageMs, wording: 'connecting to the telescope' };
    case 'retrying':
    case 'unauthorized':
      // Red, and the age keeps climbing behind it — that number is the whole point while the link
      // is down, because it is the answer to "how old is what I am looking at".
      return { grade: 'down', rttMs, ageMs, wording: 'not in touch with the telescope' };
    case 'live':
      break;
  }

  const slow = rttMs !== null && rttMs > RTT_AMBER_MS;
  const behind = ageMs !== null && ageMs > AGE_AMBER_MS;
  if (!slow && !behind) {
    return { grade: 'good', rttMs, ageMs, wording: 'in touch with the telescope' };
  }

  // Each sentence names the telescope, because "running slow" on its own is a fact about nothing
  // the operator can see, and the two halves are separately actionable: one is "your command will
  // take a moment to land", the other is "what you are looking at is not now".
  const trip = `${Math.round(rttMs ?? 0)}ms`;
  const silence = secondsText(ageMs ?? 0);
  const wording =
    slow && behind
      ? `running slow — the round trip is ${trip} and the telescope has sent nothing for ${silence}`
      : slow
        ? `running slow — the round trip to the telescope is ${trip}`
        : `the telescope has sent nothing for ${silence}`;
  return { grade: 'degraded', rttMs, ageMs, wording };
}

/**
 * Whether the numbers belong on screen without anyone asking for them — §8.3(8).
 *
 * Amber and red, obviously. The third case is the one the grade alone misses and it was found by
 * running a link that carried the upgrade and then delivered nothing: the client tears the socket
 * down after twelve seconds of silence and reconnects, so the phase reads `connecting` — which is
 * true, is amber, and says nothing at all about the forty-second-old coordinates still on screen.
 * A link on its way up is not a reason to stop reporting how old the picture is.
 */
export function isWorthShowing(health: LinkHealth): boolean {
  if (health.grade === 'degraded' || health.grade === 'down') return true;
  return health.ageMs !== null && health.ageMs > AGE_AMBER_MS;
}

/**
 * The two numbers, for the header — `620 ms · 4.2 s`.
 *
 * An em dash where a number does not exist yet, never a zero: "0 ms" is a claim about a
 * measurement that has not been made, and this readout exists to stop exactly that kind of claim.
 */
export function linkNumerals(health: LinkHealth): string {
  const rtt = health.rttMs === null ? '—' : `${Math.round(health.rttMs)} ms`;
  const age = health.ageMs === null ? '—' : secondsText(health.ageMs);
  return `${rtt} · ${age}`;
}

/**
 * `4.2 s`, then `3 min`, then `2 h`.
 *
 * A tenth of a second up to a minute because the thresholds this is read against are seconds
 * apart; coarser after that because nobody reads the second decimal of a link that has been down
 * for an hour.
 */
function secondsText(ms: number): string {
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)} min`;
  return `${Math.floor(ms / 3_600_000)} h`;
}
