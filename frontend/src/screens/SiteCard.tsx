import { useState } from 'react';
import type { ReactNode } from 'react';

import { getJson } from '../lib/api';
import { useTokenStore } from '../store/token';
import { Button } from '../ui/Button';
import { Card, Field } from '../ui/Card';

/**
 * Where the node thinks it is, next to where the phone says it is.
 *
 * Every horizontal coordinate the system computes — the altitude limit that refuses a slew, the
 * alt/az on the pointing readout, the sidereal time behind every hour angle — is derived from
 * `site` in the config. Nothing measures it. So a node shipped with the example's Oslo defaults
 * will happily judge a mount in Lithuania against a horizon 700 km away, and *nothing in the
 * system can notice*: the arithmetic is correct, its input is fiction.
 *
 * The operator's phone knows. This card is the one place those two numbers can be seen together,
 * which is all it takes for the mismatch to become obvious rather than mysterious — the real case
 * that motivated it went undiagnosed until a tube pointed 2° from where it should have.
 *
 * # Why it does not write the config
 *
 * SDD §4.4: configuration is loaded and validated once at startup and nothing re-reads the file. A
 * node that mutated its own site at runtime would make "what was this session computed against?"
 * unanswerable after the fact — the sort of question a failed night turns on. So this prints a YAML
 * block to paste, and the operator restarts the node deliberately.
 *
 * # Why the fix is not automatic
 *
 * Geolocation is asked for on a tap, never on load. It is a permission prompt and a privacy
 * boundary, and an app that reaches for it unbidden while the operator is looking at a telescope
 * has earned the refusal it will get.
 */
export function SiteCard(): ReactNode {
  const token = useTokenStore((state) => state.token);
  const [configured, setConfigured] = useState<Site | null>(null);
  const [phone, setPhone] = useState<Fix | null>(null);
  const [problem, setProblem] = useState<string | null>(null);

  const readNode = () => {
    void getJson<{ config?: { site?: Site } }>('/api/system/info', token).then((result) => {
      if (result.ok) setConfigured(result.value.config?.site ?? null);
      else setProblem(result.failure.message);
    });
  };

  const readPhone = () => {
    setProblem(null);
    if (!('geolocation' in navigator)) {
      setProblem('This browser will not share a position.');
      return;
    }
    navigator.geolocation.getCurrentPosition(
      (position) => {
        setPhone({
          latitude: position.coords.latitude,
          longitude: position.coords.longitude,
          elevation: position.coords.altitude,
          accuracy_m: position.coords.accuracy,
        });
        readNode();
      },
      (error) => {
        // The refusal is a normal answer, not a fault: an operator may simply not want to share it.
        setProblem(
          error.code === error.PERMISSION_DENIED
            ? 'Position sharing was declined. The node keeps the site in its config file.'
            : `The phone could not fix a position: ${error.message}`,
        );
      },
      { enableHighAccuracy: true, timeout: 15_000, maximumAge: 60_000 },
    );
  };

  const drift = configured && phone ? separationKm(configured, phone) : null;

  return (
    <Card title="Site">
      <p className="mb-3 text-sm text-muted">
        Every horizon, altitude and sidereal time comes from this. Nothing measures it — if the
        configured site is wrong, the numbers stay self-consistent and wrong together.
      </p>

      <dl>
        {configured !== null && (
          <Field
            label="node config"
            value={`${configured.latitude.toFixed(4)}, ${configured.longitude.toFixed(4)} · ${configured.timezone}`}
          />
        )}
        {phone !== null && (
          <Field
            label="this phone"
            value={`${phone.latitude.toFixed(4)}, ${phone.longitude.toFixed(4)} · ±${Math.round(phone.accuracy_m)} m`}
          />
        )}
        {drift !== null && (
          <Field label="apart" value={drift < 1 ? 'under a kilometre' : `${Math.round(drift)} km`} />
        )}
      </dl>

      {drift !== null && drift > 25 && (
        <p role="status" className="mt-3 rounded-md border border-warn/50 bg-overlay p-2 text-sm text-fg">
          <span aria-hidden="true" className="mr-1 text-warn">
            !
          </span>
          The node is computing horizons for somewhere {Math.round(drift)} km away. Altitude limits
          and alt/az are wrong by that much, and nothing else will tell you.
        </p>
      )}

      {phone !== null && (
        <pre className="mt-3 overflow-x-auto rounded-md border border-edge bg-overlay p-2 font-mono text-xs text-fg">
          {yamlFor(phone)}
        </pre>
      )}

      {problem !== null && (
        <p role="status" className="mt-2 text-sm text-muted">
          {problem}
        </p>
      )}

      <div className="mt-3 flex flex-wrap items-center gap-2">
        <Button onClick={readPhone}>Compare with this phone</Button>
        <Button onClick={readNode}>Read node config</Button>
      </div>
    </Card>
  );
}

interface Site {
  latitude: number;
  longitude: number;
  elevation: number;
  timezone: string;
}

interface Fix {
  latitude: number;
  longitude: number;
  /** Phones frequently have no altitude fix; the node's own elevation is the better guess then. */
  elevation: number | null;
  accuracy_m: number;
}

/** The paste-ready block, so the operator edits config rather than trusting a silent write. */
function yamlFor(fix: Fix): string {
  const elevation = fix.elevation === null ? '  # elevation: unchanged — this phone had no fix' : `  elevation: ${Math.round(fix.elevation)}`;
  return [
    'site:',
    `  latitude: ${fix.latitude.toFixed(4)}`,
    `  longitude: ${fix.longitude.toFixed(4)}`,
    elevation,
    '  # timezone affects displayed local time only; everything internal is UTC',
  ].join('\n');
}

/**
 * Great-circle distance, in kilometres.
 *
 * Haversine rather than a flat approximation: the interesting case is a node still carrying the
 * example config's Oslo defaults, which is hundreds of kilometres from anywhere it is deployed,
 * and a flat-earth formula is worst exactly there.
 */
export function separationKm(a: { latitude: number; longitude: number }, b: { latitude: number; longitude: number }): number {
  const toRad = (deg: number) => (deg * Math.PI) / 180;
  const dLat = toRad(b.latitude - a.latitude);
  const dLon = toRad(b.longitude - a.longitude);
  const h =
    Math.sin(dLat / 2) ** 2 +
    Math.cos(toRad(a.latitude)) * Math.cos(toRad(b.latitude)) * Math.sin(dLon / 2) ** 2;
  return 6371 * 2 * Math.asin(Math.min(1, Math.sqrt(h)));
}
