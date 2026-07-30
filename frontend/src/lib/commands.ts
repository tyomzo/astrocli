import type { RequestResult } from './api';
import { postJson } from './api';
import type { MountStatus } from './link/protocol';

/*
 * Mount commands — SDD §5.8.1's mount rows.
 *
 * # This module cannot change what the UI shows
 *
 * It imports nothing from `store/`, and `commands.test.ts` asserts that by reading this file's
 * source. That is the mechanical half of SDD §5.9's "commands optimistically do nothing"; the
 * other half is that the telemetry store has no action a command could dispatch. Between them
 * there is no path from "the operator pressed goto" to "the display moved" that does not go
 * through an event from the node.
 *
 * The rule is worth the enforcement because breaking it is *comfortable*. Making the coordinates
 * update the instant goto is pressed feels responsive on a desk. Over a tunnel where the request
 * may not have arrived, it means the app is showing a position the mount never went to, at the
 * moment the operator most needs to know where the telescope actually is.
 *
 * What a command may do is report **its own outcome** — a 403 `LIMIT_ALTITUDE` is the node's
 * answer to this request, not a claim about the mount, and the panel that issued it renders it
 * locally. The distinction is the whole discipline: state comes from events, replies come from
 * replies.
 */

export type TrackingMode = 'sidereal' | 'lunar' | 'solar' | 'off';

/** `ra` is the hour-angle axis, `dec` the declination axis — the two SDD §5.8.1 addresses. */
export type SlewAxis = 'ra' | 'dec';

/**
 * Which way along the axis.
 *
 * `positive`/`negative` rather than `north`/`east`: the axis names the pair, and a compass word
 * on the RA axis would have to mean "the direction in which right ascension increases", which is
 * east — one indirection nobody reading a bug report wants to perform.
 */
export type SlewDirection = 'positive' | 'negative';

/**
 * Manual slew speeds, 1 (finest) to 5 (fastest) — the five dots of the SDD §5.9 sketch.
 *
 * An ordinal, not a rate. The mapping from index to degrees per second belongs to the driver,
 * which is the only thing that knows what the mount can do; a UI that sent °/s would be asserting
 * a capability it cannot check.
 */
export const SLEW_SPEEDS = [1, 2, 3, 4, 5] as const;
export type SlewSpeed = (typeof SLEW_SPEEDS)[number];

/**
 * The dead-man's-switch lease, milliseconds — SDD §5.8.1's default, clamped to 2000 server-side.
 *
 * Renewal is at half the TTL: one lost packet then costs a stutter rather than a stop, and the
 * axis still halts within `ttl` of the operator's finger lifting if the app dies mid-hold.
 */
export const SLEW_TTL_MS = 500;
export const SLEW_RENEW_MS = SLEW_TTL_MS / 2;

/** `202` from any long-running action — §5.8.1's "202 + WS progress" pattern. */
export interface Accepted {
  correlation_id: string;
  watch_topic: string;
}

export function mountConnect(token: string | null): Promise<RequestResult<MountStatus | null>> {
  return postJson<MountStatus>('/api/mount/connect', token);
}

export function mountDisconnect(token: string | null): Promise<RequestResult<MountStatus | null>> {
  return postJson<MountStatus>('/api/mount/disconnect', token);
}

/**
 * Slew to a target. Answers `202` and a topic to watch; the mount moves on the event stream.
 *
 * Units are in the wire field names (`ra_hours`, `dec_degrees`) because SDD §2 and the core
 * newtypes exist to stop exactly the mix-up that a bare `ra` invites.
 */
export function mountGoto(
  token: string | null,
  target: { raHours: number; decDegrees: number },
): Promise<RequestResult<Accepted | null>> {
  return postJson<Accepted>('/api/mount/goto', token, {
    ra_hours: target.raHours,
    dec_degrees: target.decDegrees,
  });
}

export function mountTracking(
  token: string | null,
  mode: TrackingMode,
): Promise<RequestResult<MountStatus | null>> {
  return postJson<MountStatus>('/api/mount/tracking', token, { mode });
}

/**
 * Authorise motion on one axis for `SLEW_TTL_MS`.
 *
 * One call is not a slew — it is a lease. Holding the D-pad means re-sending this with identical
 * parameters every `SLEW_RENEW_MS`; silence means stop. See `slewHold.ts` for the loop and
 * §5.8.1 for why the server treats an identical repeat as a deadline extension rather than a new
 * motor command.
 */
export function mountSlew(
  token: string | null,
  lease: { axis: SlewAxis; direction: SlewDirection; speed: SlewSpeed },
): Promise<RequestResult<unknown>> {
  return postJson('/api/mount/slew', token, {
    axis: lease.axis,
    direction: lease.direction,
    speed: lease.speed,
    ttl_ms: SLEW_TTL_MS,
  });
}

/**
 * Stop one axis, or every axis when `axis` is omitted.
 *
 * `keepalive` because the most common release is not a finger lifting: it is the screen locking,
 * the tab hiding, or the operator switching apps mid-hold. A plain `fetch` issued from a
 * `pagehide` handler is cancelled with the document, which would leave the axis turning until the
 * TTL expired. TTL expiry is the backstop (§5.8.1), and a backstop that runs on every release is
 * a design that has given up.
 */
export function mountSlewStop(
  token: string | null,
  axis?: SlewAxis,
): Promise<RequestResult<MountStatus | null>> {
  return postJson<MountStatus>(
    '/api/mount/slew/stop',
    token,
    axis === undefined ? {} : { axis },
    { keepalive: true },
  );
}
