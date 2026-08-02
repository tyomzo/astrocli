import { describe, expect, it } from 'vitest';

import { separationKm } from './SiteCard';

/**
 * The threshold that decides whether the operator is warned is a distance, so the distance is
 * worth pinning. The cases are the real ones: the example config's Oslo, and a site far enough
 * away that every altitude the node computes is wrong.
 */
describe('separationKm', () => {
  const oslo = { latitude: 59.9139, longitude: 10.7522 };

  it('is zero at the same point', () => {
    expect(separationKm(oslo, oslo)).toBe(0);
  });

  it('measures the Oslo-default mismatch that motivated the card', () => {
    // Vilnius — the shape of "shipped with the example config and deployed elsewhere".
    const vilnius = { latitude: 54.6872, longitude: 25.2797 };
    // ~1045 km great-circle; the bound is loose because the assertion is "far", not "exactly".
    expect(separationKm(oslo, vilnius)).toBeGreaterThan(1000);
    expect(separationKm(oslo, vilnius)).toBeLessThan(1100);
  });

  it('stays under the warning threshold for a site across town', () => {
    const acrossTown = { latitude: 59.94, longitude: 10.8 };
    expect(separationKm(oslo, acrossTown)).toBeLessThan(25);
  });

  it('is symmetric', () => {
    const other = { latitude: 12.5, longitude: -70.1 };
    expect(separationKm(oslo, other)).toBeCloseTo(separationKm(other, oslo), 9);
  });

  it('does not produce NaN at antipodes, where the naive arcsin overflows', () => {
    const antipode = { latitude: -59.9139, longitude: 10.7522 - 180 };
    const d = separationKm(oslo, antipode);
    expect(Number.isNaN(d)).toBe(false);
    expect(d).toBeCloseTo(20015, 0);
  });
});
