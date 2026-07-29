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

  const envelope = await readEnvelope(response);
  if (response.status === 401) {
    return {
      ok: false,
      failure: {
        kind: 'unauthorized',
        presented: token !== null,
        message:
          envelope?.message ??
          (token !== null
            ? 'the node rejected the bearer token'
            : 'the node requires a bearer token'),
      },
    };
  }
  if (envelope !== null) {
    return {
      ok: false,
      failure: {
        kind: 'api',
        status: response.status,
        code: envelope.code,
        message: envelope.message,
        retryable: envelope.retryable,
      },
    };
  }
  return {
    ok: false,
    failure: {
      kind: 'transport',
      message: `${path} answered ${response.status} without an error envelope`,
    },
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
