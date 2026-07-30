import { describe, expect, it } from 'vitest';

import { ERROR_CODES, parseEvent, parseFrame } from './protocol';

/*
 * The wire, where this bundle disagreeing with the node would be silent.
 *
 * These are not tests of TypeScript's type system — they are tests of the *narrowing*, which is
 * the only thing standing between a malformed frame and a rendered coordinate. Every payload
 * arrives as `unknown`; the parsers decide what is readable. A field that stopped being checked
 * would compile, pass every other test, and show the operator whatever the node happened to send.
 *
 * The `mount.status` cases exist because M1-T03 added `tracking_mode` to SDD §4.3's payload, and
 * an additive schema change has two failure modes worth pinning: refusing frames from a node too
 * old to send it, and silently accepting a value that is not a rate.
 */

const ts = '2026-07-30T21:04:05.123Z';

const status = (data: Record<string, unknown>): unknown => ({
  v: 1,
  ts,
  topic: 'mount.status',
  data: { state: 'idle', tracking: false, slewing: false, parked: false, ...data },
});

describe('mount.status tracking_mode', () => {
  it('reads each of the three rates', () => {
    for (const rate of ['sidereal', 'lunar', 'solar'] as const) {
      const parsed = parseEvent(status({ tracking: true, tracking_mode: rate }));
      expect(parsed).not.toBeNull();
      expect(parsed?.topic).toBe('mount.status');
      if (parsed?.topic === 'mount.status') {
        expect(parsed.data.tracking_mode).toBe(rate);
        expect(parsed.data.tracking).toBe(true);
      }
    }
  });

  it('reads an explicit null as tracking being off', () => {
    const parsed = parseEvent(status({ tracking: false, tracking_mode: null }));
    if (parsed?.topic !== 'mount.status') throw new Error('expected mount.status');
    expect(parsed.data.tracking_mode).toBeNull();
    expect(parsed.data.tracking).toBe(false);
  });

  it('still parses a frame from a node that does not send the field at all', () => {
    // The additive half of the schema change. Refusing the frame would blank the mount panel
    // against a node that is working perfectly and merely predates M1-T03.
    const parsed = parseEvent(status({ tracking: true }));
    if (parsed?.topic !== 'mount.status') throw new Error('expected mount.status');
    expect(parsed.data.tracking_mode).toBeNull();
    expect(parsed.data.tracking).toBe(true);
  });

  it('refuses a rate it does not know rather than passing the string through', () => {
    // `king` is a real tracking rate on some mounts and is *not* in `TrackingMode`. Letting it
    // through would put a value in the store that no component has a case for.
    const parsed = parseEvent(status({ tracking: true, tracking_mode: 'king' }));
    if (parsed?.topic !== 'mount.status') throw new Error('expected mount.status');
    expect(parsed.data.tracking_mode).toBeNull();
    expect(parsed.data.tracking).toBe(true);
  });

  it('still requires the fields that were always required', () => {
    // The additive field must not have made the mandatory ones optional by accident.
    expect(parseEvent({ v: 1, ts, topic: 'mount.status', data: { state: 'idle' } })).toBeNull();
    expect(parseEvent(status({ state: 'launching' }))).toBeNull();
  });
});

describe('mount.position', () => {
  it('reads null alt/az, which is what M1-T03 emits until M1-T05', () => {
    const parsed = parseEvent({
      v: 1,
      ts,
      topic: 'mount.position',
      data: { ra: 5.5, dec: 22, alt: null, az: null, pier_side: 'unknown' },
    });
    if (parsed?.topic !== 'mount.position') throw new Error('expected mount.position');
    expect(parsed.data.alt).toBeNull();
    expect(parsed.data.az).toBeNull();
    expect(parsed.data.ra).toBe(5.5);
  });
});

describe('frame kinds', () => {
  it('tells a snapshot from an event by which key is present', () => {
    // The one part of the wire SDD §4.3 does not pin down, adopted from `mock/README.md` and
    // implemented by M1-T03: control frames carry `type`, events carry `topic`, never both.
    const snapshot = parseFrame(
      JSON.stringify({
        v: 1,
        type: 'snapshot',
        ts,
        events: [status({ tracking: true, tracking_mode: 'sidereal' })],
      }),
    );
    expect(snapshot.kind).toBe('snapshot');
    if (snapshot.kind === 'snapshot') {
      expect(snapshot.events).toHaveLength(1);
    }

    const event = parseFrame(JSON.stringify(status({})));
    expect(event.kind).toBe('event');
  });

  it('echoes a pong id so a round trip can be measured', () => {
    const pong = parseFrame(JSON.stringify({ v: 1, type: 'pong', ts, id: 7, server_time: ts }));
    expect(pong.kind).toBe('pong');
    if (pong.kind === 'pong') {
      expect(pong.id).toBe(7);
      expect(pong.serverTime).toBe(ts);
    }
  });
});

describe('the closed error vocabulary', () => {
  it('mirrors astroctl-core exactly, at the count that is the review checkpoint', () => {
    // 24 → 25 is `ABORTED` (M1-T03). The number is here for the same reason it is in
    // `ErrorCode::ALL`'s test: a frozen contract should not grow without someone noticing.
    expect(ERROR_CODES).toHaveLength(25);
    expect(ERROR_CODES).toContain('ABORTED');
    expect(new Set(ERROR_CODES).size).toBe(ERROR_CODES.length);
  });

  it('spells every code in SCREAMING_SNAKE_CASE, like the serde representation', () => {
    for (const code of ERROR_CODES) {
      expect(code).toMatch(/^[A-Z][A-Z_]*$/);
    }
  });
});
