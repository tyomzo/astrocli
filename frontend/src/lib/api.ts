/*
 * The one place this app talks to a server — ADR-07, SDD §4.2/§4.5.
 *
 * # The field node is the only origin
 *
 * ADR-07 ("operator single-URL topology", STK-19): the browser reaches the stacking server through
 * `/stack/*` on the field node, never at the stack node's own origin. That is what makes one URL,
 * one token prompt and one asymmetric-VPN hop work — the operator's phone may have no route to the
 * stack node at all.
 *
 * Every path below is therefore relative and same-origin, and [`assertSameOrigin`] refuses an
 * absolute URL at runtime rather than trusting the convention. The check exists because the
 * failure it prevents is invisible in review on a developer's LAN, where both nodes are reachable
 * and a direct call works fine, and only appears in the field.
 *
 * # Failures are a normal operating state
 *
 * The other node being down is expected, not exceptional (SDD §5.10.1), so nothing here throws.
 * Callers get a discriminated result and decide what to render. The three failure kinds are
 * distinguished because the operator's next action differs for each: fix the token, wait, or go
 * look at the other machine.
 */

import { issuedAt, newCommandId } from './clock';

/**
 * The command envelope's two request headers — SDD §5.8.1, M1-T10.
 *
 * Headers rather than body fields because half the mutation surface has no body: the node's
 * `/api/mount/park`, `/api/camera/capture/abort` and both live-view controls take none at all, and
 * `/api/mount/slew/stop` takes an optional one *so that a stop can never fail to parse*. See the
 * field node's `command.rs` for the full argument; what matters here is that the client and the
 * server put them in the same place.
 */
export const COMMAND_ID_HEADER = 'astroctl-command-id';
export const ISSUED_AT_HEADER = 'astroctl-issued-at';

/** The node's clock on every response, which is what `lib/clock.ts` corrects against. */
export const SERVER_TIME_HEADER = 'astroctl-server-time';

/** Set when the node answered from its idempotency ledger instead of executing again. */
export const REPLAYED_HEADER = 'astroctl-replayed';

/** SDD §4.2 error envelope, as it arrives on the wire. */
interface ErrorEnvelope {
  v: number;
  code: string;
  message: string;
  retryable: boolean;
  detail?: unknown;
}

export type RequestFailure =
  /**
   * 401. Retrying will not help (SEC-02).
   *
   * `presented` separates the two ways to get one, because they are different instructions:
   * `false` means the app sent no `Authorization` header at all and the operator has yet to
   * enter a token; `true` means one was sent and the node refused it. Collapsing them produces
   * "token rejected" on a first load where nothing was ever offered, which sends the operator
   * looking for a wrong value instead of an absent one.
   */
  | { kind: 'unauthorized'; presented: boolean; message: string }
  /** The node answered with the §4.2 envelope. `code` is what the UI switches on. */
  | { kind: 'api'; status: number; code: string; message: string; retryable: boolean }
  /** Nothing answered: DNS, TCP, the VPN, or a response that was not the envelope. */
  | { kind: 'transport'; message: string };

export type RequestResult<T> = { ok: true; value: T } | { ok: false; failure: RequestFailure };

/** Field node health — SDD §5.8.1. */
export const FIELD_HEALTH = '/api/system/health';

/**
 * Stack node health, **through the field node's proxy** — SDD §5.11.1 reached via §5.8.1's
 * `/stack/{*rest}`. The `/stack` prefix is the entire point; see the module docs.
 */
export const STACK_HEALTH = '/stack/api/system/health';

/** Single-use WebSocket ticket — SDD §4.5, §5.8.1. */
export const WS_TICKET = '/api/auth/ws-ticket';

/** The control/status socket — SDD §5.8.1. Binary frames live on `/ws/liveview`. */
export const WS_EVENTS = '/ws';

/**
 * The binary image socket — SDD §5.8.1, §8.3(5).
 *
 * A second socket rather than a second message type on the first, and the reason is TCP rather
 * than tidiness: two streams multiplexed onto one connection share one retransmit queue, so a
 * 500 KB JPEG that needs resending would hold up the position update and the stop command behind
 * it. Separate connections cannot do that to each other.
 */
export const WS_LIVEVIEW = '/ws/liveview';

/**
 * The stacking server's preview socket, through the field node's proxy — SDD §5.11.1, ADR-07.
 *
 * A third socket for the same §8.3(5) reason as the second, one hop further out: the stack node's
 * previews are the largest images in the system, and putting them on a connection shared with
 * anything else would let one retransmit hold up everything behind it.
 */
export const WS_STACK_PREVIEW = '/stack/ws/preview';

/** `POST /api/auth/ws-ticket` → 200. */
export interface WsTicket {
  ticket: string;
  expires_in: number;
}

/**
 * The socket URL for one ticket, on this same origin.
 *
 * Derived from `location` rather than configured, for the reason the whole module exists: the
 * PWA is served by the field node, so the field node is wherever the PWA came from. Deriving it
 * also gets the scheme right without a second decision — `https` ⇒ `wss`, and an operator who
 * reached the app over TLS cannot end up with an unencrypted socket carrying their telemetry.
 *
 * The ticket goes in the query string, which is exactly what §4.5's single-use, 30-second,
 * server-generated ticket exists to make safe: the long-lived bearer token never appears in a URL
 * and therefore never reaches the node's access log.
 */
export function eventSocketUrl(ticket: string, location: URL | Location = window.location): string {
  return socketUrl(WS_EVENTS, ticket, location);
}

/**
 * The live-view socket URL for one ticket.
 *
 * A separate ticket from the control socket's, always. §4.5 makes a ticket single-use and the
 * *upgrade* is what consumes it, so sharing one would open whichever socket connected first and
 * silently fail the other — and would then fail differently on every reconnect. `connection.ts`
 * says the same thing about reuse: it is not an optimisation, it is a failure.
 */
export function liveviewSocketUrl(
  ticket: string,
  location: URL | Location = window.location,
): string {
  return socketUrl(WS_LIVEVIEW, ticket, location);
}

/**
 * The **stacking server's** preview socket, reached through the field node (ADR-07, M1-T14).
 *
 * Still this origin. That is the whole point of the proxy: the browser has one origin, one token
 * prompt, and works in the asymmetric VPN topologies STK-19 describes — so this URL is built the
 * same way as the other two and there is deliberately no configuration key pointing at :8471.
 * `assertSameOrigin` guards the REST side of the same rule.
 *
 * A ticket of its own, again. The upgrade consumes it (§4.5), and the field node spends this one
 * before dialling the stacking server with its own bearer header — so a PWA showing live view and
 * the stack preview at once holds two sockets and fetched two tickets.
 */
export function stackPreviewSocketUrl(
  ticket: string,
  location: URL | Location = window.location,
): string {
  return socketUrl(WS_STACK_PREVIEW, ticket, location);
}

function socketUrl(path: string, ticket: string, location: URL | Location): string {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${scheme}//${location.host}${path}?ticket=${encodeURIComponent(ticket)}`;
}

function assertSameOrigin(path: string): void {
  if (/^[a-z][a-z0-9+.-]*:/i.test(path) || path.startsWith('//')) {
    throw new Error(
      `refusing to request ${path}: the PWA only ever calls the field node's own origin ` +
        `(ADR-07). The stacking server is reached through /stack/*.`,
    );
  }
}

/**
 * One authenticated GET.
 *
 * `token` may be `null`: a node running under SDD §4.5's loopback exception serves without one,
 * and sending `Authorization: Bearer null` would turn a working setup into a 401.
 */
export async function getJson<T>(path: string, token: string | null): Promise<RequestResult<T>> {
  assertSameOrigin(path);

  const headers = new Headers({ Accept: 'application/json' });
  if (token !== null) {
    headers.set('Authorization', `Bearer ${token}`);
  }

  let response: Response;
  try {
    response = await fetch(path, {
      headers,
      // The service worker never caches API responses (USB-10), but a stale HTTP cache would
      // fake liveness just as convincingly.
      cache: 'no-store',
      credentials: 'omit',
    });
  } catch (error) {
    return { ok: false, failure: { kind: 'transport', message: describe(error) } };
  }

  if (response.ok) {
    try {
      return { ok: true, value: (await response.json()) as T };
    } catch (error) {
      return {
        ok: false,
        failure: {
          kind: 'transport',
          message: `${path} answered 200 with something that is not JSON: ${describe(error)}`,
        },
      };
    }
  }

  return { ok: false, failure: await readFailure(response, path, token) };
}

/**
 * One authenticated POST — the command path (SDD §5.8.1).
 *
 * The success type is `T | null` because a command's answer is frequently nothing: some routes
 * return the resulting status, `goto` returns `202 {correlation_id, watch_topic}`, and others
 * would reasonably return `204`. Making the empty case explicit in the type is what stops a
 * caller destructuring `undefined` on the one route that happens to be silent.
 *
 * `keepalive` exists for the stop path. A hold released by the phone being locked fires
 * `pagehide`, and a normal `fetch` started in that handler is cancelled when the document goes
 * away — the axis would then keep moving until the dead-man's TTL caught it. §8.3(5) puts stop
 * traffic on the browser's separate keepalive-capable pool for the same reason.
 *
 * # The envelope (M1-T10)
 *
 * Every call carries a fresh `command_id` and a skew-corrected `issued_at` (SDD §5.8.1), because
 * the node refuses a covered mutation without them. Attached here rather than at each call site
 * for the reason the node classifies in its route table rather than in each handler: the failure
 * being designed against is the *one* command somebody forgets.
 *
 * `envelope: false` opts out, and exactly one caller uses it — the e-stop, whose route is declared
 * `exempt` on the node (§5.8.2). Sending headers it ignores would cost nothing but would suggest
 * this route depends on them, and it depends on nothing.
 */
export function postJson<T>(
  path: string,
  token: string | null,
  body?: unknown,
  options: { keepalive?: boolean; envelope?: false } = {},
): Promise<RequestResult<T | null>> {
  return send<T>('POST', path, token, body, options);
}

/**
 * One authenticated PUT — used by `/api/camera/settings` and, so far, nothing else.
 *
 * The method is the point rather than a preference: sending the same settings twice must leave the
 * camera where sending them once did, and over a tunnel where a reply can be lost that is the
 * difference between a retry and a second change. The node declares the route as a `PUT` for the
 * same reason (SDD §5.8.1), so this is the two halves agreeing rather than a client convention.
 */
export function putJson<T>(
  path: string,
  token: string | null,
  body?: unknown,
): Promise<RequestResult<T | null>> {
  return send<T>('PUT', path, token, body, {});
}

async function send<T>(
  method: 'POST' | 'PUT',
  path: string,
  token: string | null,
  body: unknown,
  options: { keepalive?: boolean; envelope?: false },
): Promise<RequestResult<T | null>> {
  assertSameOrigin(path);

  const headers = new Headers({ Accept: 'application/json' });
  if (token !== null) {
    headers.set('Authorization', `Bearer ${token}`);
  }
  if (body !== undefined) {
    headers.set('Content-Type', 'application/json');
  }
  if (options.envelope !== false) {
    headers.set(COMMAND_ID_HEADER, newCommandId());
    headers.set(ISSUED_AT_HEADER, issuedAt());
  }

  let response: Response;
  try {
    response = await fetch(path, {
      method,
      headers,
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      ...(options.keepalive === true ? { keepalive: true } : {}),
      cache: 'no-store',
      credentials: 'omit',
    });
  } catch (error) {
    return { ok: false, failure: { kind: 'transport', message: describe(error) } };
  }

  if (response.ok) {
    const text = await response.text();
    if (text.trim() === '') {
      return { ok: true, value: null };
    }
    try {
      return { ok: true, value: JSON.parse(text) as T };
    } catch (error) {
      return {
        ok: false,
        failure: {
          kind: 'transport',
          message: `${path} answered ${response.status} with something that is not JSON: ${describe(error)}`,
        },
      };
    }
  }

  return { ok: false, failure: await readFailure(response, path, token) };
}

/** The §4.2 envelope, or the two ways there isn't one, for any non-2xx response. */
async function readFailure(
  response: Response,
  path: string,
  token: string | null,
): Promise<RequestFailure> {
  const envelope = await readEnvelope(response);
  if (response.status === 401) {
    return {
      kind: 'unauthorized',
      presented: token !== null,
      message:
        envelope?.message ??
        (token !== null
          ? 'the stored token was rejected'
          : 'this telescope needs a token and none has been entered'),
    };
  }
  if (envelope !== null) {
    return {
      kind: 'api',
      status: response.status,
      code: envelope.code,
      message: envelope.message,
      retryable: envelope.retryable,
    };
  }
  return {
    kind: 'transport',
    message: `${path} answered ${response.status} without an error envelope`,
  };
}

async function readEnvelope(response: Response): Promise<ErrorEnvelope | null> {
  try {
    const body = (await response.json()) as Partial<ErrorEnvelope>;
    if (typeof body.code === 'string' && typeof body.message === 'string') {
      return {
        v: typeof body.v === 'number' ? body.v : 1,
        code: body.code,
        message: body.message,
        retryable: body.retryable === true,
      };
    }
    return null;
  } catch {
    return null;
  }
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
