import { useEffect, useState } from 'react';

import { liveviewSocketUrl, postJson, WS_TICKET } from '../api';
import type { WsTicket } from '../api';
import { decodeLiveFrame } from '../liveview';

/**
 * The `/ws/liveview` socket — SDD §5.8.1, §8.3(5).
 *
 * A second socket, not a second message type on the first. The reason is TCP: two streams sharing
 * one connection share one retransmit queue, so a 500 KB JPEG that needs resending would hold the
 * operator's position updates and stop commands behind it. `connection.ts` owns the control link
 * and its reconnect state machine (REL-10); this owns the image link, and the two are deliberately
 * independent — an image socket that dies must not make the telemetry look dead.
 *
 * # Open whenever there is a link, not only while streaming
 *
 * Previews are pushed on capture whether or not live view is running (§5.7), so a socket that
 * opened only for live view would miss exactly the image the operator waits for after an
 * exposure. Live view start/stop controls the *camera*; this socket follows the link.
 *
 * # Object URLs are revoked, always
 *
 * Every frame becomes a `Blob` URL and every one of them leaks until revoked. At 5 fps that is
 * 300 leaked blobs a minute on a phone, which is the kind of bug that looks like "the app gets
 * slow after a while". The previous URL is revoked as each new frame replaces it, and both are
 * revoked on unmount.
 */
export interface LiveViewImage {
  /** Blob URL for the JPEG. */
  url: string;
  /** `Date.now()` when it arrived — what the surface's stall detection measures against. */
  at: number;
  /** The frame previewed, on a preview. */
  frameId?: string;
}

export interface LiveViewState {
  /** The most recent camera live-view frame. */
  live: LiveViewImage | null;
  /** The most recent capture preview. */
  preview: LiveViewImage | null;
  /** Whether the image socket is currently open. */
  connected: boolean;
}

const EMPTY: LiveViewState = { live: null, preview: null, connected: false };

/** Reconnect backoff, matching the control link's shape but its own schedule. */
const RECONNECT_MIN_MS = 500;
const RECONNECT_MAX_MS = 15_000;

export function useLiveView(token: string | null, enabled: boolean): LiveViewState {
  const [state, setState] = useState<LiveViewState>(EMPTY);

  useEffect(() => {
    if (!enabled) {
      setState(EMPTY);
      return;
    }

    let stopped = false;
    let socket: WebSocket | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let backoff = RECONNECT_MIN_MS;
    // Held outside React state so the cleanup can revoke them without depending on a render.
    const urls = { live: null as string | null, preview: null as string | null };

    function replace(slot: 'live' | 'preview', url: string) {
      const previous = urls[slot];
      urls[slot] = url;
      if (previous !== null) URL.revokeObjectURL(previous);
    }

    function scheduleReconnect() {
      if (stopped) return;
      timer = setTimeout(() => {
        void connect();
      }, backoff);
      backoff = Math.min(backoff * 2, RECONNECT_MAX_MS);
    }

    async function connect(): Promise<void> {
      if (stopped) return;
      // A fresh ticket every time. §4.5 makes it single-use and the *upgrade* consumes it, so
      // reusing one would work on the first connection and fail on every reconnect.
      const result = await postJson<WsTicket>(WS_TICKET, token);
      if (stopped) return;
      if (!result.ok || result.value === null) {
        // Including 401: the control link is what tells the operator their token is wrong, and
        // two components saying so is one too many. This one just keeps trying.
        scheduleReconnect();
        return;
      }

      const ws = new WebSocket(liveviewSocketUrl(result.value.ticket));
      ws.binaryType = 'arraybuffer';
      socket = ws;

      ws.onopen = () => {
        backoff = RECONNECT_MIN_MS;
        setState((current) => ({ ...current, connected: true }));
      };
      ws.onmessage = (event) => {
        // Text on this socket is not ours — control frames belong on `/ws` (§8.3(5)).
        if (!(event.data instanceof ArrayBuffer)) return;
        const frame = decodeLiveFrame(event.data);
        if (frame === null) return;

        const url = URL.createObjectURL(new Blob([frame.jpeg], { type: 'image/jpeg' }));
        const image: LiveViewImage = {
          url,
          at: Date.now(),
          ...(frame.frameId === undefined ? {} : { frameId: frame.frameId }),
        };
        replace(frame.kind, url);
        setState((current) =>
          frame.kind === 'preview' ? { ...current, preview: image } : { ...current, live: image },
        );
      };
      ws.onclose = () => {
        setState((current) => ({ ...current, connected: false }));
        scheduleReconnect();
      };
      ws.onerror = () => {
        // `onclose` always follows, and it is where the reconnect lives. Closing here as well
        // would schedule two.
        ws.close();
      };
    }

    void connect();

    return () => {
      stopped = true;
      if (timer !== null) clearTimeout(timer);
      if (socket !== null) {
        // Detached before closing so the handler cannot schedule a reconnect on the way out.
        socket.onclose = null;
        socket.close();
      }
      if (urls.live !== null) URL.revokeObjectURL(urls.live);
      if (urls.preview !== null) URL.revokeObjectURL(urls.preview);
    };
  }, [token, enabled]);

  return state;
}
