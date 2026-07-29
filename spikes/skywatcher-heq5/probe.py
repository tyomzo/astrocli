#!/usr/bin/env python3
"""
T-HIL-1 step 2 ONLY — read-only Synta/Skywatcher handshake probe.

Retires the PRD's #1 mount risk ("Skywatcher protocol documentation is incomplete or
inaccurate — mount misbehaves, gear damage possible") by reading the parameters every
later position calculation depends on, and comparing them against the values PRD §4.2
assumes. Getting CPR or timer frequency wrong is precisely how a goto drives the mount
into the tripod, so they are verified first, from a standing start, with nothing moving.

SAFETY — this script sends INQUIRY COMMANDS ONLY:

    :e  firmware version      :a  counts per revolution
    :b  timer interrupt freq  :j  position counter        :f  axis status

It NEVER sends F (initialise), G (motion mode), S (goto target), I (step period),
J (start motion), K (stop), L (instant stop) or P (guide rate). Nothing here can
command the motors. The motion steps of T-HIL-1 (steps 3-6) require the codec suite
green, the clutches loose, and an operator at the mount — they are not automated.

Stdlib only (termios), so it runs with no build and no dependencies.

Usage:  python3 probe.py [/dev/ttyUSB0]
"""

import sys, termios, time, os

PORT = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyUSB0"

# What PRD §4.2 assumes. The whole point of this probe is to confirm or correct these.
EXPECTED = {
    "cpr": 9_024_000,
    "timer_freq": 460_800,
    "counts_home": 0x800000,
}

FORBIDDEN = set("FGSIJKLP")  # motion-capable opcodes; never emitted by this script


def open_port(path):
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    attrs = termios.tcgetattr(fd)
    iflag, oflag, cflag, lflag, ispeed, ospeed, cc = attrs
    # 9600 8N1, raw, no flow control (PRD §4.2)
    iflag = 0
    oflag = 0
    cflag = termios.CS8 | termios.CREAD | termios.CLOCAL
    lflag = 0
    ispeed = ospeed = termios.B9600
    cc = list(cc)
    cc[termios.VMIN] = 0
    cc[termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, [iflag, oflag, cflag, lflag, ispeed, ospeed, cc])
    termios.tcflush(fd, termios.TCIOFLUSH)
    return fd


def exchange(fd, cmd, timeout=0.5):
    """Send one framed command, read until CR. Returns (raw, elapsed_ms) or (None, ms)."""
    if cmd[0] in FORBIDDEN:
        raise SystemExit(f"REFUSING to send motion-capable opcode {cmd[0]!r}")
    frame = (":" + cmd + "\r").encode()
    termios.tcflush(fd, termios.TCIFLUSH)
    t0 = time.monotonic()
    os.write(fd, frame)
    buf = b""
    while time.monotonic() - t0 < timeout:
        try:
            chunk = os.read(fd, 64)
        except BlockingIOError:
            chunk = b""
        if chunk:
            buf += chunk
            if b"\r" in buf:
                break
        else:
            time.sleep(0.002)
    ms = (time.monotonic() - t0) * 1000
    return (buf.split(b"\r")[0] if b"\r" in buf else None), ms


def decode_u24(payload):
    """Synta sends 24-bit values as ASCII hex, low byte first: '563412' -> 0x123456."""
    if len(payload) < 6:
        return None
    s = payload[:6]
    return int(s[4:6] + s[2:4] + s[0:2], 16)


def show(label, raw, ms, decoder=None, expect=None):
    if raw is None:
        print(f"  {label:22} NO RESPONSE ({ms:.0f} ms)  <- mount powered on?")
        return None
    text = raw.decode(errors="replace")
    if text.startswith("!"):
        print(f"  {label:22} ERROR {text!r} ({ms:.0f} ms)")
        return None
    if not text.startswith("="):
        print(f"  {label:22} UNEXPECTED FRAMING {text!r} ({ms:.0f} ms)")
        return None
    payload = text[1:]
    out = f"  {label:22} {payload!r} ({ms:.0f} ms)"
    val = None
    if decoder:
        val = decoder(payload)
        out += f" -> {val:,}" if isinstance(val, int) else ""
        if expect is not None and isinstance(val, int):
            delta = abs(val - expect) / expect * 100 if expect else 0
            out += f"   expected {expect:,}" + ("  MATCH" if delta < 1 else f"  ** DIFFERS by {delta:.1f}% **")
    print(out)
    return val


def main():
    print("=== T-HIL-1 step 2 — read-only handshake ===")
    print(f"port: {PORT}   (inquiry commands only; no motion opcode is ever sent)\n")
    try:
        fd = open_port(PORT)
    except PermissionError:
        raise SystemExit(f"cannot open {PORT}: permission denied (need dialout or an ACL)")
    except OSError as e:
        raise SystemExit(f"cannot open {PORT}: {e}")

    try:
        results = {}
        print("RA axis (1):")
        raw, ms = exchange(fd, "e1"); show("firmware version", raw, ms)
        raw, ms = exchange(fd, "a1"); results["cpr_ra"] = show("counts/revolution", raw, ms, decode_u24, EXPECTED["cpr"])
        raw, ms = exchange(fd, "b1"); results["freq_ra"] = show("timer freq", raw, ms, decode_u24, EXPECTED["timer_freq"])
        raw, ms = exchange(fd, "j1"); results["pos_ra"] = show("position counter", raw, ms, decode_u24)
        raw, ms = exchange(fd, "f1"); show("axis status", raw, ms)

        print("\nDEC axis (2):")
        raw, ms = exchange(fd, "a2"); results["cpr_dec"] = show("counts/revolution", raw, ms, decode_u24, EXPECTED["cpr"])
        raw, ms = exchange(fd, "b2"); results["freq_dec"] = show("timer freq", raw, ms, decode_u24, EXPECTED["timer_freq"])
        raw, ms = exchange(fd, "j2"); results["pos_dec"] = show("position counter", raw, ms, decode_u24)
        raw, ms = exchange(fd, "f2"); show("axis status", raw, ms)

        print("\nround-trip latency (SDD §5.2.4 budgets a 500 ms per-request timeout):")
        lat = []
        for _ in range(20):
            _, ms = exchange(fd, "j1")
            lat.append(ms)
        lat.sort()
        print(f"  20x :j1  min {lat[0]:.1f} ms  median {lat[10]:.1f} ms  max {lat[-1]:.1f} ms")

        for axis in ("ra", "dec"):
            p = results.get(f"pos_{axis}")
            if isinstance(p, int):
                off = p - EXPECTED["counts_home"]
                print(f"  {axis.upper()} counter offset from home 0x800000: {off:+,} counts")
    finally:
        os.close(fd)
    print("\n=== end — nothing was commanded to move ===")


if __name__ == "__main__":
    main()
