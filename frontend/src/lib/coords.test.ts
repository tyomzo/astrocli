import { describe, expect, it } from 'vitest';

import {
  formatDecDegrees,
  formatDegrees,
  formatRaHours,
  parseDecDegrees,
  parseRaHours,
} from './coords';

/*
 * Coordinate notation — USB-05.
 *
 * The cases that matter are the ones a desk never produces: the sign on a declination between 0°
 * and -1°, seconds that round up into the next minute, and the forms a catalog prints that are not
 * the form this app prints.
 */

describe('formatting', () => {
  it('prints RA as sexagesimal hours', () => {
    expect(formatRaHours(5.588_055_6)).toBe('05:35:17.0');
    expect(formatRaHours(0)).toBe('00:00:00.0');
  });

  it('prints DEC with an explicit sign', () => {
    expect(formatDecDegrees(-5.391_111)).toBe('-05°23\'28"');
    expect(formatDecDegrees(22.014_44)).toBe('+22°00\'52"');
  });

  it('carries rounding rather than printing a sixtieth unit', () => {
    // 59.97 s rounds to 60.0 at one decimal; `05:35:60.0` is not a time.
    expect(formatRaHours(5 + 35 / 60 + 59.97 / 3600)).toBe('05:36:00.0');
    expect(formatDecDegrees(21 + 59 / 60 + 59.7 / 3600)).toBe('+22°00\'00"');
  });

  it('wraps RA rather than printing a 24th hour', () => {
    expect(formatRaHours(23 + 59 / 60 + 59.99 / 3600)).toBe('00:00:00.0');
  });

  it('renders an absent altitude as unknown, never as zero', () => {
    // M1-T03 emits null alt/az until M1-T05. `0.0°` would put the target on the horizon, which is
    // exactly where the altitude limit lives.
    expect(formatDegrees(null)).toBe('—');
    expect(formatDegrees(47.25)).toBe('47.3°');
  });
});

describe('parsing right ascension', () => {
  it('accepts every form a catalog prints', () => {
    for (const text of ['05:35:17.3', '5 35 17.3', '05h35m17.3s', ' 05:35:17.3 ']) {
      const parsed = parseRaHours(text);
      expect(parsed.ok && parsed.value).toBeCloseTo(5.588_138_9, 6);
    }
  });

  it('reads a bare number as hours, because the field is RA', () => {
    const parsed = parseRaHours('5.5');
    expect(parsed.ok && parsed.value).toBe(5.5);
  });

  it('refuses values outside 0h to 24h, and says which rule broke', () => {
    expect(parseRaHours('24:00:01')).toEqual({ ok: false, problem: 'right ascension runs 0h to 24h' });
    expect(parseRaHours('-1:00:00').ok).toBe(false);
    expect(parseRaHours('05:60:00')).toEqual({ ok: false, problem: 'minutes run 0 to 59' });
    expect(parseRaHours('')).toEqual({ ok: false, problem: 'enter a coordinate' });
    expect(parseRaHours('five')).toEqual({ ok: false, problem: '"five" is not a number' });
  });
});

describe('parsing declination', () => {
  it('accepts sexagesimal and decimal degrees', () => {
    for (const text of ['-05:23:28', '-5 23 28', '-05°23\'28"']) {
      const parsed = parseDecDegrees(text);
      expect(parsed.ok && parsed.value).toBeCloseTo(-5.391_111, 6);
    }
    const decimal = parseDecDegrees('-5.391111');
    expect(decimal.ok && decimal.value).toBeCloseTo(-5.391_111, 6);
  });

  it('keeps the sign when the degrees are zero', () => {
    // The case that catches a parser reading the sign off the first number: -0 is 0.
    const south = parseDecDegrees('-00:30:00');
    expect(south.ok && south.value).toBeCloseTo(-0.5, 9);

    const north = parseDecDegrees('+00:30:00');
    expect(north.ok && north.value).toBeCloseTo(0.5, 9);
  });

  it('refuses beyond the poles', () => {
    expect(parseDecDegrees('90:00:01')).toEqual({ ok: false, problem: 'declination runs -90° to +90°' });
    expect(parseDecDegrees('-91').ok).toBe(false);
    expect(parseDecDegrees('90:00:00').ok).toBe(true);
  });

  it('refuses a fractional value combined with minutes, which has no intended reading', () => {
    expect(parseDecDegrees('5.5:30').ok).toBe(false);
  });
});

describe('round trip', () => {
  it('parses back what it printed', () => {
    for (const hours of [0, 1.25, 5.588_055, 12.5, 23.999]) {
      const parsed = parseRaHours(formatRaHours(hours));
      expect(parsed.ok && parsed.value).toBeCloseTo(hours, 4);
    }
    for (const degrees of [0, -0.5, 22.0144, -5.3911, 89.9, -89.9]) {
      const parsed = parseDecDegrees(formatDecDegrees(degrees));
      expect(parsed.ok && parsed.value).toBeCloseTo(degrees, 3);
    }
  });
});
