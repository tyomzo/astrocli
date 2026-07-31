#!/usr/bin/env python3
"""wsobserve.py — record a field node's `/ws` event stream as timestamped JSON lines.

Usage:
    scripts/lib/wsobserve.py --base http://127.0.0.1:8470 --token s3cret \\
        [--seconds N] [--until-topic capture.progress] [--out events.jsonl]

Writes one JSON object per line to `--out` (default stdout):

    {"t": 12.482, "wall": "2026-07-31T18:40:02.115Z", "topic": "camera.status", "data": {...}}

`t` is seconds since this observer's own start, monotonic. It is the number the M2-T05
evidence is computed from — "preview within 3 s of the exposure ending" is a difference
between two arrival times, and a difference of monotonic readings survives an NTP step
where a difference of wall clocks does not. `wall` is there so a reader can line an event
up against `journalctl`.

# Why this file exists at all

The M2-T05 desk run has to measure *when* things happened, and the two things it measures
— the preview latency per frame, and the cable-pull recovery timings — are both event
streams rather than request/response. `curl` cannot hold a socket open, and neither
`websocat` nor Python's `websockets` is installed on the reference desk machine.

RFC 6455 is small enough that a **read-only** client is about eighty lines: an HTTP
upgrade with a base64 nonce, then a frame loop that only has to handle text, ping and
close. Nothing here masks a data frame because nothing here sends one, and the whole
thing is deliberately not general — see `close()` for what it does not implement.

Taking a dependency instead would mean either a pip install on the operator's machine
(which the repo asks nowhere else) or vendoring a library to read a socket for two
minutes a night.
"""

import argparse
import base64
import datetime
import json
import os
import socket
import ssl
import struct
import sys
import time
import urllib.parse
import urllib.request

# ---------------------------------------------------------------------------------------------
# The ticket dance (SDD §4.5)
# ---------------------------------------------------------------------------------------------


def ws_ticket(base: str, token: str) -> str:
    """Spend the bearer token on a short-lived ticket.

    A browser cannot attach an `Authorization` header to an upgrade, so `/ws` is not
    bearer-authenticated; the token buys a ticket and the *ticket* goes in the query string.
    This client could have sent the header — but then it would be testing a path the PWA
    never takes, which is the opposite of what a desk run is for.
    """
    request = urllib.request.Request(
        f"{base}/api/auth/ws-ticket",
        method="POST",
        data=b"",
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)["ticket"]


# ---------------------------------------------------------------------------------------------
# RFC 6455, the reading half
# ---------------------------------------------------------------------------------------------

OPCODE_TEXT = 0x1
OPCODE_BINARY = 0x2
OPCODE_CLOSE = 0x8
OPCODE_PING = 0x9
OPCODE_PONG = 0xA


class WebSocket:
    """A read-only RFC 6455 client. Not general: see the module docstring."""

    def __init__(self, url: str, timeout: float = 65.0):
        parts = urllib.parse.urlsplit(url)
        secure = parts.scheme in ("wss", "https")
        port = parts.port or (443 if secure else 80)
        raw = socket.create_connection((parts.hostname, port), timeout=timeout)
        if secure:
            # The desk run uses a self-signed certificate when it uses TLS at all, and this
            # observer is not the thing that should be asserting the node's identity — the
            # bearer token it already spent is.
            context = ssl._create_unverified_context()  # noqa: S323
            raw = context.wrap_socket(raw, server_hostname=parts.hostname)
        self.sock = raw
        self.buffer = b""

        path = parts.path or "/"
        if parts.query:
            path = f"{path}?{parts.query}"
        nonce = base64.b64encode(os.urandom(16)).decode()
        handshake = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {parts.hostname}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {nonce}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        self.sock.sendall(handshake.encode())

        while b"\r\n\r\n" not in self.buffer:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("the node closed the connection during the upgrade")
            self.buffer += chunk
        head, _, rest = self.buffer.partition(b"\r\n\r\n")
        status = head.split(b"\r\n", 1)[0].decode(errors="replace")
        if "101" not in status:
            raise ConnectionError(f"the upgrade was refused: {status}\n{head.decode('replace')}")
        self.buffer = rest

    def _read(self, count: int) -> bytes:
        while len(self.buffer) < count:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("the node closed the connection")
            self.buffer += chunk
        head, self.buffer = self.buffer[:count], self.buffer[count:]
        return head

    def recv(self):
        """The next frame as `(opcode, payload)`, reassembling continuations."""
        opcode = None
        payload = b""
        while True:
            first, second = self._read(2)
            final = bool(first & 0x80)
            this_opcode = first & 0x0F
            masked = bool(second & 0x80)
            length = second & 0x7F
            if length == 126:
                (length,) = struct.unpack("!H", self._read(2))
            elif length == 127:
                (length,) = struct.unpack("!Q", self._read(8))
            # A server must not mask, but reading the key anyway costs four bytes and turns a
            # spec violation into a decode rather than a desynchronised stream.
            mask = self._read(4) if masked else None
            body = self._read(length)
            if mask:
                body = bytes(b ^ mask[i % 4] for i, b in enumerate(body))

            if this_opcode in (OPCODE_PING, OPCODE_PONG, OPCODE_CLOSE):
                return this_opcode, body
            if opcode is None:
                opcode = this_opcode
            payload += body
            if final:
                return opcode, payload

    def pong(self, payload: bytes) -> None:
        """Answer a ping. Client frames are masked; this is the only frame this client sends."""
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        header = bytes([0x80 | OPCODE_PONG])
        length = len(masked)
        if length < 126:
            header += bytes([0x80 | length])
        elif length < 65536:
            header += bytes([0x80 | 126]) + struct.pack("!H", length)
        else:
            header += bytes([0x80 | 127]) + struct.pack("!Q", length)
        self.sock.sendall(header + mask + masked)

    def close(self) -> None:
        # No close handshake: this observer is always the one being torn down, the node's own
        # tests cover a clean client close, and a half-second spent waiting for a close frame
        # at the end of a soak is a half-second of nothing.
        try:
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="e.g. http://127.0.0.1:8470")
    parser.add_argument("--token", default=os.environ.get("ASTROCTL_TOKEN", ""))
    parser.add_argument("--path", default="/ws")
    parser.add_argument("--seconds", type=float, default=0.0, help="0 = until killed")
    parser.add_argument(
        "--until-topic",
        default="",
        help="stop after this topic is seen this many times (--until-count)",
    )
    parser.add_argument("--until-count", type=int, default=1)
    parser.add_argument("--out", default="-")
    parser.add_argument(
        "--echo",
        action="store_true",
        help="also print a one-line human summary to stderr as events arrive",
    )
    args = parser.parse_args()

    ticket = ws_ticket(args.base, args.token)
    scheme = "wss" if args.base.startswith("https") else "ws"
    host = args.base.split("://", 1)[1]
    url = f"{scheme}://{host}{args.path}?ticket={urllib.parse.quote(ticket)}"

    sink = sys.stdout if args.out == "-" else open(args.out, "w", buffering=1)
    started = time.monotonic()
    deadline = started + args.seconds if args.seconds > 0 else None
    seen = 0

    ws = WebSocket(url)
    # Announced on stderr so a caller that is waiting for the socket to be live can block on it
    # rather than on a sleep — the desk run starts the observer *before* the captures.
    print("wsobserve: connected", file=sys.stderr, flush=True)
    try:
        while True:
            if deadline is not None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                ws.sock.settimeout(max(0.1, remaining))
            try:
                opcode, payload = ws.recv()
            except (TimeoutError, socket.timeout):
                continue
            except (ConnectionError, OSError) as error:
                print(f"wsobserve: {error}", file=sys.stderr, flush=True)
                break

            if opcode == OPCODE_CLOSE:
                print("wsobserve: the node closed the stream", file=sys.stderr, flush=True)
                break
            if opcode == OPCODE_PING:
                ws.pong(payload)
                continue
            if opcode == OPCODE_BINARY:
                # `/ws/liveview` frames. Recorded by size only: a JSONL file with 190 KB of
                # base64 per frame is not a file anyone reads.
                record = {"topic": "liveview.frame", "data": {"bytes": len(payload)}}
            else:
                try:
                    record = json.loads(payload.decode("utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError) as error:
                    record = {"topic": "unparsed", "data": {"error": str(error)}}

            now = time.monotonic()
            line = {
                "t": round(now - started, 4),
                "wall": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "topic": record.get("topic", "?"),
                "data": record.get("data", record),
            }
            sink.write(json.dumps(line) + "\n")
            if args.echo:
                print(f"  {line['t']:8.3f}  {line['topic']}", file=sys.stderr, flush=True)

            if args.until_topic and line["topic"] == args.until_topic:
                seen += 1
                if seen >= args.until_count:
                    break
    finally:
        ws.close()
        if sink is not sys.stdout:
            sink.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
