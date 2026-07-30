"""A worker that handshakes and then answers nothing.

This is what a wedged worker looks like from outside: the process is alive, its stdin is being
drained so no write to it ever blocks, and nothing comes back. It is the case the ping cadence
of SDD §5.12.3 exists for — a crash is announced by the pipe closing, but a worker stuck in a
CUDA call or a deadlocked thread announces nothing at all.

Stdin is deliberately still read. A worker that stopped reading would fill the pipe and trip the
supervisor's *write* timeout instead, and then the test would be passing for the wrong reason.
"""

from __future__ import annotations

import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
_ROOT = os.path.normpath(os.path.join(_HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(_ROOT, "workers"))

# Imported after the bootstrap above, which is what puts it on the path.
import astroctl_ipc as ipc


def main():
    channel = ipc.Channel.open()
    try:
        for message in channel.frames():
            if message["type"] == "hello":
                channel.send(ipc.hello())
            # Every other frame — pings and jobs alike — is read and dropped.
        return 0
    finally:
        channel.close()


if __name__ == "__main__":
    raise SystemExit(main())
