/*
 * DEV-ONLY MOCK — M1-T03 DELETES THIS DIRECTORY.
 *
 * The field node's Phase 1 surface, as far as M1-T04's mount panel touches it: the ws-ticket
 * exchange (SDD §4.5), the `/ws` hub with snapshot-on-connect (§5.8.3), and the mount rows of the
 * §5.8.1 route table. M1-T04's task header authorises this because M1-T03 does not exist yet and
 * a store fed by a socket cannot be written, let alone demonstrated, without one.
 *
 * **This is a contract, not a sketch.** `mock/README.md` writes down every frame and every route
 * exactly as implemented here, including the two frame shapes SDD §5.8.3 leaves undefined
 * (snapshot and ping/pong). M1-T03 implements *that*, and the PWA needs no change when it does —
 * pointing at the real node is stopping this process, nothing more.
 *
 * It deliberately does NOT simulate: the camera (M1-T06/T08 — `camera.status` is reported
 * disconnected, which is true of this build), capture, transfers, live view, the command envelope
 * (M1-T10), or `/api/mount/estop` (M1-T05 — the header button stays visibly unarmed, and a mock
 * that answered it would have made that button lie).
 *
 * Run:  npm run mock          (listens on 127.0.0.1:8470, which is where vite.config.ts proxies)
 * Then: npm run dev           in a second terminal
 *
 * Options (env):
 *   ASTROCTL_MOCK_TOKEN=…   require exactly this bearer token; unset = accept anything, matching
 *                           §4.5's loopback exception
 *   ASTROCTL_MOCK_PORT=…    default 8470
 *   ASTROCTL_MOCK_NULL_ALTAZ=1
 *                           report alt/az as null, which is what M1-T03 emits until M1-T05
 *                           populates them — the state the UI must render as "unknown", not 0°
 *   ASTROCTL_MOCK_STACK_OFFLINE=1
 *                           report the stack node unreachable
 */

import { createServer } from 'node:http';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { WebSocketServer } from 'ws';

import { createMount, SLEW_SPEEDS } from './mount.mjs';

const PORT = Number(process.env.ASTROCTL_MOCK_PORT ?? 8470);
const TOKEN = process.env.ASTROCTL_MOCK_TOKEN ?? null;
const NULL_ALTAZ = process.env.ASTROCTL_MOCK_NULL_ALTAZ === '1';
const STACK_OFFLINE = process.env.ASTROCTL_MOCK_STACK_OFFLINE === '1';

const EVENT_SCHEMA_VERSION = 1;
const TICKET_TTL_MS = 30_000;
const TICKET_STORE_CAP = 64;
const STARTED_MS = Date.now();

/**
 * Topics whose latest value is state rather than an occurrence (SDD §5.8.3 "current status of
 * every stateful topic"). `alert`, `frame.saved` and `transfer.acked` are events that happened,
 * not values that are true, so they are never replayed into a snapshot.
 */
const STATEFUL_TOPICS = [
  'mount.status',
  'mount.position',
  'camera.status',
  'capture.progress',
  'transfer.status',
  'stack.status',
  'system.health',
];

// ---------------------------------------------------------------------------------------------
// Event bus + snapshot
// ---------------------------------------------------------------------------------------------

/** topic → the latest event, which is exactly what a new client's snapshot is built from. */
const latest = new Map();
const clients = new Set();

function publish(topic, data) {
  const event = { v: EVENT_SCHEMA_VERSION, ts: rfc3339(), topic, data };
  if (STATEFUL_TOPICS.includes(topic)) {
    latest.set(topic, event);
  }
  const line = JSON.stringify(event);
  for (const socket of clients) {
    if (socket.readyState === socket.OPEN) socket.send(line);
  }
}

const alert = (severity, code, message) => publish('alert', { severity, code, message });

function snapshotFrame() {
  return JSON.stringify({
    v: EVENT_SCHEMA_VERSION,
    type: 'snapshot',
    ts: rfc3339(),
    events: STATEFUL_TOPICS.filter((topic) => latest.has(topic)).map((topic) => latest.get(topic)),
  });
}

// ---------------------------------------------------------------------------------------------
// The simulated mount
// ---------------------------------------------------------------------------------------------

const mount = createMount({ publish, alert });

/** Strip alt/az when asked to, so the M1-T03-shaped payload can be exercised. See the header. */
function position() {
  const raw = mount.position();
  return NULL_ALTAZ ? { ...raw, alt: null, az: null } : raw;
}

function publishPosition() {
  if (mount.isConnected()) {
    publish('mount.position', position());
  } else {
    // Disconnected means the coordinates are unknown, not stale — so the topic leaves the
    // snapshot entirely and a reconnecting client reduces it back to "unknown".
    latest.delete('mount.position');
  }
}

setInterval(() => {
  mount.tick();
  publishPosition();
}, 1000);

// The dead-man's switch has to expire on time, and a 1 Hz loop would miss a 500 ms TTL by 500 ms.
setInterval(() => mount.subTick(), 100);

setInterval(() => publish('system.health', systemHealthEvent()), 60_000);
setInterval(() => publish('camera.status', cameraStatus()), 60_000);
setInterval(() => publish('stack.status', stackStatus()), 30_000);
setInterval(() => publish('transfer.status', transferStatus()), 30_000);

// Seed the snapshot so the very first client sees a complete picture. `tick` rather than a direct
// publish, so the mount's own change detector starts with the state it just reported.
mount.tick();
publishPosition();
publish('camera.status', cameraStatus());
publish('stack.status', stackStatus());
publish('transfer.status', transferStatus());
publish('system.health', systemHealthEvent());

function cameraStatus() {
  return { connected: false, battery_pct: null, charging: false, storage_free_mb: null };
}

function stackStatus() {
  return STACK_OFFLINE
    ? {
        connected: false,
        session_frame_count: 0,
        last_preview_ts: null,
        worker_state: null,
        restarts: 0,
      }
    : {
        connected: true,
        session_frame_count: 0,
        last_preview_ts: null,
        worker_state: 'ready',
        restarts: 0,
      };
}

function transferStatus() {
  return { state: 'idle', queue_depth: 0, oldest_queued_age_s: null, last_ack_ts: null };
}

function systemHealthEvent() {
  return { disk_free_gb: 182.4, clock_synced: true, uptime_s: uptimeSeconds() };
}

// ---------------------------------------------------------------------------------------------
// Auth (SDD §4.5)
// ---------------------------------------------------------------------------------------------

/** ticket → expiry. Bounded and swept, per §4.5's "bounded store". */
const tickets = new Map();

function authorized(req) {
  if (TOKEN === null) return true;
  const header = req.headers.authorization ?? '';
  const presented = header.startsWith('Bearer ') ? header.slice(7) : '';
  const a = Buffer.from(presented);
  const b = Buffer.from(TOKEN);
  return a.length === b.length && timingSafeEqual(a, b);
}

function issueTicket() {
  const now = Date.now();
  for (const [value, expiry] of tickets) {
    if (expiry <= now) tickets.delete(value);
  }
  while (tickets.size >= TICKET_STORE_CAP) {
    tickets.delete(tickets.keys().next().value);
  }
  // ≥128 bits, server-generated, never derived from the bearer token (§4.5).
  const ticket = randomBytes(16).toString('hex');
  tickets.set(ticket, now + TICKET_TTL_MS);
  return { ticket, expires_in: TICKET_TTL_MS / 1000 };
}

/** Single use: valid exactly once, and the check consumes it. */
function consumeTicket(ticket) {
  const expiry = tickets.get(ticket);
  tickets.delete(ticket);
  return expiry !== undefined && expiry > Date.now();
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

const server = createServer((req, res) => {
  const url = new URL(req.url ?? '/', `http://${req.headers.host ?? 'localhost'}`);
  const route = `${req.method} ${url.pathname}`;

  if (!authorized(req)) {
    return envelope(res, 401, 'AUTH', 'missing or wrong bearer token', false);
  }

  switch (route) {
    case 'POST /api/auth/ws-ticket':
      return json(res, 200, issueTicket());

    case 'GET /api/system/health':
      return json(res, 200, health('astroctl-field'));

    case 'GET /stack/api/system/health':
      return STACK_OFFLINE
        ? envelope(res, 502, 'DEVICE_TIMEOUT', 'the stack node did not answer', true)
        : json(res, 200, {
            ...health('astroctl-stack'),
            worker: { state: 'ready', restarts: 0 },
          });

    case 'GET /api/mount/position':
      return json(res, 200, position());

    case 'GET /api/mount/status':
      return json(res, 200, mount.status());

    case 'POST /api/mount/connect':
      return json(res, 200, mount.connect());

    case 'POST /api/mount/disconnect':
      return json(res, 200, mount.disconnect());

    case 'POST /api/mount/goto':
      return withBody(req, res, (body) => {
        const ra = finite(body.ra_hours);
        const dec = finite(body.dec_degrees);
        if (ra === null || ra < 0 || ra >= 24 || dec === null || dec < -90 || dec > 90) {
          return envelope(res, 422, 'VALIDATION', 'ra_hours ∈ [0,24), dec_degrees ∈ [-90,90]', false);
        }
        const started = mount.startGoto(ra, dec);
        return started.ok
          ? json(res, 202, { correlation_id: started.correlationId, watch_topic: started.watchTopic })
          : envelope(res, started.status, started.code, started.message, false);
      });

    case 'POST /api/mount/tracking':
      return withBody(req, res, (body) => {
        if (!['sidereal', 'lunar', 'solar', 'off'].includes(body.mode)) {
          return envelope(res, 422, 'VALIDATION', 'mode ∈ sidereal|lunar|solar|off', false);
        }
        return json(res, 200, mount.setTracking(body.mode));
      });

    case 'POST /api/mount/slew':
      return withBody(req, res, (body) => {
        const speed = Number(body.speed);
        const ttl = Math.min(2000, Number(body.ttl_ms ?? 500));
        if (
          !['ra', 'dec'].includes(body.axis) ||
          !['positive', 'negative'].includes(body.direction) ||
          !Number.isInteger(speed) ||
          speed < 1 ||
          speed > SLEW_SPEEDS
        ) {
          return envelope(
            res,
            422,
            'VALIDATION',
            `axis ∈ ra|dec, direction ∈ positive|negative, speed ∈ 1..${SLEW_SPEEDS}`,
            false,
          );
        }
        const leased = mount.slew(body.axis, body.direction, speed, ttl);
        return leased.ok
          ? json(res, 200, { axis: leased.axis, expires_in_ms: leased.expires_in_ms })
          : envelope(res, leased.status, leased.code, leased.message, false);
      });

    case 'POST /api/mount/slew/stop':
      return withBody(req, res, (body) =>
        json(res, 200, mount.stopSlew(body.axis === undefined ? undefined : String(body.axis))),
      );

    // Not a route the field node has. It exists so the REL-10 reconnect path can be exercised
    // without killing the process — `curl -XPOST localhost:8470/api/mock/drop-clients`.
    case 'POST /api/mock/drop-clients': {
      const dropped = clients.size;
      for (const socket of clients) socket.close(1012, 'mock: service restart');
      return json(res, 200, { dropped });
    }

    default:
      return envelope(res, 404, 'NOT_FOUND', `the mock does not implement ${route}`, false);
  }
});

// ---------------------------------------------------------------------------------------------
// WebSocket (SDD §5.8.3)
// ---------------------------------------------------------------------------------------------

const wss = new WebSocketServer({ noServer: true });

server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url ?? '/', `http://${req.headers.host ?? 'localhost'}`);
  if (url.pathname !== '/ws') {
    return socket.destroy();
  }
  // The bearer token is never accepted here — §4.5 is explicit that the ticket is the only way a
  // browser authenticates the upgrade, and honouring a header would hide a PWA bug.
  if (!consumeTicket(url.searchParams.get('ticket') ?? '')) {
    socket.write('HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n');
    return socket.destroy();
  }
  wss.handleUpgrade(req, socket, head, (ws) => wss.emit('connection', ws, req));
});

wss.on('connection', (ws) => {
  clients.add(ws);
  // Snapshot first, always, before any event — §5.8.3, and what stops the UI rendering from
  // partial state.
  ws.send(snapshotFrame());

  ws.on('message', (raw) => {
    let message;
    try {
      message = JSON.parse(String(raw));
    } catch {
      return;
    }
    if (message.type === 'ping') {
      ws.send(
        JSON.stringify({
          v: EVENT_SCHEMA_VERSION,
          type: 'pong',
          ts: rfc3339(),
          id: message.id,
          server_time: rfc3339(),
        }),
      );
    }
    // subscribe/unsubscribe are accepted and ignored: the mock always sends every topic, which
    // is the default the PWA relies on (see mock/README.md).
  });

  ws.on('close', () => clients.delete(ws));
  ws.on('error', () => clients.delete(ws));
});

// ---------------------------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------------------------

function health(service) {
  return {
    v: 1,
    status: 'ok',
    service,
    disk_free_gb: 182.4,
    clock_synced: true,
    uptime_s: uptimeSeconds(),
    cert_expires_at: null,
    cert_days_remaining: null,
    versions: { astroctl: '0.1.0-mock', api: 1, event: 1, error_envelope: 1 },
  };
}

function uptimeSeconds() {
  return Math.floor((Date.now() - STARTED_MS) / 1000);
}

function withBody(req, res, handle) {
  const chunks = [];
  req.on('data', (chunk) => chunks.push(chunk));
  req.on('end', () => {
    const raw = Buffer.concat(chunks).toString('utf8').trim();
    let body = {};
    if (raw !== '') {
      try {
        body = JSON.parse(raw);
      } catch {
        return envelope(res, 422, 'VALIDATION', 'request body is not JSON', false);
      }
    }
    handle(body);
  });
}

function json(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'content-type': 'application/json',
    'content-length': Buffer.byteLength(payload),
    'cache-control': 'no-store',
  });
  res.end(payload);
}

/** The SDD §4.2 error envelope, byte-for-byte what `astroctl-core` serializes. */
function envelope(res, status, code, message, retryable) {
  json(res, status, { v: 1, code, message, retryable });
}

function finite(value) {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function rfc3339() {
  return new Date().toISOString().replace(/\.(\d{3})\d*Z$/, '.$1Z');
}

// The likely collision is a real `astroctl-field` already on this port, which is a good thing to
// have running and a terrible thing to diagnose from an unhandled EADDRINUSE stack trace.
server.on('error', (error) => {
  if (error.code === 'EADDRINUSE') {
    process.stderr.write(
      `astroctl mock: port ${PORT} is already in use.\n` +
        `  If that is a real astroctl-field, you do not need the mock — just run \`npm run dev\`.\n` +
        `  Otherwise pick another port with ASTROCTL_MOCK_PORT=… and point vite.config.ts at it.\n`,
    );
    process.exit(1);
  }
  throw error;
});

server.listen(PORT, '127.0.0.1', () => {
  process.stdout.write(
    `astroctl mock field node (M1-T03 retires this) on http://127.0.0.1:${PORT}\n` +
      `  auth      ${TOKEN === null ? 'open — any token accepted (§4.5 loopback exception)' : 'bearer token required'}\n` +
      `  alt/az    ${NULL_ALTAZ ? 'null (the M1-T03 shape, before M1-T05)' : 'computed'}\n` +
      `  stack     ${STACK_OFFLINE ? 'offline' : 'online'}\n`,
  );
});
