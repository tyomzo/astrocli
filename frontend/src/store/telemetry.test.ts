import { describe, expect, it } from 'vitest';

import type { TelemetryEvent } from '../lib/link/protocol';
import { parseEvent } from '../lib/link/protocol';
import type { LinkAction, TelemetryState } from './telemetry';
import { EMPTY, reduce } from './telemetry';

/*
 * The reducer, tested where the SDD makes a promise.
 *
 * Each case below is one sentence from §5.9 or §5.8.3 that a plausible refactor would break
 * silently — the resnapshot rule especially, because merging is the smaller diff and the failure
 * only shows up as a coordinate that is quietly an hour old.
 */

const at = 1_000_000;

function event(topic: string, data: unknown, ts = '2026-07-30T21:04:05.123Z'): TelemetryEvent {
  const parsed = parseEvent({ v: 1, ts, topic, data });
  if (parsed === null) throw new Error(`fixture for ${topic} does not parse`);
  return parsed;
}

const POSITION = event('mount.position', {
  ra: 5.588,
  dec: -5.391,
  alt: 47.2,
  az: 128.4,
  pier_side: 'west',
});

const STATUS = event('mount.status', {
  state: 'idle',
  tracking: true,
  slewing: false,
  parked: false,
});

const CAMERA = event('camera.status', {
  connected: false,
  battery_pct: null,
  charging: false,
  storage_free_mb: null,
});

function apply(state: TelemetryState, ...actions: LinkAction[]): TelemetryState {
  return actions.reduce(reduce, state);
}

describe('snapshot', () => {
  it('makes the link live and applies every event it carried', () => {
    const state = apply(EMPTY, { type: 'link/snapshot', at, events: [STATUS, POSITION] });

    expect(state.link).toEqual({ phase: 'live', since: at });
    expect(state.mountPosition).toEqual({
      state: 'observed',
      at,
      ts: POSITION.ts,
      value: POSITION.data,
    });
    expect(state.mountStatus.state).toBe('observed');
  });

  it('replaces rather than merges: a topic missing from a resnapshot returns to unknown', () => {
    // §5.9: "the store must resnapshot rather than resume from a hole". The mount was
    // disconnected while the client was away, so the node no longer has a position to report —
    // and the operator must not be shown the one from before the drop.
    const first = apply(EMPTY, { type: 'link/snapshot', at, events: [STATUS, POSITION, CAMERA] });
    expect(first.mountPosition.state).toBe('observed');

    const second = apply(first, { type: 'link/snapshot', at: at + 5000, events: [CAMERA] });

    expect(second.mountPosition).toEqual({ state: 'unknown' });
    expect(second.mountStatus).toEqual({ state: 'unknown' });
    expect(second.cameraStatus.state).toBe('observed');
  });

  it('keeps alerts across a resnapshot — they are occurrences, not state', () => {
    const alerted = apply(EMPTY, {
      type: 'link/event',
      at,
      event: event('alert', {
        severity: 'warning',
        code: 'SLEW_TTL_EXPIRED',
        message: 'no renewal',
      }),
    });
    const resnapshotted = apply(alerted, { type: 'link/snapshot', at: at + 1000, events: [] });

    expect(resnapshotted.alerts).toHaveLength(1);
    expect(resnapshotted.alerts[0]?.value.code).toBe('SLEW_TTL_EXPIRED');
  });
});

describe('events', () => {
  it('records the arrival instant and the node timestamp separately', () => {
    const state = apply(EMPTY, { type: 'link/event', at, event: POSITION });

    expect(state.mountPosition).toMatchObject({ at, ts: '2026-07-30T21:04:05.123Z' });
  });

  it('keeps only the newest alerts, newest first', () => {
    let state = EMPTY;
    for (let index = 0; index < 40; index += 1) {
      state = apply(state, {
        type: 'link/event',
        at: at + index,
        event: event('alert', { severity: 'info', code: 'X', message: `alert ${index}` }),
      });
    }

    expect(state.alerts).toHaveLength(32);
    expect(state.alerts[0]?.value.message).toBe('alert 39');
    // Sequence numbers keep rising even as the list is trimmed, so a React key stays unique.
    expect(state.alerts[0]?.seq).toBe(40);
  });

  it('ignores per-frame topics until the tasks that own them land', () => {
    const before = apply(EMPTY, { type: 'link/snapshot', at, events: [POSITION] });
    const after = apply(before, {
      type: 'link/event',
      at: at + 1,
      event: event('frame.saved', {
        frame_id: 'light_00001',
        path: '/srv/f.cr3',
        size_bytes: 1,
        sha256: 'ab',
      }),
    });

    expect(after).toEqual(before);
  });
});

describe('link phases', () => {
  it('does not erase telemetry when the link drops', () => {
    // The last known position stays so it can be rendered with its age; what changes is the
    // phase, which is how every readout knows not to present it as current.
    const live = apply(EMPTY, { type: 'link/snapshot', at, events: [POSITION] });
    const dropped = apply(live, {
      type: 'link/retrying',
      at: at + 100,
      attempt: 1,
      retryAt: at + 600,
      failure: { kind: 'transport', message: 'closed' },
    });

    expect(dropped.mountPosition).toEqual(live.mountPosition);
    expect(dropped.link.phase).toBe('retrying');
  });

  it('treats an open socket as syncing, not live, until the snapshot arrives', () => {
    const open = apply(EMPTY, { type: 'link/open', at, attempt: 1 });

    expect(open.link.phase).toBe('syncing');
  });

  it('holds unauthorized until the credential changes', () => {
    const refused = apply(EMPTY, { type: 'link/unauthorized', at, message: 'bad token' });
    expect(refused.link).toEqual({ phase: 'unauthorized', at, message: 'bad token' });

    // Nothing but a new session clears it — there is no action that retries out of this phase.
    expect(apply(refused, { type: 'session/reset' })).toEqual(EMPTY);
  });

  it('keeps the measured RTT across a snapshot, since it describes the link and not a topic', () => {
    const state = apply(
      EMPTY,
      { type: 'link/rtt', rttMs: 42 },
      { type: 'link/snapshot', at, events: [POSITION] },
    );

    expect(state.rttMs).toBe(42);
  });
});
