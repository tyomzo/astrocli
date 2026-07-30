import { describe, expect, it } from 'vitest';

import type { LinkAction } from '../../store/telemetry';
import type { LinkEnvironment, LinkSocket, LinkSocketHandlers } from './connection';
import { createLink } from './connection';

/*
 * The reconnect machine — SDD §4.5 (a fresh ticket per attempt), §5.8.3 (snapshot first), REL-10.
 *
 * All of it runs on a fake environment: fake tickets, a fake socket, a fake clock and a manual
 * timer queue. That is not only for speed. Every property worth asserting here is about *timing
 * and ordering under failure* — a ticket reused on a retry, an event applied before the snapshot
 * that then rebuilt over it, a socket that stops delivering without ever closing — and none of
 * them can be produced on demand against a real server.
 */

interface Harness {
  env: LinkEnvironment;
  actions: LinkAction[];
  ticketRequests: number;
  sockets: FakeSocket[];
  /** Fail the next `fetchTicket` with this, then clear it. */
  ticketFailure: { kind: 'unauthorized' | 'transport'; message: string } | null;
  advance(ms: number): void;
  flush(): Promise<void>;
}

interface FakeSocket extends LinkSocket {
  ticket: string;
  sent: string[];
  closed: boolean;
  handlers: LinkSocketHandlers;
}

function harness(): Harness {
  let now = 0;
  let nextTimer = 1;
  const timers = new Map<number, { at: number; run: () => void }>();

  const state: Harness = {
    actions: [],
    ticketRequests: 0,
    sockets: [],
    ticketFailure: null,

    advance(ms) {
      now += ms;
      // Fire in due order, allowing a timer to schedule another within the same advance.
      for (;;) {
        const due = [...timers.entries()]
          .filter(([, timer]) => timer.at <= now)
          .sort((a, b) => a[1].at - b[1].at)[0];
        if (due === undefined) break;
        timers.delete(due[0]);
        due[1].run();
      }
    },

    async flush() {
      // Two turns: `attemptConnect` awaits the ticket, then continues on a microtask.
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    },

    env: {
      async fetchTicket() {
        state.ticketRequests += 1;
        const failure = state.ticketFailure;
        state.ticketFailure = null;
        if (failure !== null) {
          return failure.kind === 'unauthorized'
            ? { ok: false, failure: { kind: 'unauthorized', presented: true, message: failure.message } }
            : { ok: false, failure: { kind: 'transport', message: failure.message } };
        }
        return { ok: true, value: { ticket: `ticket-${state.ticketRequests}`, expires_in: 30 } };
      },

      openSocket(ticket, handlers) {
        const socket: FakeSocket = {
          ticket,
          sent: [],
          closed: false,
          handlers,
          send: (text) => socket.sent.push(text),
          close: () => {
            if (socket.closed) return;
            socket.closed = true;
            // A real `WebSocket` fires `close` for a close *we* asked for, exactly as it does for
            // one the peer asked for. Modelling that is what surfaced the double-retry: the
            // controller cannot tell the two apart from the event alone.
            socket.handlers.onClosed('closed locally');
          },
        };
        state.sockets.push(socket);
        return socket;
      },

      now: () => now,
      setTimer: (run, ms) => {
        const handle = nextTimer++;
        timers.set(handle, { at: now + ms, run });
        return handle;
      },
      clearTimer: (handle) => {
        timers.delete(handle);
      },
      random: () => 0.5, // no jitter, so delays are exactly the exponential series
      dispatch: (action) => state.actions.push(action),
      // The real one folds the sample into `lib/clock.ts`; here the arithmetic is the assertion.
      // A `null` server time must produce no `link/skew` at all, which is what makes "a node older
      // than this bundle is normal" (§5.8.3) true rather than a silent zero.
      recordServerTime: (serverTime, sentAt, receivedAt) =>
        serverTime === null ? null : Date.parse(serverTime) - (sentAt + (receivedAt - sentAt) / 2),
    },
  };

  return state;
}

const snapshotFrame = JSON.stringify({
  v: 1,
  type: 'snapshot',
  ts: '2026-07-30T21:04:05.000Z',
  events: [
    {
      v: 1,
      ts: '2026-07-30T21:04:05.000Z',
      topic: 'mount.status',
      data: { state: 'idle', tracking: true, slewing: false, parked: false },
    },
  ],
});

function positionFrame(ra: number): string {
  return JSON.stringify({
    v: 1,
    ts: '2026-07-30T21:04:06.000Z',
    topic: 'mount.position',
    data: { ra, dec: 0, alt: null, az: null, pier_side: 'unknown' },
  });
}

function types(actions: LinkAction[]): string[] {
  return actions.map((action) => action.type);
}

describe('connecting', () => {
  it('fetches a ticket before the socket and opens with exactly that ticket', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();

    expect(h.ticketRequests).toBe(1);
    expect(h.sockets).toHaveLength(1);
    expect(h.sockets[0]?.ticket).toBe('ticket-1');
    expect(types(h.actions)).toEqual(['link/authorizing', 'link/connecting']);
  });

  it('buffers events that arrive before the snapshot and applies them after it', async () => {
    // The snapshot rebuilds the store from empty, so an event applied before it would be
    // discarded by it — a position silently one update out of date, forever.
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    const socket = h.sockets[0];
    if (socket === undefined) throw new Error('no socket');

    socket.handlers.onOpen();
    socket.handlers.onFrame(positionFrame(1.5));
    socket.handlers.onFrame(snapshotFrame);

    const applied = h.actions.filter(
      (action) => action.type === 'link/snapshot' || action.type === 'link/event',
    );
    expect(types(applied)).toEqual(['link/snapshot', 'link/event']);
    const replayed = applied[1];
    expect(replayed?.type === 'link/event' && replayed.event.topic).toBe('mount.position');
  });

  it('applies events directly once the snapshot has landed', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    const socket = h.sockets[0];
    if (socket === undefined) throw new Error('no socket');

    socket.handlers.onOpen();
    socket.handlers.onFrame(snapshotFrame);
    h.actions.length = 0;
    socket.handlers.onFrame(positionFrame(2.5));

    expect(types(h.actions)).toEqual(['link/event']);
  });
});

describe('reconnecting', () => {
  it('takes a new ticket for every attempt — a spent one would be rejected', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    h.sockets[0]?.handlers.onClosed('closed by peer');

    h.advance(1000);
    await h.flush();

    expect(h.ticketRequests).toBe(2);
    expect(h.sockets.map((socket) => socket.ticket)).toEqual(['ticket-1', 'ticket-2']);
  });

  it('backs off exponentially and resnapshots when it succeeds', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();

    // Three failed attempts: 500, 1000, 2000 ms with jitter pinned to the midpoint.
    for (const expected of [500, 1000, 2000]) {
      const socket = h.sockets[h.sockets.length - 1];
      socket?.handlers.onClosed('dropped');
      const retry = h.actions.filter((action) => action.type === 'link/retrying').at(-1);
      expect(retry?.type === 'link/retrying' && retry.retryAt - retry.at).toBe(expected);
      h.advance(expected);
      await h.flush();
    }

    const socket = h.sockets[h.sockets.length - 1];
    socket?.handlers.onOpen();
    socket?.handlers.onFrame(snapshotFrame);

    expect(types(h.actions).filter((type) => type === 'link/snapshot')).toEqual(['link/snapshot']);
  });

  it('resets the backoff after a snapshot, so one long outage does not slow the next blip', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();

    h.sockets[0]?.handlers.onClosed('dropped');
    h.advance(500);
    await h.flush();
    h.sockets[1]?.handlers.onOpen();
    h.sockets[1]?.handlers.onFrame(snapshotFrame);

    h.sockets[1]?.handlers.onClosed('dropped again');
    const retry = h.actions.filter((action) => action.type === 'link/retrying').at(-1);
    expect(retry?.type === 'link/retrying' && retry.retryAt - retry.at).toBe(500);
  });

  it('stops for good on a 401 from the ticket endpoint', async () => {
    // §5.9: a bad token must be surfaced, not retried forever.
    const h = harness();
    h.ticketFailure = { kind: 'unauthorized', message: 'the node rejected the bearer token' };
    createLink(h.env).start();
    await h.flush();

    expect(types(h.actions)).toEqual(['link/authorizing', 'link/unauthorized']);
    expect(h.sockets).toHaveLength(0);

    // Nothing is scheduled, so time passing changes nothing.
    h.advance(60_000);
    await h.flush();
    expect(h.ticketRequests).toBe(1);
  });

  it('retries a ticket endpoint that did not answer', async () => {
    const h = harness();
    h.ticketFailure = { kind: 'transport', message: 'network error' };
    createLink(h.env).start();
    await h.flush();

    expect(types(h.actions)).toEqual(['link/authorizing', 'link/retrying']);
    h.advance(500);
    await h.flush();
    expect(h.ticketRequests).toBe(2);
  });
});

describe('a link that stops carrying traffic', () => {
  it('tears down a socket that goes silent, even though it never closed', async () => {
    // The failure this exists for: a VPN drop leaves the socket OPEN and `onclose` never fires,
    // so a client waiting for a close event shows an hour-old position indefinitely.
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    const socket = h.sockets[0];
    socket?.handlers.onOpen();
    socket?.handlers.onFrame(snapshotFrame);

    h.advance(5000); // first ping
    expect(socket?.sent.map((text) => JSON.parse(text).type)).toEqual(['ping']);

    h.advance(5000); // second ping, still within the silence limit
    expect(socket?.closed).toBe(false);

    h.advance(5000); // now past 12 s with nothing received
    expect(socket?.closed).toBe(true);
    expect(h.actions.at(-1)?.type).toBe('link/retrying');
  });

  it('schedules exactly one retry when it closes a socket itself', async () => {
    // The close we ask for fires the same `close` event the peer's would. Treating it as a second
    // failure schedules a second retry: two tickets, two sockets, two snapshots, one store fed by
    // both.
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    const socket = h.sockets[0];
    socket?.handlers.onOpen();
    socket?.handlers.onFrame(snapshotFrame);

    h.advance(15_000); // silence watchdog fires and closes the socket
    expect(socket?.closed).toBe(true);
    expect(h.actions.filter((action) => action.type === 'link/retrying')).toHaveLength(1);

    h.advance(500);
    await h.flush();
    expect(h.sockets).toHaveLength(2);
    expect(h.ticketRequests).toBe(2);
  });

  it('measures RTT from the pong that answers a ping', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    const socket = h.sockets[0];
    socket?.handlers.onOpen();
    socket?.handlers.onFrame(snapshotFrame);

    h.advance(5000);
    const ping: unknown = JSON.parse(socket?.sent[0] ?? '{}');
    const id = (ping as { id: number }).id;
    h.advance(120);
    socket?.handlers.onFrame(JSON.stringify({ v: 1, type: 'pong', ts: '', id, server_time: null }));

    const rtt = h.actions.filter((action) => action.type === 'link/rtt').at(-1);
    expect(rtt?.type === 'link/rtt' && rtt.rttMs).toBe(120);
    // A pong with no `server_time` measures no clock offset. Dispatching a zero would tell the UI
    // the clocks agree on the strength of a node that never said so (SDD §5.8.1).
    expect(h.actions.filter((action) => action.type === 'link/skew')).toEqual([]);
  });

  /*
   * The skew measurement rides on the same ping — SDD §5.8.1, §8.3(4), M1-T10.
   *
   * Asserted here rather than only in `clock.test.ts` because the wiring is what can silently
   * rot: `protocol.ts` has parsed `server_time` since M1-T04 and nothing read it, which is
   * exactly how a value ends up on the wire and nowhere else.
   */
  it('folds the pong’s server time into a clock offset', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    const socket = h.sockets[0];
    socket?.handlers.onOpen();
    socket?.handlers.onFrame(snapshotFrame);

    h.advance(5000);
    const ping: unknown = JSON.parse(socket?.sent[0] ?? '{}');
    const id = (ping as { id: number }).id;
    h.advance(200);
    // The harness clock reads 5200 and the ping left at 5000, so the midpoint is 5100. A node
    // reporting 65 100 is sixty seconds ahead of this device.
    socket?.handlers.onFrame(
      JSON.stringify({
        v: 1,
        type: 'pong',
        ts: '',
        id,
        server_time: new Date(65_100).toISOString(),
      }),
    );

    const skew = h.actions.filter((action) => action.type === 'link/skew').at(-1);
    expect(skew?.type === 'link/skew' && skew.skewMs).toBe(60_000);
  });

  it('counts an unreadable frame as traffic — a newer node must not look like a dead link', async () => {
    const h = harness();
    createLink(h.env).start();
    await h.flush();
    const socket = h.sockets[0];
    socket?.handlers.onOpen();
    socket?.handlers.onFrame(snapshotFrame);

    h.advance(11_000);
    socket?.handlers.onFrame(
      JSON.stringify({ v: 1, ts: '2026-07-30T21:04:05.000Z', topic: 'future.topic', data: {} }),
    );
    h.advance(11_000);

    expect(socket?.closed).toBe(false);
  });
});

describe('stopping', () => {
  it('closes the socket and schedules nothing further', async () => {
    const h = harness();
    const link = createLink(h.env);
    link.start();
    await h.flush();
    h.sockets[0]?.handlers.onOpen();

    link.stop();

    expect(h.sockets[0]?.closed).toBe(true);
    expect(h.actions.at(-1)?.type).toBe('link/stopped');

    // A close arriving after stop must not start a retry — the store has already been reset by
    // whatever asked for the stop.
    h.sockets[0]?.handlers.onClosed('late close');
    h.advance(60_000);
    await h.flush();
    expect(h.ticketRequests).toBe(1);
  });
});
