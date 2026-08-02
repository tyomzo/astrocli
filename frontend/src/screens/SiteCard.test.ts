import { describe, expect, it } from 'vitest';

import { separationKm } from './SiteCard';

/**
 * The threshold that decides whether the operator is warned is a distance, so the distance is
 * worth pinning. The cases are the real ones: the deployment site, and a default left behind from
 * somewhere far enough away that every altitude the node computes is wrong.
 */
describe('separationKm', () => {
  const vilnius = { latitude: 54.6872, longitude: 25.2797 };

  it('is zero at the same point', () => {
    expect(separationKm(vilnius, vilnius)).toBe(0);
  });

  it('measures the stale-default mismatch that motivated the card', () => {
    // Oslo — the example config's site until 2026-08-02, and the exact shape of the hazard: a
    // node still carrying a default from a previous deployment computes a horizon 1000 km away
    // and stays perfectly self-consistent while doing it.
    const stale = { latitude: 59.9139, longitude: 10.7522 };
    // ~1045 km great-circle; the bound is loose because the assertion is "far", not "exactly".
    expect(separationKm(vilnius, stale)).toBeGreaterThan(1000);
    expect(separationKm(vilnius, stale)).toBeLessThan(1100);
  });

  it('stays under the warning threshold for a site across town', () => {
    const acrossTown = { latitude: 54.71, longitude: 25.33 };
    expect(separationKm(vilnius, acrossTown)).toBeLessThan(25);
  });

  it('is symmetric', () => {
    const other = { latitude: 12.5, longitude: -70.1 };
    expect(separationKm(vilnius, other)).toBeCloseTo(separationKm(other, vilnius), 9);
  });

  it('does not produce NaN at antipodes, where the naive arcsin overflows', () => {
    const antipode = { latitude: -vilnius.latitude, longitude: vilnius.longitude - 180 };
    const d = separationKm(vilnius, antipode);
    expect(Number.isNaN(d)).toBe(false);
    expect(d).toBeCloseTo(20015, 0);
  });
});
