import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { MountPosition, MountStatus, TrackingRate } from './link/protocol';
import type { Observation } from './predict';
import {
  CONFIRMED_MS,
  PREDICT_LIMIT_MS,
  raRateHoursPerMs,
  reckon,
  telemetryAgeMs,
} from './predict';

/*
 * The dead reckoning, tested where getting it wrong would be invisible.
 *
 * Two of these assertions are deliberately the same numbers the node's own simulator asserts
 * (`astroctl-drivers::simulator::mount`): an idle mount's RA climbing 1.00274 h per hour, and the
 * lunar rate slipping ~1,281 arcsec per hour. That duplication is the point. The display and the
 * drive have to agree about which way the sky turns, and the only way a disagreement shows up
 * otherwise is as coordinates that are subtly wrong on a screen nobody can check against anything.
 */

const T0 = Date.parse('2026-07-30T21:04:05.000Z');

const POSITION: MountPosition = {
  ra: 5.588_25,
  dec: -5.391,
  alt: 47.2,
  az: 128.4,
  pier_side: 'west',
};

function observed(value: Partial<MountPosition> = {}, atMs = T0): Observation<MountPosition> {
  return { at: atMs, ts: new Date(atMs).toISOString(), value: { ...POSITION, ...value } };
}

function status(over: Partial<MountStatus> = {}): MountStatus {
  return { state: 'idle', tracking: false, tracking_mode: null, slewing: false, parked: false, ...over };
}

function tracking(mode: TrackingRate): MountStatus {
  return status({ tracking: true, tracking_mode: mode });
}

/** Hours of RA the model moves in one hour of clock, for the mount state given. */
function hoursPerHour(mount: MountStatus | null): number {
  return raRateHoursPerMs(mount) * 3_600_000;
}

describe('which way the displayed right ascension runs', () => {
  it('holds RA while the mount tracks at the sidereal rate', () => {
    // Tracking *is* standing still relative to the sky: the axis turns west exactly as fast as
    // local sidereal time advances, so RA = LST − HA does not change (SDD §5.2.3).
    expect(hoursPerHour(tracking('sidereal'))).toBeCloseTo(0, 9);

    const held = reckon({ position: observed(), status: tracking('sidereal'), nowMs: T0 + 4_000 });
    expect(held.ra).toBeCloseTo(POSITION.ra, 9);
  });

  it('advances RA at the full sidereal rate while the mount is idle', () => {
    // The same number the node's simulator asserts: 1.00274 hours of RA per hour of clock. A
    // stopped drive whose displayed RA climbs is not a defect — it is what a stopped drive looks
    // like, and it is the picture that says "tracking is off" before the stars trail.
    expect(hoursPerHour(status())).toBeCloseTo(1.002_74, 5);

    const drifting = reckon({ position: observed(), status: status(), nowMs: T0 + 5_000 });
    // Five seconds of clock is 5.0137 seconds of RA — 50 ticks of the readout's last digit.
    const secondsOfRa = (drifting.ra - POSITION.ra) * 3600;
    expect(secondsOfRa).toBeCloseTo(5.0137, 3);
  });

  it('lets the star field slip west at the lunar rate, in the direction the node uses', () => {
    // (15.041 − 14.685) arcsec/s over an hour ≈ 1,281 arcsec of RA, matching
    // `the_lunar_rate_lets_the_stars_slip_west`. The *sign* is the assertion: a rate table that
    // made the Moon faster than the stars would trail a lunar sequence the wrong way, and nothing
    // would say so until somebody imaged the Moon.
    const arcsecPerHour = hoursPerHour(tracking('lunar')) * 15 * 3600;
    expect(arcsecPerHour).toBeGreaterThan(0);
    expect(arcsecPerHour).toBeCloseTo(1281, -1);
  });

  it('creeps at the solar rate, which is slower than sidereal but barely', () => {
    const arcsecPerHour = hoursPerHour(tracking('solar')) * 15 * 3600;
    // (15.0410686 − 15.0) arcsec/s × 3600 ≈ 148 arcsec/hour.
    expect(arcsecPerHour).toBeCloseTo(148, -1);
    // Under a second of RA in the whole five-second prediction window: the display holds, in
    // practice, but for a stated reason rather than by accident.
    expect(raRateHoursPerMs(tracking('solar')) * PREDICT_LIMIT_MS * 3600).toBeLessThan(0.1);
  });

  it('reads a tracking mount with no stated rate as sidereal, so it never invents motion', () => {
    // A node older than this bundle is a normal condition (SDD §5.8.3). Guessing "idle" would
    // dead-reckon a tracking mount forward at the full sidereal rate — movement that is not
    // happening, which is a worse error than being a little behind.
    expect(hoursPerHour(status({ tracking: true, tracking_mode: null }))).toBeCloseTo(0, 9);
  });

  it('never moves declination, under any rate', () => {
    for (const mount of [null, status(), tracking('sidereal'), tracking('lunar')]) {
      const r = reckon({ position: observed(), status: mount, nowMs: T0 + 4_000 });
      expect(r.dec).toBe(POSITION.dec);
    }
  });

  it('wraps right ascension at 24 h rather than printing hour 27', () => {
    const nearMidnight = reckon({
      position: observed({ ra: 23.999 }),
      status: status(),
      nowMs: T0 + 5_000,
    });

    expect(nearMidnight.ra).toBeGreaterThanOrEqual(0);
    expect(nearMidnight.ra).toBeLessThan(1);
  });
});

describe('the guardrail', () => {
  it('predicts right up to five cadences and stops the moment it is past them', () => {
    // "Never fabricate beyond one expected update gap ×5" — §8.3(8). The boundary is asserted on
    // both sides because an off-by-one here is a display that either gives up a second early or
    // keeps inventing coordinates, and neither is visible without this test.
    const drifting = { position: observed(), status: status() };

    expect(reckon({ ...drifting, nowMs: T0 + PREDICT_LIMIT_MS }).kind).toBe('predicted');
    expect(reckon({ ...drifting, nowMs: T0 + PREDICT_LIMIT_MS + 1 }).kind).toBe('stale');
  });

  it('shows the last reported numbers once stale, not the ones it had been working out', () => {
    // The struck-out value has to be something the mount actually said. Carrying the extrapolation
    // into the stale state would freeze an invented number on screen and label it "last reported".
    const stale = reckon({
      position: observed(),
      status: status(),
      nowMs: T0 + PREDICT_LIMIT_MS + 10_000,
    });

    expect(stale.kind).toBe('stale');
    expect(stale.ra).toBe(POSITION.ra);
    expect(stale.rateHoursPerMs).toBe(0);
  });

  it('calls a report confirmed within the cadence and predicted past it', () => {
    const drifting = { position: observed(), status: status() };

    expect(reckon({ ...drifting, nowMs: T0 + CONFIRMED_MS }).kind).toBe('confirmed');
    expect(reckon({ ...drifting, nowMs: T0 + CONFIRMED_MS + 1 }).kind).toBe('predicted');
  });

  it('still carries the number forward inside the confirmed window, so nothing jumps', () => {
    // The label changes at the boundary; the arithmetic does not. If extrapolation switched on at
    // CONFIRMED_MS the display would sit still for a second and a half and then leap by a second
    // and a half of RA, which reads as a glitch and is less accurate than the smooth version.
    const justInside = reckon({ position: observed(), status: status(), nowMs: T0 + 1_000 });

    expect(justInside.kind).toBe('confirmed');
    expect(justInside.ra).toBeGreaterThan(POSITION.ra);
  });
});

describe('a mount in motion', () => {
  it('holds the last numbers instead of extrapolating them', () => {
    // A goto runs at up to 835× sidereal, so a model built on tracking rates is wrong by degrees
    // within a second. `mount.position` keeps its 1 Hz cadence through a slew, so the event stream
    // is still the truth channel and the display simply waits for it.
    const slewing = reckon({
      position: observed(),
      status: status({ state: 'slewing', slewing: true }),
      nowMs: T0 + 3_000,
    });

    expect(slewing.kind).toBe('moving');
    expect(slewing.ra).toBe(POSITION.ra);
    expect(slewing.rateHoursPerMs).toBe(0);
  });

  it('is held even while the reading is perfectly fresh', () => {
    // 300 ms old and already wrong by arcminutes. This is the case that makes `moving` a state of
    // its own rather than a flavour of `confirmed`.
    const justArrived = reckon({
      position: observed(),
      status: status({ state: 'slewing', slewing: true }),
      nowMs: T0 + 300,
    });

    expect(justArrived.kind).toBe('moving');
  });

  it('gives way to stale, because a slew nobody is hearing about is not a slew', () => {
    const abandoned = reckon({
      position: observed(),
      status: status({ state: 'slewing', slewing: true }),
      nowMs: T0 + PREDICT_LIMIT_MS + 1,
    });

    expect(abandoned.kind).toBe('stale');
  });
});

describe('telemetry age', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('is measured on the node’s clock, corrected by the skew, not on the phone’s', () => {
    // A phone forty seconds fast. Uncorrected, every event it receives looks forty seconds stale —
    // thirteen times §8.3's amber threshold, on a link that is working perfectly.
    const deviceAhead = 40_000;
    vi.setSystemTime(T0 + deviceAhead);

    const justArrived = { at: Date.now(), ts: new Date(T0).toISOString() };
    // skew is "node minus device", so a fast phone measures a negative offset.
    expect(telemetryAgeMs(justArrived, Date.now(), -deviceAhead)).toBe(0);
    expect(telemetryAgeMs(justArrived, Date.now(), 0)).toBe(deviceAhead);
  });

  it('counts the transit the node’s stamp reveals, which is what makes it the honest number', () => {
    // Arrived 100 ms ago, measured a second before that: the picture is 1.1 s old, not 0.1 s. On a
    // two-second tunnel that difference is most of the staleness budget.
    vi.setSystemTime(T0 + 1_100);
    const delayed = { at: T0 + 1_000, ts: new Date(T0).toISOString() };

    expect(telemetryAgeMs(delayed, Date.now(), 0)).toBe(1_100);
  });

  it('never reports a picture as fresher than the moment it arrived', () => {
    // The floor that stops a not-yet-measured skew — zero until the first pong, five seconds after
    // connect — from making a stale picture look current.
    vi.setSystemTime(T0 + 4_000);
    const arrivedLongAgo = { at: T0, ts: new Date(T0 + 3_900).toISOString() };

    expect(telemetryAgeMs(arrivedLongAgo, Date.now(), 0)).toBe(4_000);
  });

  it('falls back to arrival when the node stamps something it cannot read', () => {
    vi.setSystemTime(T0 + 2_500);

    expect(telemetryAgeMs({ at: T0, ts: 'not a timestamp' }, Date.now(), 0)).toBe(2_500);
  });

  it('defaults to the wall clock, so a caller cannot forget to pass one', () => {
    vi.setSystemTime(T0 + 750);

    expect(telemetryAgeMs({ at: T0, ts: new Date(T0).toISOString() })).toBe(750);
  });
});

describe('a link throttled to one position every five seconds', () => {
  // The M1-T15 acceptance criterion, as a deterministic clock rather than a stopwatch and a
  // browser. The mount is **idle**, which is the state whose RA visibly moves — see the module
  // docs, and the report: a *tracking* mount's RA holding still is the whole point of tracking.

  it('advances smoothly between reports and snaps back onto each one', () => {
    const mount = status();
    const reportedRa = [5.0, 5.001_4, 5.002_8];
    const samples: { t: number; ra: number; kind: string }[] = [];

    for (const [index, ra] of reportedRa.entries()) {
      const eventAt = T0 + index * 5_000;
      const position = observed({ ra }, eventAt);
      for (let offset = 0; offset < 5_000; offset += 200) {
        const r = reckon({ position, status: mount, nowMs: eventAt + offset });
        samples.push({ t: eventAt + offset - T0, ra: r.ra, kind: r.kind });
      }
    }

    // Smooth: every 200 ms tick moves RA forward by the same 0.2 s of sidereal rotation, and none
    // of them stands still. A display that only moved when an event arrived would fail here.
    const withinGap = samples.filter((s) => s.t % 5_000 !== 0);
    expect(withinGap.every((s) => s.ra > 0)).toBe(true);
    for (let i = 1; i < samples.length; i += 1) {
      const step = samples[i]!.ra - samples[i - 1]!.ra;
      const crossedAnEvent = samples[i]!.t % 5_000 === 0;
      if (!crossedAnEvent) {
        // 200 ms of sidereal drift = 0.2005 seconds of RA = 5.57e-5 hours.
        expect(step).toBeCloseTo(5.57e-5, 7);
      }
    }

    // Snaps: the first sample after each real report is exactly what the mount said, with no
    // accumulated model error carried across the boundary.
    expect(samples.find((s) => s.t === 5_000)?.ra).toBe(5.001_4);
    expect(samples.find((s) => s.t === 10_000)?.ra).toBe(5.002_8);

    // And marked: at this cadence the display is predicted for most of every gap, and reaches the
    // guardrail exactly as the next report is due — which is the honest report of a link running
    // at a fifth of §4.3's contract, not a slower contract the app quietly accepted.
    expect(samples.find((s) => s.t === 200)?.kind).toBe('confirmed');
    expect(samples.find((s) => s.t === 2_000)?.kind).toBe('predicted');
    expect(samples.find((s) => s.t === 4_800)?.kind).toBe('predicted');
    expect(reckon({ position: observed({ ra: 5 }), status: mount, nowMs: T0 + 5_001 }).kind).toBe(
      'stale',
    );
  });
});
