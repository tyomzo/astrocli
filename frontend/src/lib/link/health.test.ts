import { describe, expect, it } from 'vitest';

import type { LinkPhase } from '../../store/telemetry';
import { AGE_AMBER_MS, RTT_AMBER_MS, isWorthShowing, linkHealth, linkNumerals } from './health';

/*
 * The two thresholds SDD §5.9 and §8.3(8) state in numbers — 500 ms round trip, 3 s telemetry age.
 *
 * Written against the boundaries rather than the middle, because the failure this guards is a
 * refactor that turns `>` into `>=` or drops one of the two conditions: the header would stay
 * green on a link that is a second behind, and nothing else in the app would notice.
 */

const T0 = Date.parse('2026-07-30T21:04:05.000Z');
const LIVE: LinkPhase = { phase: 'live', since: T0 };

function health(over: {
  link?: LinkPhase;
  rttMs?: number | null;
  ageMs?: number;
  skewMs?: number;
}) {
  const ageMs = over.ageMs ?? 0;
  return linkHealth({
    link: over.link ?? LIVE,
    rttMs: over.rttMs === undefined ? 50 : over.rttMs,
    lastEvent: { at: T0 - ageMs, ts: new Date(T0 - ageMs).toISOString() },
    nowMs: T0,
    skewMs: over.skewMs ?? 0,
  });
}

describe('grading a live link', () => {
  it('is good when both numbers are inside their thresholds', () => {
    expect(health({ rttMs: RTT_AMBER_MS, ageMs: AGE_AMBER_MS }).grade).toBe('good');
  });

  it('goes amber one millisecond past the round-trip threshold', () => {
    expect(health({ rttMs: RTT_AMBER_MS + 1, ageMs: 0 }).grade).toBe('degraded');
  });

  it('goes amber one millisecond past the telemetry-age threshold, on its own', () => {
    // Independently of RTT: a hub that dropped this client as a slow consumer (§5.8.3) answers
    // pings promptly and delivers no events, which is a fast link showing an old picture.
    expect(health({ rttMs: 20, ageMs: AGE_AMBER_MS + 1 }).grade).toBe('degraded');
  });

  it('says which of the two tripped, because they send the operator to different places', () => {
    expect(health({ rttMs: 620, ageMs: 0 }).wording).toContain('round trip');
    expect(health({ rttMs: 620, ageMs: 0 }).wording).not.toContain('sent nothing');

    expect(health({ rttMs: 20, ageMs: 4_200 }).wording).toContain('sent nothing for 4.2 s');
    expect(health({ rttMs: 20, ageMs: 4_200 }).wording).not.toContain('round trip');

    const both = health({ rttMs: 620, ageMs: 4_200 }).wording;
    expect(both).toContain('round trip');
    expect(both).toContain('sent nothing');
  });

  it('does not grade on a round trip it has never measured', () => {
    // `null` is "no ping has been answered yet", which is a normal five-second window after
    // connect. Treating it as zero would report a measurement nobody made.
    expect(health({ rttMs: null, ageMs: 0 }).grade).toBe('good');
    expect(linkNumerals(health({ rttMs: null, ageMs: 0 }))).toContain('—');
  });

  it('measures the age on the node’s clock, corrected for skew', () => {
    // A phone forty seconds fast: the node stamps an event with its own clock, which reads forty
    // seconds *earlier*. Skew is "node minus device", so this device measures −40 s. Uncorrected,
    // the header would sit amber all night on a connection that is working perfectly.
    const justArrived = { at: T0, ts: new Date(T0 - 40_000).toISOString() };
    const measure = (skewMs: number) =>
      linkHealth({ link: LIVE, rttMs: 20, lastEvent: justArrived, nowMs: T0, skewMs });

    expect(measure(-40_000).ageMs).toBe(0);
    expect(measure(-40_000).grade).toBe('good');

    // What it looks like without the correction, which is the defect this guards.
    expect(measure(0).ageMs).toBe(40_000);
    expect(measure(0).grade).toBe('degraded');
  });
});

describe('grading a link that is not live', () => {
  it('is red while retrying, and keeps showing how old the picture is', () => {
    const retrying = health({
      link: { phase: 'retrying', attempt: 2, retryAt: T0 + 500, failure: { kind: 'transport', message: 'closed' } },
      ageMs: 12_000,
    });

    expect(retrying.grade).toBe('down');
    // The age is the most useful number on the screen at exactly this moment, so it stays.
    expect(retrying.ageMs).toBe(12_000);
    expect(linkNumerals(retrying)).toContain('12.0 s');
  });

  it('is red on a refused token, which is a dead end rather than a slow link', () => {
    expect(
      health({ link: { phase: 'unauthorized', at: T0, message: 'bad token' } }).grade,
    ).toBe('down');
  });

  it('is amber-but-starting while connecting, which is not the same as degraded', () => {
    // Both are amber; they read identically to a colour-blind operator and mean opposite things.
    // One says wait, the other says distrust the screen.
    for (const phase of ['authorizing', 'connecting', 'syncing'] as const) {
      expect(health({ link: { phase, attempt: 1 } }).grade).toBe('starting');
    }
  });

  it('is hollow, not red, before anything has been attempted', () => {
    expect(health({ link: { phase: 'idle' } }).grade).toBe('idle');
  });

  it('says nothing about an age it has no event for', () => {
    const nothingYet = linkHealth({
      link: LIVE,
      rttMs: 40,
      lastEvent: null,
      nowMs: T0,
    });

    expect(nothingYet.ageMs).toBeNull();
    expect(nothingYet.grade).toBe('good');
    expect(linkNumerals(nothingYet)).toBe('40 ms · —');
  });
});

describe('the numerals', () => {
  it('reads as two measurements, not one', () => {
    expect(linkNumerals(health({ rttMs: 619.6, ageMs: 4_240 }))).toBe('620 ms · 4.2 s');
  });

  it('coarsens once a link has been down long enough that decimals are noise', () => {
    expect(linkNumerals(health({ rttMs: 20, ageMs: 125_000 }))).toContain('2 min');
    expect(linkNumerals(health({ rttMs: 20, ageMs: 7_400_000 }))).toContain('2 h');
  });

  it('never prints a zero for a measurement it has not made', () => {
    const unmeasured = linkHealth({ link: LIVE, rttMs: null, lastEvent: null, nowMs: T0 });
    expect(linkNumerals(unmeasured)).toBe('— · —');
  });
});

describe('when the numbers show themselves', () => {
  // §8.3(8): "degradation is explicit, never silent". The tap is for a green link somebody wants
  // to interrogate anyway; it is never the only way to find out something is wrong.

  it('stays out of the way while the link is good', () => {
    expect(isWorthShowing(health({ rttMs: 40, ageMs: 800 }))).toBe(false);
  });

  it('shows itself on amber and on red', () => {
    expect(isWorthShowing(health({ rttMs: 620, ageMs: 0 }))).toBe(true);
    expect(isWorthShowing(health({ link: { phase: 'idle' }, ageMs: 0 }))).toBe(false);
    expect(
      isWorthShowing(health({ link: { phase: 'unauthorized', at: T0, message: 'x' }, ageMs: 0 })),
    ).toBe(true);
  });

  it('shows itself while reconnecting over an old picture, which the grade alone misses', () => {
    // Found by running a link that carried the upgrade and then delivered nothing: the client
    // tears the socket down after twelve seconds and reconnects, so the phase reads `connecting`.
    // That is true, and it says nothing about the forty-second-old coordinates still on screen.
    const comingUpOverStaleData = health({
      link: { phase: 'syncing', attempt: 2 },
      ageMs: 40_000,
    });

    expect(comingUpOverStaleData.grade).toBe('starting');
    expect(isWorthShowing(comingUpOverStaleData)).toBe(true);
  });

  it('does not show itself while connecting for the first time, with nothing to be stale', () => {
    const firstConnect = linkHealth({
      link: { phase: 'connecting', attempt: 1 },
      rttMs: null,
      lastEvent: null,
      nowMs: T0,
    });

    expect(isWorthShowing(firstConnect)).toBe(false);
  });
});

describe('the operator-facing wording', () => {
  it('names the telescope rather than the transport', () => {
    const phrases = [
      health({ link: { phase: 'idle' } }).wording,
      health({ link: { phase: 'connecting', attempt: 1 } }).wording,
      health({ link: { phase: 'unauthorized', at: T0, message: 'x' } }).wording,
      health({ rttMs: 20, ageMs: 0 }).wording,
      health({ rttMs: 620, ageMs: 0 }).wording,
      health({ rttMs: 20, ageMs: 4_200 }).wording,
      health({ rttMs: 620, ageMs: 4_200 }).wording,
    ];

    for (const phrase of phrases) {
      expect(phrase).toContain('telescope');
      for (const jargon of ['socket', 'websocket', 'ticket', 'topic', 'payload', 'event link']) {
        expect(phrase.toLowerCase()).not.toContain(jargon);
      }
    }
  });
});
