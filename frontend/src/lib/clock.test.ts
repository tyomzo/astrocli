import { renderToStaticMarkup } from 'react-dom/server';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { COMMAND_ID_HEADER, ISSUED_AT_HEADER, postJson } from './api';
import {
  SKEW_WARN_MS,
  issuedAt,
  isSkewWorthWarning,
  newCommandId,
  recordServerTime,
  resetClock,
  skewMs,
} from './clock';
import { ClockSkewNoteView } from '../panels/ClockSkewNote';

/*
 * The command envelope's client half — SDD §5.8.1, §8.3(4), REL-14, M1-T10.
 *
 * The acceptance criterion this file exists for is the last `describe` below: a device whose clock
 * is a minute fast must still be able to drive the telescope, *and* must be told its clock is
 * wrong. Both halves, because either one alone is a bug — a silent correction hides a problem that
 * also corrupts the operator's photograph timestamps, and a warning without a correction is an app
 * that explains why nothing works.
 */

beforeEach(() => {
  resetClock();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetClock();
});

describe('measuring the offset from a pong', () => {
  it('subtracts half the round trip rather than counting transit as skew', () => {
    // The ping left at 1000 and the answer arrived at 3000, so the node read its clock at about
    // 2000 by this device's reckoning. A node reporting 2000 is therefore in step — and a naive
    // `server - receivedAt` would have called that a one-second offset.
    const serverTime = new Date(2000).toISOString();
    expect(recordServerTime(serverTime, 1000, 3000)).toBe(0);
    expect(skewMs()).toBe(0);
  });

  it('reports a positive offset when the node is ahead of this device', () => {
    const serverTime = new Date(62_000).toISOString();
    expect(recordServerTime(serverTime, 1000, 3000)).toBe(60_000);
  });

  it('ignores a pong with no server time rather than guessing zero', () => {
    // A node older than this bundle is a normal condition after an upgrade (SDD §5.8.3). Treating
    // its silence as "no skew" would overwrite a good estimate with a fabricated one.
    expect(recordServerTime(new Date(62_000).toISOString(), 1000, 3000)).toBe(60_000);
    expect(recordServerTime(null, 4000, 5000)).toBeNull();
    expect(recordServerTime('not a timestamp', 4000, 5000)).toBeNull();
    expect(skewMs()).toBe(60_000);
  });

  it('is not moved by one pong that sat in a retransmit queue', () => {
    // Four honest samples at +10 s and one that was delayed by twenty seconds on the way back.
    // The median holds; a "latest sample wins" estimate would have jumped by ten seconds, which
    // over `max_command_age_ms` is the difference between a working goto and a refused one.
    for (const at of [1000, 2000, 3000, 4000]) {
      recordServerTime(new Date(at + 10_000).toISOString(), at, at);
    }
    expect(skewMs()).toBe(10_000);

    recordServerTime(new Date(5000 + 30_000).toISOString(), 5000, 5000);
    expect(skewMs()).toBe(10_000);
  });

  it('follows the clock once the outliers stop being outliers', () => {
    // A phone whose clock was just corrected by the network is not an outlier — it is the new
    // truth, and the window is short enough that the estimate catches up rather than fighting it.
    for (const at of [1, 2, 3, 4, 5]) {
      recordServerTime(new Date(at + 10_000).toISOString(), at, at);
    }
    expect(skewMs()).toBe(10_000);

    for (const at of [6, 7, 8, 9, 10]) {
      recordServerTime(new Date(at).toISOString(), at, at);
    }
    expect(skewMs()).toBe(0);
  });
});

describe('the warning threshold', () => {
  it('is 30 s and fires in both directions, but not before it is measured', () => {
    expect(SKEW_WARN_MS).toBe(30_000);
    expect(isSkewWorthWarning(null)).toBe(false);
    expect(isSkewWorthWarning(0)).toBe(false);
    expect(isSkewWorthWarning(SKEW_WARN_MS)).toBe(false);
    expect(isSkewWorthWarning(SKEW_WARN_MS + 1)).toBe(true);
    expect(isSkewWorthWarning(-(SKEW_WARN_MS + 1))).toBe(true);
  });
});

describe('the command id', () => {
  it('is long enough for the node to accept and never repeats', () => {
    const ids = new Set(Array.from({ length: 500 }, newCommandId));
    expect(ids.size).toBe(500);
    // The node refuses anything under eight characters — two tabs would collide on shorter ids.
    for (const id of ids) expect(id.length).toBeGreaterThanOrEqual(8);
  });
});

describe('issued_at', () => {
  it('is RFC 3339 UTC, which is the one spelling this system uses', () => {
    const stamped = issuedAt(1_700_000_000_000);
    expect(stamped).toBe('2023-11-14T22:13:20.000Z');
    expect(stamped.endsWith('Z')).toBe(true);
  });
});

/*
 * The M1-T10 acceptance criterion: client clock +60 s.
 */
describe('a device whose clock is a minute fast', () => {
  /** The node's true time throughout. */
  const SERVER_NOW = 1_700_000_000_000;
  /** What this device believes the time is — a minute ahead. */
  const DEVICE_NOW = SERVER_NOW + 60_000;

  function stubFetch(): { headers: () => Headers } {
    let captured = new Headers();
    vi.stubGlobal(
      'fetch',
      vi.fn((_input: unknown, init: RequestInit) => {
        captured = new Headers(init.headers);
        return Promise.resolve(
          new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } }),
        );
      }),
    );
    return { headers: () => captured };
  }

  beforeEach(() => {
    vi.spyOn(Date, 'now').mockReturnValue(DEVICE_NOW);
  });

  it('still sends a timestamp the node will accept, once a pong has been answered', async () => {
    const fetched = stubFetch();

    // Uncorrected, the device would stamp a command a minute in the future. The node does not
    // refuse those (a future `issued_at` is skew, not staleness) — but the same wrong clock in the
    // other direction is refused outright, and neither reading is what the operator meant.
    await postJson('/api/mount/goto', 'token', { ra_hours: 12, dec_degrees: 70 });
    expect(Date.parse(fetched.headers().get(ISSUED_AT_HEADER) ?? '')).toBe(DEVICE_NOW);

    // One answered ping: the node reported its own time, the round trip was 200 ms.
    recordServerTime(new Date(SERVER_NOW).toISOString(), DEVICE_NOW - 200, DEVICE_NOW);
    // −59 900, not −60 000: the node's reading is attributed to the midpoint of the round trip,
    // so 100 ms of the difference is charged to transit rather than to the clock. That residue is
    // the measurement's honest error bar, and it is two orders of magnitude inside the budget.
    expect(skewMs()).toBe(-59_900);

    await postJson('/api/mount/goto', 'token', { ra_hours: 12, dec_degrees: 70 });
    const corrected = Date.parse(fetched.headers().get(ISSUED_AT_HEADER) ?? '');
    expect(
      Math.abs(corrected - SERVER_NOW),
      'the corrected stamp must land inside the node’s 2000 ms staleness budget',
    ).toBeLessThan(2000);
  });

  it('is told its clock is wrong, in words about the device rather than about the link', () => {
    const html = renderToStaticMarkup(ClockSkewNoteView({ skewMs: -60_000 }));
    expect(html).toContain('60s');
    expect(html).toContain('ahead of');
    expect(html).toContain('nothing is blocked');
    // Nothing here may read as "the telescope is unreachable" — that is `LinkBanner`'s message,
    // and the two send an operator to opposite places.
    expect(html).not.toContain('Not connected');
  });

  it('says nothing at all while the clocks agree', () => {
    expect(renderToStaticMarkup(ClockSkewNoteView({ skewMs: null }))).toBe('');
    expect(renderToStaticMarkup(ClockSkewNoteView({ skewMs: 1200 }))).toBe('');
  });

  it('names the other direction correctly', () => {
    const html = renderToStaticMarkup(ClockSkewNoteView({ skewMs: 45_000 }));
    expect(html).toContain('45s behind');
  });
});

describe('what the envelope puts on the wire', () => {
  function stubFetch(): () => Headers[] {
    const seen: Headers[] = [];
    vi.stubGlobal(
      'fetch',
      vi.fn((_input: unknown, init: RequestInit) => {
        seen.push(new Headers(init.headers));
        return Promise.resolve(
          new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } }),
        );
      }),
    );
    return () => seen;
  }

  it('attaches a fresh id to every mutation, so no two commands share one', async () => {
    const seen = stubFetch();
    await postJson('/api/mount/slew', 't', { axis: 'ra' });
    await postJson('/api/mount/slew', 't', { axis: 'ra' });

    const ids = seen().map((headers) => headers.get(COMMAND_ID_HEADER));
    expect(ids.every((id) => id !== null)).toBe(true);
    expect(
      new Set(ids).size,
      'two slew renewals sharing an id would be replayed out of the node’s ledger, and the lease ' +
        'would never be extended',
    ).toBe(2);
  });

  it('omits the envelope entirely when the caller opts out — the e-stop', async () => {
    const seen = stubFetch();
    await postJson('/api/mount/estop', 't', undefined, { keepalive: true, envelope: false });

    const headers = seen()[0];
    expect(headers?.get(COMMAND_ID_HEADER)).toBeNull();
    expect(headers?.get(ISSUED_AT_HEADER)).toBeNull();
  });
});
