import type { RequestFailure, RequestResult, WsTicket } from '../api';
import { WS_TICKET, eventSocketUrl, postJson } from '../api';
import type { LinkAction } from '../../store/telemetry';
import { dispatch as dispatchTelemetry } from '../../store/telemetry';
import type { TelemetryEvent } from './protocol';
import { parseFrame } from './protocol';

/*
 * The `/ws` connection: ticket, socket, snapshot, and the reconnect loop — SDD §4.5, §5.8.3,
 * REL-10.
 *
 * # A fresh ticket before every connect, including every retry
 *
 * §4.5 is unusually explicit about this and the reason is a browser limitation, not a policy:
 * `new WebSocket(url)` takes a URL and a subprotocol list and nothing else, so there is no way to
 * put `Authorization` on the upgrade. The bearer token therefore never touches this socket. Each
 * attempt POSTs `/api/auth/ws-ticket`, gets a single-use 30-second value, and spends it. A
 * reconnect costs one extra round trip, which on a link that reconnects as often as this one does
 * is a real cost and still the right trade: the alternative puts a long-lived credential in a URL
 * that the node writes to its own access log — a file the operator may reasonably send to someone
 * for help.
 *
 * Because the ticket is consumed by the upgrade, **reusing one is not an optimisation, it is a
 * failure**: the retry would be rejected and the loop would never recover.
 *
 * # 401 is terminal
 *
 * A ticket request refused with 401 means the token is wrong. Retrying a wrong token is a loop
 * that never ends and never says why, so the attempt stops and the phase becomes `unauthorized`
 * until the operator supplies a new credential (§5.9).
 *
 * # Silence, not a close event, is how this link usually dies
 *
 * A VPN that drops does not close TCP connections — it stops carrying them. The socket stays
 * `OPEN`, `onclose` never fires, and a client that waits for one waits forever while showing
 * coordinates from ten minutes ago. So the client tests the link itself: a ping every
 * `PING_INTERVAL_MS`, and if nothing at all has arrived for `SILENCE_LIMIT_MS` the socket is
 * declared dead and torn down. §5.8.3's "the hub answers `ping` frames immediately" is what makes
 * that possible, and the round-trip it measures is the RTT §8.3(8) wants in the header (M1-T15).
 *
 * # Snapshot before events, always
 *
 * §5.8.3 sends a state snapshot on connect and §5.9 requires a resnapshot rather than resuming
 * from a hole. Two consequences implemented below: an event arriving before the snapshot is
 * *buffered*, not applied, because applying it and then rebuilding from the snapshot would
 * silently discard it; and a successful snapshot resets the backoff, because a connection that
 * reached a usable state is not evidence that the next attempt should wait longer.
 */

/** First retry delay. Short enough that a blip is invisible to the operator. */
const BACKOFF_BASE_MS = 500;

/** Ceiling. Beyond this the operator is waiting on something a human has to fix. */
const BACKOFF_CAP_MS = 15_000;

/** ±25%, so two devices dropped by the same tunnel do not retry in lockstep forever. */
const BACKOFF_JITTER = 0.25;

const PING_INTERVAL_MS = 5_000;

/**
 * How long a socket may deliver nothing before it is assumed dead.
 *
 * `mount.position` alone is 1 Hz, so twelve seconds of total silence is not a quiet moment. It is
 * deliberately several ping intervals: a single lost ping over a lossy tunnel must not cost a
 * reconnect, because the reconnect is more expensive than the packet.
 */
const SILENCE_LIMIT_MS = 12_000;

/**
 * Events held while the snapshot is outstanding.
 *
 * A cap because a hub that never sends a snapshot must not turn into unbounded memory on the
 * phone; if it is ever reached, the silence watchdog is already the thing that will fix it.
 */
const PENDING_LIMIT = 128;

export type TimerHandle = number;

/** What the socket hands back to the controller. */
export interface LinkSocketHandlers {
  onOpen(): void;
  onFrame(raw: string): void;
  onClosed(why: string): void;
}

export interface LinkSocket {
  send(text: string): void;
  close(): void;
}

/**
 * Everything the controller does to the outside world.
 *
 * Injected rather than imported so the reconnect logic is testable without a browser, a network,
 * or real time — the behaviours that matter here (a fresh ticket per attempt, backoff growth, a
 * resnapshot after a drop) are exactly the ones that are impossible to verify by hand.
 */
export interface LinkEnvironment {
  fetchTicket(): Promise<RequestResult<WsTicket>>;
  openSocket(ticket: string, handlers: LinkSocketHandlers): LinkSocket;
  now(): number;
  setTimer(run: () => void, ms: number): TimerHandle;
  clearTimer(handle: TimerHandle): void;
  /** [0, 1) — jitter only. */
  random(): number;
  dispatch(action: LinkAction): void;
}

export interface Link {
  /** Begin connecting. Calling twice is a no-op on the second call. */
  start(): void;
  /** Tear down. The controller is not reusable afterwards; build a new one. */
  stop(): void;
  /**
   * Test the link now rather than waiting for the watchdog.
   *
   * Wired to the tab becoming visible: a phone that slept for an hour comes back with a socket
   * the OS quietly killed, and the operator's first act is to look at the coordinates. Waiting
   * `SILENCE_LIMIT_MS` to notice is twelve seconds of showing an hour-old position.
   */
  checkNow(): void;
}

export function createLink(env: LinkEnvironment): Link {
  let started = false;
  let stopped = false;
  let attempt = 0;
  /**
   * Which connection attempt a callback belongs to.
   *
   * Closing a socket ourselves — on a silence timeout, or from `stop` — still fires its `close`
   * event a moment later. Without this, that late callback is indistinguishable from the peer
   * hanging up, and the controller schedules a *second* retry on top of the one it already
   * scheduled: two tickets, two sockets, two snapshots, and a store fed by both. Every handler
   * therefore carries the generation it was created for and does nothing once it is stale.
   */
  let generation = 0;
  let socket: LinkSocket | null = null;
  let snapshotApplied = false;
  let pending: TelemetryEvent[] = [];
  let lastFrameAt = 0;
  let retryTimer: TimerHandle | null = null;
  let heartbeatTimer: TimerHandle | null = null;
  let nextPingId = 1;
  const pingsInFlight = new Map<number, number>();

  function clear(handle: TimerHandle | null): null {
    if (handle !== null) env.clearTimer(handle);
    return null;
  }

  function teardownSocket(): void {
    heartbeatTimer = clear(heartbeatTimer);
    pingsInFlight.clear();
    // Retire this generation *before* closing, so the close event this triggers is already stale
    // by the time it arrives.
    generation += 1;
    const current = socket;
    socket = null;
    current?.close();
  }

  function scheduleRetry(failure: RequestFailure): void {
    if (stopped) return;
    const exponential = Math.min(BACKOFF_CAP_MS, BACKOFF_BASE_MS * 2 ** Math.max(0, attempt - 1));
    const jittered = Math.round(exponential * (1 + (env.random() * 2 - 1) * BACKOFF_JITTER));
    const at = env.now();
    env.dispatch({ type: 'link/retrying', at, attempt, retryAt: at + jittered, failure });
    retryTimer = env.setTimer(() => {
      retryTimer = null;
      void attemptConnect();
    }, jittered);
  }

  /** The socket is gone or must go; decide whether to try again. */
  function drop(why: string): void {
    teardownSocket();
    if (stopped) return;
    scheduleRetry({ kind: 'transport', message: why });
  }

  function heartbeat(): void {
    if (stopped || socket === null) return;

    if (env.now() - lastFrameAt > SILENCE_LIMIT_MS) {
      // Not an error the node reported — an absence the client noticed. See the module docs.
      drop(`no frame for ${Math.round(SILENCE_LIMIT_MS / 1000)} s; assuming the link is dead`);
      return;
    }

    const id = nextPingId++;
    pingsInFlight.set(id, env.now());
    socket.send(JSON.stringify({ type: 'ping', id }));
    heartbeatTimer = env.setTimer(heartbeat, PING_INTERVAL_MS);
  }

  function handlersFor(mine: number): LinkSocketHandlers {
    const current = (): boolean => !stopped && mine === generation;

    return {
      onOpen() {
        if (!current()) return;
        lastFrameAt = env.now();
        env.dispatch({ type: 'link/open', at: lastFrameAt, attempt });
        heartbeatTimer = env.setTimer(heartbeat, PING_INTERVAL_MS);
      },

      onFrame(raw) {
        if (!current()) return;
        // Any frame at all proves the link carries traffic, including one this build cannot read.
        lastFrameAt = env.now();
        const frame = parseFrame(raw);

        switch (frame.kind) {
          case 'snapshot': {
            env.dispatch({ type: 'link/snapshot', at: lastFrameAt, events: frame.events });
            // Buffered events are applied after, in arrival order: they are strictly newer than
            // the snapshot, so replaying them onto it is the correct order, not a race workaround.
            for (const event of pending) {
              env.dispatch({ type: 'link/event', at: lastFrameAt, event });
            }
            pending = [];
            snapshotApplied = true;
            attempt = 0;
            return;
          }
          case 'event': {
            if (snapshotApplied) {
              env.dispatch({ type: 'link/event', at: lastFrameAt, event: frame.event });
            } else if (pending.length < PENDING_LIMIT) {
              pending.push(frame.event);
            }
            return;
          }
          case 'pong': {
            const sentAt = pingsInFlight.get(frame.id);
            pingsInFlight.delete(frame.id);
            if (sentAt !== undefined) {
              env.dispatch({ type: 'link/rtt', rttMs: lastFrameAt - sentAt });
            }
            return;
          }
          case 'unreadable':
            // Counted as traffic above and otherwise ignored: a node one version ahead is a
            // normal condition, and one frame this bundle cannot read must not cost the
            // connection.
            return;
        }
      },

      onClosed(why) {
        if (!current()) return;
        drop(why);
      },
    };
  }

  async function attemptConnect(): Promise<void> {
    if (stopped) return;

    attempt += 1;
    snapshotApplied = false;
    pending = [];
    env.dispatch({ type: 'link/authorizing', at: env.now(), attempt });

    const ticket = await env.fetchTicket();
    if (stopped) return;

    if (!ticket.ok) {
      if (ticket.failure.kind === 'unauthorized') {
        env.dispatch({ type: 'link/unauthorized', at: env.now(), message: ticket.failure.message });
        return;
      }
      scheduleRetry(ticket.failure);
      return;
    }

    env.dispatch({ type: 'link/connecting', at: env.now(), attempt });
    try {
      lastFrameAt = env.now();
      generation += 1;
      socket = env.openSocket(ticket.value.ticket, handlersFor(generation));
    } catch (error) {
      scheduleRetry({
        kind: 'transport',
        message: error instanceof Error ? error.message : String(error),
      });
    }
  }

  return {
    start() {
      if (started) return;
      started = true;
      void attemptConnect();
    },

    stop() {
      stopped = true;
      retryTimer = clear(retryTimer);
      teardownSocket();
      env.dispatch({ type: 'link/stopped', at: env.now() });
    },

    checkNow() {
      if (stopped || socket === null) return;
      if (env.now() - lastFrameAt > SILENCE_LIMIT_MS) {
        drop('no frame since the tab was last visible; assuming the link is dead');
      }
    },
  };
}

/**
 * The real environment: `fetch`, `WebSocket`, `window` timers, the telemetry store.
 *
 * `getToken` is a function rather than a value because the controller outlives a render and the
 * credential must be read at the moment of the request, not at the moment of construction.
 */
export function browserEnvironment(getToken: () => string | null): LinkEnvironment {
  return {
    async fetchTicket() {
      const result = await postJson<WsTicket>(WS_TICKET, getToken());
      if (!result.ok) return result;
      if (result.value === null || typeof result.value.ticket !== 'string') {
        return {
          ok: false,
          failure: {
            kind: 'transport',
            message: `${WS_TICKET} answered without a ticket`,
          },
        };
      }
      return { ok: true, value: result.value };
    },

    openSocket(ticket, handlers) {
      const ws = new WebSocket(eventSocketUrl(ticket));
      ws.onopen = () => handlers.onOpen();
      ws.onmessage = (event) => {
        // Binary frames belong on `/ws/liveview` (§8.3(5)); anything binary here is not ours.
        if (typeof event.data === 'string') handlers.onFrame(event.data);
      };
      ws.onclose = (event) =>
        handlers.onClosed(
          `the event socket closed (code ${event.code}${event.reason === '' ? '' : `: ${event.reason}`})`,
        );
      // `error` is always followed by `close`, which is where the retry is decided; handling both
      // would schedule two.
      ws.onerror = () => {};
      return {
        send: (text) => {
          if (ws.readyState === WebSocket.OPEN) ws.send(text);
        },
        close: () => ws.close(),
      };
    },

    now: () => Date.now(),
    setTimer: (run, ms) => window.setTimeout(run, ms),
    clearTimer: (handle) => window.clearTimeout(handle),
    random: () => Math.random(),
    dispatch: dispatchTelemetry,
  };
}
