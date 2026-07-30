"""A worker built against a protocol version this backbone cannot speak.

The drift ADR-13 exists to catch, made reproducible. Everything else about this worker is
correct — it handshakes promptly and answers pings — so a test using it isolates the version
check from every other reason a start can fail.

`ipc.hello()` is deliberately not used: it stamps the version this repository agrees on, which
is the one thing this file must not do.
"""

from __future__ import annotations

import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.normpath(os.path.join(_HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(_ROOT, "workers"))

# Imported after the bootstrap above, which is what puts it on the path.
import astroctl_ipc as ipc

# Far enough from any real version that no future bump can accidentally make this test pass.
FOREIGN_PROTO_VERSION = ipc.PROTO_VERSION + 98


def main():
    channel = ipc.Channel.open()
    try:
        for message in channel.frames():
            if message["type"] == "hello":
                channel.send(
                    {
                        "type": "hello",
                        "proto_version": FOREIGN_PROTO_VERSION,
                        "capabilities": ipc.capabilities(),
                    }
                )
            elif message["type"] == "ping":
                channel.send(ipc.pong(message["nonce"]))
            elif message["type"] == "shutdown":
                return 0
        return 0
    finally:
        channel.close()


if __name__ == "__main__":
    raise SystemExit(main())
