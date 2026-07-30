import type { RequestResult } from './api';
import { getJson, postJson, putJson } from './api';
import type { CameraStatus, MountStatus } from './link/protocol';

/*
 * Mount and camera commands — SDD §5.8.1's mount and camera rows.
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
 *
 * **Each renewal is a new command with a new `command_id`** (M1-T10), which `postJson` gives it
 * without anything here asking. That is not an implementation detail: a renewal that reused the
 * previous id would be replayed out of the node's idempotency ledger, the lease would never be
 * extended, and the dead-man's switch would stop the axis half a second into a hold the operator
 * is still performing. "Identical parameters extend the deadline" is a property of the *lease*,
 * decided in `SafeMount`; identical `command_id`s would mean the second request never got there.
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
 * Stop everything, now — SDD §5.8.2, MNT-08, REL-01, PRF-12.
 *
 * `keepalive` for the same reason `mountSlewStop` uses it, only more so: the request must survive
 * the document going away. An operator who hits stop and then puts the phone in their pocket, or
 * whose browser tab is killed by Android the moment after the tap, has still hit stop.
 *
 * No body. The route accepts an empty one deliberately (§5.8.2: "auth only, no JSON parsing"), and
 * sending nothing means there is no serialisation step between the operator's finger and the
 * socket, and nothing that can make the request malformed.
 *
 * **No envelope either** (`envelope: false`, M1-T10). The node declares this route `exempt` rather
 * than merely `stopping`: it reads nothing off the request, so there is no header, no timestamp
 * and no command id whose absence or malformation could put a 4xx in front of a stop. Sending them
 * anyway would work — they would be ignored — and would leave the next person to read this file
 * believing the e-stop needs something. It needs nothing.
 *
 * The reply is **not** evidence that the mount stopped — only that the node accepted the request.
 * What the telescope did arrives on the event stream, like everything else (§5.9). The header
 * button is written against that distinction; see `app/EStopButton.tsx`.
 */
export function mountEstop(token: string | null): Promise<RequestResult<unknown>> {
  return postJson('/api/mount/estop', token, undefined, { keepalive: true, envelope: false });
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

// ---------------------------------------------------------------------------------------------
// Camera — SDD §5.8.1's camera rows, §5.3.2's flow (M1-T08)
// ---------------------------------------------------------------------------------------------

/**
 * The capture format tokens, exactly as the node spells them.
 *
 * `RAW+JPEG` has a `+` in it and that is the wire value, not a display string: `astroctl-core`'s
 * `ImageFormat` renames the variant to it. Prettifying it here would produce a body the node
 * rejects.
 */
export const IMAGE_FORMATS = ['RAW', 'JPEG', 'RAW+JPEG'] as const;
export type ImageFormat = (typeof IMAGE_FORMATS)[number];

/**
 * `GET /api/camera/settings` — the settings in force plus every value the body will accept.
 *
 * The settings are flattened into the top level and the lists are nested under `available`,
 * which is how SDD §5.8.1 writes the row. ISO, shutter and aperture are **the camera's own
 * tokens** rather than numbers (`"1600"`, `"1/250"`, `"30"`, `"bulb"`, `"5.6"`): parsing them into
 * numbers here would mean rendering them back to speak to the camera, and the round trip is where
 * `"1/250"` becomes `0.004` and then `4e-3`.
 */
export interface CameraSettings {
  v: number;
  iso: string;
  shutter: string;
  /** `null` on a body that reports none — a manual lens. */
  aperture: string | null;
  format: ImageFormat;
  available: {
    isos: string[];
    shutters: string[];
    /** Empty when the lens has no electronic aperture. */
    apertures: string[];
    formats: ImageFormat[];
  };
}

/** What `POST /api/camera/capture` answers — §5.8.1's `202 + WS progress`. */
export interface CaptureAccepted {
  correlation_id: string;
  /** The id this exposure will be stored under. */
  frame_id: string;
  /** What the node resolved the exposure to, in seconds. The countdown is measured against it. */
  exposure_s: number;
  watch_topic: string;
}

/** One frame in `GET /api/session/current` — the store's `FrameEntry`. */
export interface SessionFrame {
  frame_id: string;
  file_name: string;
  size_bytes: number;
  /**
   * `null` for a frame whose metadata write was interrupted.
   *
   * Not an error and not a reason to hide the frame: SDD §5.5 note 3 makes the frame durable
   * *before* its metadata, so a crash between the two leaves exactly this, and REL-05 says the
   * frame is still a frame.
   */
  quality: {
    exposure_s: number;
    sha256: string;
    size_bytes: number;
    settings: { iso: string; shutter: string; aperture: string | null; format: ImageFormat };
  } | null;
}

/** `GET /api/session/current` — SDD §5.8.1's "session.json view + frame list". */
export interface SessionView {
  session_id: string;
  created_ts: string;
  target: unknown;
  equipment: { telescope: string; camera: string; filter: string };
  /** Ids granted, which can exceed `frame_count`: a granted id is never reused (REL-04). */
  frames_reserved: number;
  frame_count: number;
  frames: SessionFrame[];
}

export function cameraConnect(token: string | null): Promise<RequestResult<CameraStatus | null>> {
  return postJson<CameraStatus>('/api/camera/connect', token);
}

export function cameraDisconnect(
  token: string | null,
): Promise<RequestResult<CameraStatus | null>> {
  return postJson<CameraStatus>('/api/camera/disconnect', token);
}

export function cameraSettings(token: string | null): Promise<RequestResult<CameraSettings>> {
  return getJson<CameraSettings>('/api/camera/settings', token);
}

/**
 * Start the camera's live-view stream (CAM-05, SDD §5.7).
 *
 * Answers `202`: the stream having *started* is not something the reply can promise, and §5.9
 * forbids the UI rendering a mutation from its own request anyway. The panel learns live view is
 * running the only honest way — a frame arrives on `/ws/liveview`.
 *
 * Idempotent server-side, so a second tap while it is already running is success rather than an
 * error the panel would have to explain.
 */
export function liveViewStart(token: string | null): Promise<RequestResult<null>> {
  return postJson<never>('/api/camera/liveview/start', token) as Promise<RequestResult<null>>;
}

/** Stop it, and close the server-side work with it. Idempotent, like `liveViewStart`. */
export function liveViewStop(token: string | null): Promise<RequestResult<null>> {
  return postJson<never>('/api/camera/liveview/stop', token) as Promise<RequestResult<null>>;
}

/**
 * Change one or more settings. Absent fields are left alone.
 *
 * A partial update rather than a whole-object replace, because the panel changes one selector at a
 * time and a replace would make changing the ISO re-send a shutter the client read a moment ago —
 * which over a tunnel is how one operator's change silently reverts another's.
 */
export function cameraSetSettings(
  token: string | null,
  update: Partial<{ iso: string; shutter: string; aperture: string; format: ImageFormat }>,
): Promise<RequestResult<CameraSettings | null>> {
  return putJson<CameraSettings>('/api/camera/settings', token, update);
}

/**
 * Start one exposure. Answers `202` and a frame id; the exposure itself arrives as events.
 *
 * `bulbSeconds` is for CAM-04's timed bulb. Omitting it uses the shutter setting — and if that
 * setting is `bulb`, the node refuses with `DEVICE_REJECTED` *before* the 202 rather than
 * accepting a capture that cannot run.
 */
export function cameraCapture(
  token: string | null,
  bulbSeconds?: number,
): Promise<RequestResult<CaptureAccepted | null>> {
  return postJson<CaptureAccepted>(
    '/api/camera/capture',
    token,
    bulbSeconds === undefined ? {} : { bulb_seconds: bulbSeconds },
  );
}

/**
 * Abandon the exposure in flight.
 *
 * `keepalive` for the reason the mount's stop path uses it: the most common way an operator stops
 * something is not a deliberate tap on a live screen, and a request issued as the document goes
 * away is cancelled with it. A stopping command is never refused for state — aborting when nothing
 * is running is a success.
 */
export function cameraAbort(token: string | null): Promise<RequestResult<unknown>> {
  return postJson('/api/camera/capture/abort', token, undefined, { keepalive: true });
}

/**
 * Clear the fault that is holding capture — SDD §5.6's "operator ack → Idle".
 *
 * The only thing that clears it. A retry gets the same refusal, which is the point: the state is
 * raised when the node can no longer be trusted to take the next frame, and a retry loop must not
 * be able to paper over a disk that is failing.
 */
export function cameraAcknowledgeFault(
  token: string | null,
): Promise<RequestResult<{ cleared: boolean; code: string | null; message: string | null } | null>> {
  return postJson('/api/camera/fault/ack', token);
}

/**
 * The session and its frames.
 *
 * A document, not telemetry — which is why it is a request rather than a slot in the event store.
 * SDD §5.9's "no REST polling" rule is about the values that say what the observatory is *doing*;
 * a frame list is a directory listing, it has no cadence, and `frame.saved` is the event that says
 * when to read it again. Nothing here is on a timer.
 */
export function sessionCurrent(token: string | null): Promise<RequestResult<SessionView>> {
  return getJson<SessionView>('/api/session/current', token);
}
