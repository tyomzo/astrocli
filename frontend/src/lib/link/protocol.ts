/*
 * The `/ws` wire, in TypeScript — SDD §4.3 (event schema), §4.2 (error codes), §5.8.3 (hub).
 *
 * # This file mirrors Rust; it does not invent a parallel schema
 *
 * Every type here has a counterpart in `crates/astroctl-core/src/event.rs` or `error.rs`, and the
 * **field names are the wire names**, snake_case included. Camel-casing them here would be more
 * idiomatic TypeScript and would cost the one property that matters: that a reviewer can put this
 * file beside the Rust struct and see, line for line, whether they agree. The schema is frozen
 * (SDD §2 gives every externally visible JSON schema a version); a divergence is a defect, not a
 * style difference, and it should look like one.
 *
 * # Nothing is trusted
 *
 * `data` arrives as `unknown` and every payload is narrowed by an explicit parser. A malformed or
 * unrecognised frame is dropped, never rendered: this app's whole claim is that what it shows is
 * what the node said, so a field it could not read is a field it must not guess at. Unknown
 * *topics* are dropped for the same reason plus one more — a node newer than this bundle is a
 * normal condition after an upgrade, and it must not break the panel that reports the mount.
 *
 * # The two frame shapes on this socket
 *
 * SDD §4.3 requires the WS frame and the session-log line to be the *identical* serialization of
 * `Event`, so an event frame is exactly `{v, ts, topic, data}` and can carry no envelope. §5.8.3
 * additionally requires a snapshot on connect and a `ping` answer, neither of which is an event.
 * They are therefore control frames carrying `type` instead of `topic`, and the two are told apart
 * by which of those keys is present. `mock/README.md` records this as the contract M1-T03
 * implements; it is the one part of this file the SDD does not already pin down.
 */

/** SDD §4.3: every event carries `v`, and this build understands version 1. */
export const EVENT_SCHEMA_VERSION = 1;

// ---------------------------------------------------------------------------------------------
// Enums (astroctl-core/src/event.rs)
// ---------------------------------------------------------------------------------------------

export type PierSide = 'east' | 'west' | 'unknown';

export type MountState = 'disconnected' | 'idle' | 'slewing' | 'parked' | 'fault';

export type CaptureState = 'exposing' | 'downloading' | 'saved' | 'preview_ready';

export type TransferState = 'idle' | 'uploading' | 'offline';

export type WorkerState = 'starting' | 'ready' | 'busy' | 'restarting' | 'failed';

export type AlertSeverity = 'info' | 'warning' | 'critical';

/**
 * The closed `ErrorCode` set of SDD §4.2 — 27 codes, the order of `ErrorCode::ALL`.
 *
 * Exported as a value as well as a type because the UI switches on these strings and a typo in a
 * `case` label is otherwise silent.
 */
export const ERROR_CODES = [
  'NOT_CONNECTED',
  'UNSUPPORTED',
  'BUSY',
  // A motion that had started was stopped before it finished (SDD §4.2 change note, M1-T03).
  // A goto interrupted by an e-stop used to answer `DEVICE_REJECTED`/422, which told the
  // operator their request was malformed at the moment their emergency stop worked. 409, and
  // not retryable — re-issuing the goto would drive the mount back into whatever stopped it.
  'ABORTED',
  // REL-02's watchdog verdict, added by M1-T17 (§4.2, 27 codes): the mount has missed
  // `mount.serial.heartbeat_misses` consecutive polls. Alert-only — no route returns it — and it
  // is the one alert code whose message may say the tube was slewing when contact was lost, which
  // is what turns "watch the badge" into "go outside".
  'MOUNT_LINK_LOST',
  'MOUNT_TIMEOUT',
  'CAMERA_TIMEOUT',
  'DEVICE_TIMEOUT',
  'DEVICE_TRANSPORT',
  'DEVICE_PROTOCOL',
  // The *other node* is not answering — the `/stack/*` proxy (ADR-07) and the transfer agent.
  // §4.2 has specified this since change note 1.9.0 and nothing produced it, so the proxy
  // answered `DEVICE_TRANSPORT`: the exact conflation the row exists to prevent, which tells the
  // operator to check a cable when the problem is a tunnel. Added by M1-T14 (§4.2, 26 codes).
  'NODE_UNREACHABLE',
  'DEVICE_REJECTED',
  'VALIDATION',
  'COMMAND_STALE',
  'CHECKSUM_MISMATCH',
  'LIMIT_ALTITUDE',
  'LIMIT_MERIDIAN',
  'SLEW_TTL_EXPIRED',
  'AUTH',
  'FRAME_ID_CONFLICT',
  'DISK_FULL',
  'NOT_FOUND',
  // The worker vocabulary (SDD §4.2 change note, 2026-07-30) — added so supervisor failures do
  // not collapse onto INTERNAL: a crashed worker being restarted and a bug are different walks.
  'CANCELLED',
  'WORKER_UNAVAILABLE',
  'WORKER_CRASHED',
  'WORKER_TIMEOUT',
  'INTERNAL',
] as const;

export type ErrorCode = (typeof ERROR_CODES)[number];

// ---------------------------------------------------------------------------------------------
// Payloads (one per SDD §4.3 topic)
// ---------------------------------------------------------------------------------------------

/**
 * `mount.position`.
 *
 * `alt`/`az` are populated as of M1-T05, by the same topocentric transform the node's altitude
 * limit uses — so the number on this screen and the number a slew is refused on cannot disagree.
 *
 * They stay nullable, and that is not defensive typing left over from before. SDD §4.3 declares
 * them nullable, and the reason survives: rendering a missing altitude as 0° would put the target
 * exactly on the horizon, which is exactly where the altitude limit lives. An absent number must
 * look absent.
 */
export interface MountPosition {
  ra: number;
  dec: number;
  alt: number | null;
  az: number | null;
  pier_side: PierSide;
}

/**
 * `mount.status`.
 *
 * `tracking_mode` is the rate the mount settled on, and it is `null` whenever `tracking` is
 * `false`. It exists because the pair `{tracking: true}` alone could not say *which* rate was
 * running, so `TrackingControl` could show that the mount was tracking and had to leave every
 * rate button unhighlighted rather than guess from the last button pressed — which would have
 * been wrong after any reconnect. Added to the §4.3 payload by M1-T03 at M1-T04's request.
 *
 * Kept alongside `tracking` rather than replacing it: most of the UI binds to the boolean, and a
 * `null` check is a worse thing to ask of every consumer.
 */
export interface MountStatus {
  state: MountState;
  tracking: boolean;
  tracking_mode: TrackingRate | null;
  slewing: boolean;
  parked: boolean;
}

/**
 * The rates a mount can track at — `astroctl_core::types::TrackingMode`.
 *
 * Not the same set as the tracking *command*'s `mode`, which also accepts `"off"`. Off is not a
 * rate: the HAL splits starting and stopping into two methods, and `tracking_mode: null` is how
 * the event says the drive is not running.
 */
export type TrackingRate = 'sidereal' | 'lunar' | 'solar';

export interface CameraStatus {
  connected: boolean;
  battery_pct: number | null;
  charging: boolean;
  storage_free_mb: number | null;
}

export interface CaptureProgress {
  frame_id: string;
  state: CaptureState;
  elapsed_s: number;
}

export interface FrameSaved {
  frame_id: string;
  path: string;
  size_bytes: number;
  sha256: string;
}

export interface TransferAcked {
  frame_id: string;
  sha256: string;
  acked_at: string;
  queue_depth: number;
}

export interface TransferStatus {
  state: TransferState;
  queue_depth: number;
  oldest_queued_age_s: number | null;
  last_ack_ts: string | null;
}

export interface StackStatus {
  connected: boolean;
  session_frame_count: number;
  last_preview_ts: string | null;
  worker_state: WorkerState | null;
  restarts: number;
}

/**
 * `alert`.
 *
 * `code` is a bare string rather than [`ErrorCode`]: §4.3 says it is "the stable machine-readable
 * identifier the UI switches on", and the codes that reach it are a superset of the error set —
 * `astroctl-ipc` publishes alert-only codes such as `WORKER_PROTO_MISMATCH` and
 * `WORKER_RESTARTED` on this topic, and `DISK_LOW` is alert-only by design (§4.2). Typing it as
 * [`ErrorCode`] would have made a real alert unparseable. (Four worker codes *are* in the error
 * set since 2026-07-30, but the superset argument stands on the others.)
 */
export interface Alert {
  severity: AlertSeverity;
  code: string;
  message: string;
}

export interface SystemHealth {
  disk_free_gb: number;
  clock_synced: boolean;
  uptime_s: number;
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

/**
 * One parsed event: the topic and its payload, narrowed together.
 *
 * A discriminated union rather than `{topic: Topic, data: unknown}` so the store's reducer must
 * handle every topic to compile. A new row in the §4.3 table then breaks the build at the one
 * place that has to change, instead of being silently ignored at runtime.
 */
export type TelemetryEvent =
  | { topic: 'mount.position'; ts: string; data: MountPosition }
  | { topic: 'mount.status'; ts: string; data: MountStatus }
  | { topic: 'camera.status'; ts: string; data: CameraStatus }
  | { topic: 'capture.progress'; ts: string; data: CaptureProgress }
  | { topic: 'frame.saved'; ts: string; data: FrameSaved }
  | { topic: 'transfer.acked'; ts: string; data: TransferAcked }
  | { topic: 'transfer.status'; ts: string; data: TransferStatus }
  | { topic: 'stack.status'; ts: string; data: StackStatus }
  | { topic: 'alert'; ts: string; data: Alert }
  | { topic: 'system.health'; ts: string; data: SystemHealth };

export type Topic = TelemetryEvent['topic'];

/** The §4.3 table's topic order, which is also `Topic::ALL`'s. */
export const TOPICS: readonly Topic[] = [
  'mount.position',
  'mount.status',
  'camera.status',
  'capture.progress',
  'frame.saved',
  'transfer.acked',
  'transfer.status',
  'stack.status',
  'alert',
  'system.health',
];

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/**
 * A frame the hub sent, after parsing.
 *
 * `unreadable` is a real outcome rather than a thrown error: over a tunnel, on a node that may be
 * a version ahead, a frame this bundle cannot read is an ordinary event and must cost nothing more
 * than one dropped frame. It is distinct from a parse failure returning `null` because the
 * connection still counts it as traffic — a link delivering frames we cannot read is alive.
 */
export type LinkFrame =
  | { kind: 'snapshot'; ts: string; events: TelemetryEvent[] }
  | { kind: 'event'; event: TelemetryEvent }
  | { kind: 'pong'; id: number; serverTime: string | null }
  | { kind: 'unreadable'; why: string };

export function parseFrame(raw: string): LinkFrame {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return { kind: 'unreadable', why: 'frame is not JSON' };
  }
  if (!isRecord(value)) {
    return { kind: 'unreadable', why: 'frame is not a JSON object' };
  }

  // `topic` means an Event (§4.3's exact struct); `type` means a control frame. Nothing carries
  // both, and testing for `topic` first keeps the hot path — 1 Hz positions — one check long.
  if ('topic' in value) {
    const event = parseEvent(value);
    return event === null
      ? { kind: 'unreadable', why: `unreadable event on topic ${String(value['topic'])}` }
      : { kind: 'event', event };
  }

  switch (value['type']) {
    case 'snapshot': {
      const events = value['events'];
      if (!Array.isArray(events)) {
        return { kind: 'unreadable', why: 'snapshot has no events array' };
      }
      return {
        kind: 'snapshot',
        ts: str(value['ts']) ?? '',
        // A snapshot entry this build cannot read is dropped, not fatal: the rest of the picture
        // is still better than no picture, and the topic simply stays unknown.
        events: events.map(parseEvent).filter((event): event is TelemetryEvent => event !== null),
      };
    }
    case 'pong':
      return {
        kind: 'pong',
        id: num(value['id']) ?? -1,
        serverTime: str(value['server_time']),
      };
    default:
      return { kind: 'unreadable', why: `unknown frame type ${String(value['type'])}` };
  }
}

/** One `{v, ts, topic, data}` object → a narrowed event, or `null` if it cannot be read. */
export function parseEvent(value: unknown): TelemetryEvent | null {
  if (!isRecord(value)) return null;
  const ts = str(value['ts']);
  const data = value['data'];
  if (ts === null || !isRecord(data)) return null;

  switch (value['topic']) {
    case 'mount.position': {
      const ra = num(data['ra']);
      const dec = num(data['dec']);
      const pier = oneOf(data['pier_side'], ['east', 'west', 'unknown'] as const);
      if (ra === null || dec === null || pier === null) return null;
      return {
        topic: 'mount.position',
        ts,
        data: { ra, dec, alt: num(data['alt']), az: num(data['az']), pier_side: pier },
      };
    }
    case 'mount.status': {
      const state = oneOf(data['state'], [
        'disconnected',
        'idle',
        'slewing',
        'parked',
        'fault',
      ] as const);
      const tracking = bool(data['tracking']);
      const slewing = bool(data['slewing']);
      const parked = bool(data['parked']);
      if (state === null || tracking === null || slewing === null || parked === null) return null;
      // `tracking_mode` is narrowed but never required: a node older than this bundle does not
      // send it, and refusing the frame over a missing field would blank the mount panel against
      // a node that is working perfectly. Unreadable or absent both become `null`, which is the
      // same thing the UI shows for "tracking is off" — and the `tracking` boolean beside it is
      // what distinguishes the two.
      const tracking_mode = oneOf(data['tracking_mode'], [
        'sidereal',
        'lunar',
        'solar',
      ] as const);
      return {
        topic: 'mount.status',
        ts,
        data: { state, tracking, tracking_mode, slewing, parked },
      };
    }
    case 'camera.status': {
      const connected = bool(data['connected']);
      const charging = bool(data['charging']);
      if (connected === null || charging === null) return null;
      return {
        topic: 'camera.status',
        ts,
        data: {
          connected,
          charging,
          battery_pct: num(data['battery_pct']),
          storage_free_mb: num(data['storage_free_mb']),
        },
      };
    }
    case 'capture.progress': {
      const frameId = str(data['frame_id']);
      const state = oneOf(data['state'], [
        'exposing',
        'downloading',
        'saved',
        'preview_ready',
      ] as const);
      const elapsed = num(data['elapsed_s']);
      if (frameId === null || state === null || elapsed === null) return null;
      return {
        topic: 'capture.progress',
        ts,
        data: { frame_id: frameId, state, elapsed_s: elapsed },
      };
    }
    case 'frame.saved': {
      const frameId = str(data['frame_id']);
      const path = str(data['path']);
      const size = num(data['size_bytes']);
      const sha = str(data['sha256']);
      if (frameId === null || path === null || size === null || sha === null) return null;
      return {
        topic: 'frame.saved',
        ts,
        data: { frame_id: frameId, path, size_bytes: size, sha256: sha },
      };
    }
    case 'transfer.acked': {
      const frameId = str(data['frame_id']);
      const sha = str(data['sha256']);
      const ackedAt = str(data['acked_at']);
      const depth = num(data['queue_depth']);
      if (frameId === null || sha === null || ackedAt === null || depth === null) return null;
      return {
        topic: 'transfer.acked',
        ts,
        data: { frame_id: frameId, sha256: sha, acked_at: ackedAt, queue_depth: depth },
      };
    }
    case 'transfer.status': {
      const state = oneOf(data['state'], ['idle', 'uploading', 'offline'] as const);
      const depth = num(data['queue_depth']);
      if (state === null || depth === null) return null;
      return {
        topic: 'transfer.status',
        ts,
        data: {
          state,
          queue_depth: depth,
          oldest_queued_age_s: num(data['oldest_queued_age_s']),
          last_ack_ts: str(data['last_ack_ts']),
        },
      };
    }
    case 'stack.status': {
      const connected = bool(data['connected']);
      const count = num(data['session_frame_count']);
      const restarts = num(data['restarts']);
      if (connected === null || count === null || restarts === null) return null;
      return {
        topic: 'stack.status',
        ts,
        data: {
          connected,
          session_frame_count: count,
          restarts,
          last_preview_ts: str(data['last_preview_ts']),
          worker_state: oneOf(data['worker_state'], [
            'starting',
            'ready',
            'busy',
            'restarting',
            'failed',
          ] as const),
        },
      };
    }
    case 'alert': {
      const severity = oneOf(data['severity'], ['info', 'warning', 'critical'] as const);
      const code = str(data['code']);
      const message = str(data['message']);
      if (severity === null || code === null || message === null) return null;
      return { topic: 'alert', ts, data: { severity, code, message } };
    }
    case 'system.health': {
      const disk = num(data['disk_free_gb']);
      const synced = bool(data['clock_synced']);
      const uptime = num(data['uptime_s']);
      if (disk === null || synced === null || uptime === null) return null;
      return {
        topic: 'system.health',
        ts,
        data: { disk_free_gb: disk, clock_synced: synced, uptime_s: uptime },
      };
    }
    default:
      // A topic this bundle does not know — a newer node, which is normal after an upgrade.
      return null;
  }
}

// ---------------------------------------------------------------------------------------------
// Narrowing helpers. `null` means "absent or the wrong type", never a coerced value.
// ---------------------------------------------------------------------------------------------

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function str(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function bool(value: unknown): boolean | null {
  return typeof value === 'boolean' ? value : null;
}

function oneOf<T extends string>(value: unknown, allowed: readonly T[]): T | null {
  return typeof value === 'string' && (allowed as readonly string[]).includes(value)
    ? (value as T)
    : null;
}
