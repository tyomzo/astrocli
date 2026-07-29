#!/usr/bin/env python3
"""
T-SER-4 — read-only protocol surface and resilience survey against a real HEQ5.

Closes the Tier-1 unknowns that do not require motion: the high-speed ratio, the `!` error
frame, the mount's true inquiry surface, sustained-polling behaviour, malformed-frame
recovery, and port reopen.

SAFETY — enforced by construction, not by convention:

  In the Synta protocol lowercase opcodes are inquiries and uppercase opcodes are actions
  (F initialise, G motion mode, S goto target, I step period, J start, K stop, L instant
  stop, P guide rate). This script therefore refuses to transmit ANY uppercase byte. The
  gate is applied to the raw byte stream, so even a deliberately malformed frame cannot
  align an uppercase opcode into the parser.

  Every probe is bracketed by a position read on both axes. If a counter moves by even one
  count, the survey aborts immediately.

Usage:  python3 survey.py [/dev/ttyUSB0]
"""

import os, sys, termios, time

PORT = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyUSB0"
SAFE_BYTES = set(b":\r" + b"abcdefghijklmnopqrstuvwxyz" + b"0123456789")
COUNTS_HOME = 0x800000


class Moved(Exception):
    pass


def open_port(path):
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    a = termios.tcgetattr(fd)
    a[0] = 0                                   # iflag
    a[1] = 0                                   # oflag
    a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    a[3] = 0                                   # lflag
    a[4] = a[5] = termios.B9600
    cc = list(a[6]); cc[termios.VMIN] = 0; cc[termios.VTIME] = 0; a[6] = cc
    termios.tcsetattr(fd, termios.TCSANOW, a)
    termios.tcflush(fd, termios.TCIOFLUSH)
    return fd


def write_guarded(fd, raw: bytes):
    """The safety gate. No uppercase byte ever reaches the wire."""
    bad = [b for b in raw if b not in SAFE_BYTES]
    if bad:
        raise SystemExit(f"REFUSED to transmit unsafe bytes {bad!r} in {raw!r}")
    os.write(fd, raw)


def exchange_raw(fd, raw: bytes, timeout=0.5):
    termios.tcflush(fd, termios.TCIFLUSH)
    t0 = time.monotonic()
    write_guarded(fd, raw)
    buf = b""
    while time.monotonic() - t0 < timeout:
        try:
            c = os.read(fd, 64)
        except BlockingIOError:
            c = b""
        if c:
            buf += c
            if b"\r" in buf:
                break
        else:
            time.sleep(0.001)
    ms = (time.monotonic() - t0) * 1000
    return (buf.split(b"\r")[0] if b"\r" in buf else None), ms, buf


def cmd(fd, opcode_and_args, timeout=0.5):
    return exchange_raw(fd, (":" + opcode_and_args + "\r").encode(), timeout)


def swap24(p):
    return int(p[4:6] + p[2:4] + p[0:2], 16) if len(p) >= 6 else None


def read_pos(fd):
    out = []
    for ax in ("1", "2"):
        raw, _, _ = cmd(fd, "j" + ax)
        out.append(swap24(raw.decode()[1:]) if raw and raw.startswith(b"=") else None)
    return tuple(out)


def assert_still(fd, baseline, context):
    now = read_pos(fd)
    if now != baseline:
        raise Moved(f"POSITION CHANGED during {context}: {baseline} -> {now}")
    return now


# ------------------------------------------------------------------ probes

def probe_high_speed_ratio(fd):
    print("\n[1] high-speed ratio  (PRD §4.2 assumes ~16x, unverified)")
    for ax in ("1", "2"):
        raw, ms, _ = cmd(fd, "g" + ax)
        if raw is None:
            print(f"  axis {ax}: NO RESPONSE ({ms:.0f} ms)")
        elif raw.startswith(b"!"):
            print(f"  axis {ax}: error frame {raw.decode()!r} — :g not supported")
        else:
            p = raw.decode()[1:]
            as_hex = int(p, 16) if all(c in "0123456789abcdefABCDEF" for c in p) else None
            print(f"  axis {ax}: {p!r}  -> as-transmitted {as_hex}  byte-swapped {swap24(p)}")


def probe_error_frame(fd):
    print("\n[2] `!` error frame  (undefined lowercase opcode; T-COD-1 has no real sample)")
    for op in ("z1", "y1", "j9"):
        raw, ms, buf = cmd(fd, op)
        shown = raw.decode() if raw else f"<no CR: {buf!r}>"
        print(f"  :{op:3} -> {shown!r}  ({ms:.0f} ms)")


def probe_inquiry_sweep(fd, baseline):
    print("\n[3] lowercase inquiry sweep a-z  (mapping the real surface vs the SDD table)")
    known = set("eabjf")
    found, errors, silent = [], [], []
    for ch in "abcdefghijklmnopqrstuvwxyz":
        raw, ms, _ = cmd(fd, ch + "1", timeout=0.3)
        baseline = assert_still(fd, baseline, f":{ch}1")
        if raw is None:
            silent.append(ch)
        elif raw.startswith(b"!"):
            errors.append(ch)
        else:
            payload = raw.decode()[1:]
            found.append((ch, payload))
            flag = "" if ch in known else "   <-- NOT IN SDD §5.2.2 TABLE"
            print(f"  :{ch}1 = {payload!r}{flag}")
    print(f"  supported: {''.join(c for c, _ in found)}")
    print(f"  error(!):  {''.join(errors) or '-'}")
    print(f"  silent:    {''.join(silent) or '-'}")
    return baseline


def probe_sustained(fd, n=2000):
    print(f"\n[4] sustained polling — {n} x :j1  (heartbeat threshold is currently a guess)")
    lat, timeouts, malformed = [], 0, 0
    t0 = time.monotonic()
    for _ in range(n):
        raw, ms, _ = cmd(fd, "j1", timeout=0.5)
        if raw is None:
            timeouts += 1
        elif not raw.startswith(b"="):
            malformed += 1
        else:
            lat.append(ms)
    total = time.monotonic() - t0
    lat.sort()
    pct = lambda p: lat[min(int(len(lat) * p), len(lat) - 1)]
    print(f"  {len(lat)} ok, {timeouts} timeouts, {malformed} malformed, over {total:.1f} s")
    print(f"  min {lat[0]:.1f}  p50 {pct(.5):.1f}  p99 {pct(.99):.1f}  max {lat[-1]:.1f} ms")
    print(f"  effective rate {n/total:.1f} exchanges/s")


def probe_malformed(fd, baseline):
    print("\n[5] malformed frames  (T-SER-1 tests a mock; this is the real device)")
    cases = [
        (b":j\r",        "missing axis digit"),
        (b":\r",         "bare colon"),
        (b":j1",         "no terminator (then a good command follows)"),
        (b"zzz:j1\r",    "junk prefix — does it resync on ':'?"),
        (b":j1\r:j1\r",  "two frames in one write"),
    ]
    for raw_bytes, label in cases:
        raw, ms, buf = exchange_raw(fd, raw_bytes, timeout=0.4)
        shown = raw.decode() if raw else f"<none: {buf!r}>"
        print(f"  {label:44} -> {shown!r} ({ms:.0f} ms)")
        # recovery: a normal command must work immediately afterwards
        rec, rms, _ = cmd(fd, "j1")
        ok = rec is not None and rec.startswith(b"=")
        print(f"  {'':44}    recovery {'OK' if ok else 'FAILED'} ({rms:.0f} ms)")
        baseline = assert_still(fd, baseline, label)
    return baseline


def probe_reopen(fd_holder):
    print("\n[6] port close/reopen  (half of the REL-03 recovery path)")
    os.close(fd_holder[0])
    time.sleep(0.2)
    t0 = time.monotonic()
    fd_holder[0] = open_port(PORT)
    raw, ms, _ = cmd(fd_holder[0], "j1")
    ok = raw is not None and raw.startswith(b"=")
    print(f"  reopened in {(time.monotonic()-t0)*1000:.0f} ms; first exchange {'OK' if ok else 'FAILED'} ({ms:.0f} ms)")


def main():
    print("=== T-SER-4 — read-only protocol survey ===")
    print(f"port {PORT}; uppercase bytes are refused at the write gate; position guarded per probe")
    holder = [open_port(PORT)]
    try:
        fd = holder[0]
        baseline = read_pos(fd)
        print(f"\nbaseline position: RA={baseline[0]} DEC={baseline[1]}"
              f"  (home={COUNTS_HOME}, offset {baseline[0]-COUNTS_HOME:+} / {baseline[1]-COUNTS_HOME:+})")
        probe_high_speed_ratio(fd);         baseline = assert_still(fd, baseline, "step 1")
        probe_error_frame(fd);              baseline = assert_still(fd, baseline, "step 2")
        baseline = probe_inquiry_sweep(fd, baseline)
        probe_sustained(fd);                baseline = assert_still(fd, baseline, "step 4")
        baseline = probe_malformed(fd, baseline)
        probe_reopen(holder)
        final = read_pos(holder[0])
        print(f"\nfinal position: RA={final[0]} DEC={final[1]}")
        print("MOTION CHECK:", "PASS — counters never moved" if final == baseline else f"** MOVED ** {baseline} -> {final}")
    except Moved as e:
        print(f"\n!!! ABORTED: {e}")
        sys.exit(1)
    finally:
        try:
            os.close(holder[0])
        except OSError:
            pass
    print("\n=== end — inquiries only, nothing was commanded to move ===")


if __name__ == "__main__":
    main()
