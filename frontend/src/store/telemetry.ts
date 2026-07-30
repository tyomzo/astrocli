import { create } from 'zustand';

import type { RequestFailure } from '../lib/api';
import type {
  Alert,
  CameraStatus,
  CaptureProgress,
  MountPosition,
  MountStatus,
  StackStatus,
  SystemHealth,
  TelemetryEvent,
  TransferStatus,
} from '../lib/link/protocol';

/*
 * Everything the node has told us, as a reducer over the event stream — SDD §5.9 "Store
 * discipline", §5.8.3 (snapshot), REL-10.
 *
 * This is the store `store/nodes.ts` promised M1-T04 would add, and it keeps that file's three
 * rules verbatim: nothing outside `dispatch` writes, only observations write, and every value
 * carries the instant it was observed at. Three more rules are specific to a socket-fed store and
 * are the ones worth reading before changing anything here.
 *
 * # 1. A command cannot reach this reducer
 *
 * SDD §5.9: "Commands are REST calls that optimistically do nothing: UI state changes only when
 * the corresponding event arrives." That is enforced structurally, in two layers:
 *
 *   * Every action below is a **`link/…` action** — something the socket observed. There is no
 *     `mount/slewing` or `tracking/set` for a command handler to dispatch, so writing optimistic
 *     UI would mean adding an action, which is a review-visible act rather than a one-line
 *     temptation.
 *   * `lib/commands.ts` **imports nothing from `store/`**, and `lib/commands.test.ts` asserts
 *     that by reading the file. A command layer that cannot see the store cannot write to it.
 *
 * The failure this prevents is specific: on a link where a request may not have landed,
 * optimistic UI does not merely look wrong, it tells the operator the mount is somewhere it is
 * not, which is the one thing this display exists to never do.
 *
 * # 2. A snapshot replaces; it never merges
 *
 * §5.9: if the hub drops this client as a slow consumer, "the store must resnapshot rather than
 * resume from a hole". `link/snapshot` therefore rebuilds the telemetry from empty and applies
 * only what the snapshot carried. A topic the node no longer has a value for — `mount.position`
 * after the mount was disconnected — goes back to `unknown` instead of leaving last night's
 * coordinates on screen. Merging would be the smaller diff and would produce exactly the stale
 * render the reconnect acceptance criterion is written to catch.
 *
 * # 3. A dropped link does not erase what was learned
 *
 * The last known position stays, because "RA 05:35:17, 40 s old, link down" is useful and a blank
 * panel is not (§8.3's telemetry age). What must never happen is rendering it as current — so the
 * link phase is part of this state, next to the values, and every readout that shows a value also
 * has the freshness of that value in hand. M1-T15 turns that into the confirmed/predicted/stale
 * treatment; M1-T04 owes it the honest inputs.
 */

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

/**
 * One topic's latest value, or the fact that there isn't one.
 *
 * `at` is wall-clock arrival (what "how old is this" is measured from) and `ts` is the node's own
 * timestamp from the event. Both, because they answer different questions and the difference
 * between them is the clock skew M1-T10 measures.
 */
export type Slot<T> = { state: 'unknown' } | { state: 'observed'; at: number; ts: string; value: T };

/**
 * The connection, as the UI needs to see it.
 *
 * `syncing` is separate from `live` on purpose: between the socket opening and the snapshot
 * arriving there is a window where the connection is up and the picture is not, and §5.9's "the
 * UI never renders from partial state" is precisely about that window.
 *
 * `unauthorized` is terminal by design. §5.9: "If the ticket request itself fails with 401 the
 * token is bad and the UI must say so rather than retrying forever." Only a new credential leaves
 * this phase, which is why it carries the message that explains it.
 */
export type LinkPhase =
  | { phase: 'idle' }
  | { phase: 'authorizing'; attempt: number }
  | { phase: 'connecting'; attempt: number }
  | { phase: 'syncing'; attempt: number }
  | { phase: 'live'; since: number }
  | { phase: 'retrying'; attempt: number; retryAt: number; failure: RequestFailure }
  | { phase: 'unauthorized'; at: number; message: string };

/** An `alert` event with the identity a list needs. */
export interface ReceivedAlert {
  seq: number;
  at: number;
  ts: string;
  value: Alert;
}

/**
 * Alerts kept. Bounded because `alert` has no cadence bound in §4.3 ("as needed") and a fault
 * loop on the node must not turn into unbounded growth on the phone.
 */
const ALERT_LIMIT = 32;

export interface TelemetryState {
  link: LinkPhase;
  /**
   * Round-trip time from the last answered ping, milliseconds.
   *
   * Recorded here and rendered by M1-T15 (§8.3(8) header link health). M1-T04 needs the ping loop
   * regardless — see `lib/link/connection.ts` on why silence, not a close event, is how a tunnel
   * usually dies — so publishing the number it already has costs nothing and saves T15 a rework.
   */
  rttMs: number | null;
  mountPosition: Slot<MountPosition>;
  mountStatus: Slot<MountStatus>;
  cameraStatus: Slot<CameraStatus>;
  captureProgress: Slot<CaptureProgress>;
  transferStatus: Slot<TransferStatus>;
  stackStatus: Slot<StackStatus>;
  systemHealth: Slot<SystemHealth>;
  /** Newest first. */
  alerts: ReceivedAlert[];
  /** Monotonic, so a React key survives the list being trimmed. */
  alertSeq: number;
}

const UNKNOWN = { state: 'unknown' } as const;

/** Telemetry with nothing in it. The snapshot handler rebuilds from exactly this. */
const NO_TELEMETRY = {
  mountPosition: UNKNOWN as Slot<MountPosition>,
  mountStatus: UNKNOWN as Slot<MountStatus>,
  cameraStatus: UNKNOWN as Slot<CameraStatus>,
  captureProgress: UNKNOWN as Slot<CaptureProgress>,
  transferStatus: UNKNOWN as Slot<TransferStatus>,
  stackStatus: UNKNOWN as Slot<StackStatus>,
  systemHealth: UNKNOWN as Slot<SystemHealth>,
};

export const EMPTY: TelemetryState = {
  link: { phase: 'idle' },
  rttMs: null,
  ...NO_TELEMETRY,
  alerts: [],
  alertSeq: 0,
};

// ---------------------------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------------------------

/**
 * Every way this store can change.
 *
 * All of them are `link/…` — something the socket did or saw — except `session/reset`, which is
 * the operator replacing the credential and therefore invalidating everything learned under the
 * old one. See rule 1 in the module docs for why there is nothing here a command could dispatch.
 */
export type LinkAction =
  | { type: 'link/authorizing'; at: number; attempt: number }
  | { type: 'link/connecting'; at: number; attempt: number }
  | { type: 'link/open'; at: number; attempt: number }
  | { type: 'link/snapshot'; at: number; events: TelemetryEvent[] }
  | { type: 'link/event'; at: number; event: TelemetryEvent }
  | { type: 'link/retrying'; at: number; attempt: number; retryAt: number; failure: RequestFailure }
  | { type: 'link/unauthorized'; at: number; message: string }
  | { type: 'link/rtt'; rttMs: number }
  | { type: 'link/stopped'; at: number }
  | { type: 'session/reset' };

export function reduce(state: TelemetryState, action: LinkAction): TelemetryState {
  switch (action.type) {
    case 'link/authorizing':
      return { ...state, link: { phase: 'authorizing', attempt: action.attempt } };
    case 'link/connecting':
      return { ...state, link: { phase: 'connecting', attempt: action.attempt } };
    case 'link/open':
      return { ...state, link: { phase: 'syncing', attempt: action.attempt } };
    case 'link/snapshot': {
      // Rebuild from empty — rule 2 in the module docs.
      const rebuilt: TelemetryState = {
        ...state,
        ...NO_TELEMETRY,
        link: { phase: 'live', since: action.at },
      };
      return action.events.reduce((acc, event) => applyEvent(acc, event, action.at), rebuilt);
    }
    case 'link/event':
      return applyEvent(state, action.event, action.at);
    case 'link/retrying':
      return {
        ...state,
        link: {
          phase: 'retrying',
          attempt: action.attempt,
          retryAt: action.retryAt,
          failure: action.failure,
        },
      };
    case 'link/unauthorized':
      return { ...state, link: { phase: 'unauthorized', at: action.at, message: action.message } };
    case 'link/rtt':
      return { ...state, rttMs: action.rttMs };
    case 'link/stopped':
      return { ...state, link: { phase: 'idle' } };
    case 'session/reset':
      return EMPTY;
  }
}

function applyEvent(state: TelemetryState, event: TelemetryEvent, at: number): TelemetryState {
  switch (event.topic) {
    case 'mount.position':
      return { ...state, mountPosition: observed(at, event.ts, event.data) };
    case 'mount.status':
      return { ...state, mountStatus: observed(at, event.ts, event.data) };
    case 'camera.status':
      return { ...state, cameraStatus: observed(at, event.ts, event.data) };
    case 'capture.progress':
      return { ...state, captureProgress: observed(at, event.ts, event.data) };
    case 'transfer.status':
      return { ...state, transferStatus: observed(at, event.ts, event.data) };
    case 'stack.status':
      return { ...state, stackStatus: observed(at, event.ts, event.data) };
    case 'system.health':
      return { ...state, systemHealth: observed(at, event.ts, event.data) };
    case 'alert':
      return {
        ...state,
        alertSeq: state.alertSeq + 1,
        alerts: [
          { seq: state.alertSeq + 1, at, ts: event.ts, value: event.data },
          ...state.alerts,
        ].slice(0, ALERT_LIMIT),
      };
    case 'frame.saved':
    case 'transfer.acked':
      // Per-frame facts, not state: they belong to a session frame list that M1-T08 (capture) and
      // M1-T11 (transfer) add. Listed explicitly rather than caught by a default so that a new
      // §4.3 topic still fails to compile until somebody decides what it means here.
      return state;
  }
}

function observed<T>(at: number, ts: string, value: T): Slot<T> {
  return { state: 'observed', at, ts, value };
}

// ---------------------------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------------------------

interface TelemetryStore extends TelemetryState {
  dispatch: (action: LinkAction) => void;
}

export const useTelemetryStore = create<TelemetryStore>()((set) => ({
  ...EMPTY,
  dispatch: (action) => set((state) => reduce(state, action)),
}));

export const dispatch = (action: LinkAction): void => useTelemetryStore.getState().dispatch(action);

// ---------------------------------------------------------------------------------------------
// Selectors — §5.9 requires these because `mount.position` ticks at 1 Hz and must not re-render
// a panel that does not read a position.
// ---------------------------------------------------------------------------------------------

export const selectLink = (state: TelemetryState): LinkPhase => state.link;
export const selectMountPosition = (state: TelemetryState): Slot<MountPosition> =>
  state.mountPosition;
export const selectMountStatus = (state: TelemetryState): Slot<MountStatus> => state.mountStatus;
export const selectCameraStatus = (state: TelemetryState): Slot<CameraStatus> => state.cameraStatus;
export const selectStackStatus = (state: TelemetryState): Slot<StackStatus> => state.stackStatus;
export const selectAlerts = (state: TelemetryState): ReceivedAlert[] => state.alerts;

/** Whether the socket is up *and* the snapshot has been applied — not merely connected. */
export function isLive(link: LinkPhase): boolean {
  return link.phase === 'live';
}
