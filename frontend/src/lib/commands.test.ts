import { describe, expect, it } from 'vitest';

// `?raw` rather than `node:fs`: reading the file through Vite's own pipeline keeps Node's type
// declarations out of a browser app, where an accidental `Buffer` or `process` should not compile.
import commandsSource from './commands.ts?raw';
import { nudgeAvailability } from './nudge';
import type { LinkPhase, Slot } from '../store/telemetry';
import type { MountStatus } from './link/protocol';

/**
 * The structural half of SDD §5.9's "commands optimistically do nothing".
 *
 * The reducer half is that `store/telemetry.ts` has no action a command could dispatch. This is
 * the other half, and it is a source-reading test on purpose — the same shape as the colour grep
 * in `frontend/README.md`. A rule that is only written in a comment is a rule that survives until
 * the first hurry; a rule with a test fails the build.
 *
 * What it forbids is narrow and deliberate: the command layer may not import the store. It may
 * still return a failure for a panel to render, because a refusal is the reply to *this request*
 * and not a claim about where the telescope is.
 */
describe('command layer isolation', () => {
  it('imports nothing from the store', () => {
    const imports = [...commandsSource.matchAll(/^\s*import[^;]*from\s+'([^']+)'/gm)].map(
      (match) => match[1] ?? '',
    );

    expect(imports.filter((specifier) => specifier.includes('store'))).toEqual([]);
    expect([...new Set(imports)].sort()).toEqual(['./api', './link/protocol']);
  });

  it('does not reach the store through a side-effect import either', () => {
    expect(commandsSource).not.toMatch(/import\s+'[^']*store/);
    expect(commandsSource).not.toMatch(/require\(/);
  });
});

/*
 * The two stop paths carry `keepalive` — SDD §8.3(5), M1-T05.
 *
 * The most common way either of them is issued is not a finger lifting: it is the phone locking,
 * the tab hiding, or the operator switching apps. A plain `fetch` started as the document goes
 * away is cancelled with it, and for the e-stop that means the telescope keeps moving after the
 * operator believes they stopped it. Asserted on the source because the flag has no observable
 * effect in a test environment — it only matters in the moment the page is being torn down, which
 * is precisely the moment nothing is watching.
 */
describe('the stop paths', () => {
  it('send slew/stop and estop with keepalive', () => {
    for (const command of ['mountSlewStop', 'mountEstop']) {
      const body = commandsSource.slice(commandsSource.indexOf(`export function ${command}`));
      const end = body.indexOf('\n}\n');
      expect(body.slice(0, end), `${command} must survive the document going away`).toContain(
        'keepalive: true',
      );
    }
  });

  it('sends the e-stop with no body, because the route parses none', () => {
    // SDD §5.8.2: "auth only, no JSON parsing — empty body accepted". Sending nothing means there
    // is no serialisation step between the operator's finger and the socket.
    expect(commandsSource).toMatch(/postJson\('\/api\/mount\/estop', token, undefined/);
  });
});

/*
 * Nudge availability — SDD §5.9's badge states. Pure, so it is tested where the sentences live
 * rather than through a rendered badge.
 */

const LIVE: LinkPhase = { phase: 'live', since: 0 };

function mount(state: MountStatus['state'], slewing = false): Slot<MountStatus> {
  return {
    state: 'observed',
    at: 0,
    ts: '2026-07-30T21:04:05.000Z',
    value: { state, tracking: false, tracking_mode: null, slewing, parked: state === 'parked' },
  };
}

describe('nudge availability', () => {
  it('is available only with a live link and an idle connected mount', () => {
    expect(nudgeAvailability(LIVE, mount('idle'))).toEqual({ available: true });
  });

  it('is unavailable with a dead link, whatever the mount last said', () => {
    const result = nudgeAvailability({ phase: 'idle' }, mount('idle'));
    expect(result.available).toBe(false);
  });

  it('explains each unavailable case rather than returning a bare false', () => {
    for (const slot of [mount('disconnected'), mount('parked'), mount('fault'), mount('slewing', true)]) {
      const result = nudgeAvailability(LIVE, slot);
      expect(result.available).toBe(false);
      // §5.9: tapping an unavailable badge must explain why. An empty reason is a badge that
      // does nothing, which is the behaviour the requirement exists to forbid.
      expect(!result.available && result.reason.length).toBeGreaterThan(20);
    }
  });

  it('refuses while a goto is in flight', () => {
    const result = nudgeAvailability(LIVE, mount('slewing', true));
    expect(!result.available && result.reason).toContain('goto');
  });
});
