#!/usr/bin/env python3
"""
HEQ5 motion harness — executes MOTION-PLAN.md experiments against real hardware.

SAFETY ARCHITECTURE (see MOTION-PLAN.md "The software fence"):

  * Every experiment declares the exact opcodes it may emit. Anything else is refused
    at the write gate, before it reaches the wire.
  * During motion a fence task polls :j; if a counter departs its start by more than
    `fence` counts, L (instant stop) then K go to BOTH axes and the run aborts.
  * Every run is bounded by a wall-clock deadline as a second, independent backstop.
  * Every byte in and out is logged to out/<experiment>.log for post-hoc analysis.

Usage:  python3 motion.py E1 [E2 ...]      (no default — you must name experiments)
"""

import os, sys, termios, time, json

PORT = "/dev/ttyUSB0"
OUT = "out"
CPR = 9_024_000
HOME = 0x800000
SIDEREAL_CPS = CPR / 86164.0905
FENCE = 20_000          # counts; ~0.8 degrees

log_lines = []


def log(msg):
    print(msg)
    log_lines.append(msg)


# ---------------------------------------------------------------- transport

class Link:
    def __init__(self, path=PORT, allowed=frozenset()):
        self.allowed = set(allowed)
        self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        a = termios.tcgetattr(self.fd)
        a[0] = a[1] = a[3] = 0
        a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
        a[4] = a[5] = termios.B9600
        cc = list(a[6]); cc[termios.VMIN] = 0; cc[termios.VTIME] = 0; a[6] = cc
        termios.tcsetattr(self.fd, termios.TCSANOW, a)
        termios.tcflush(self.fd, termios.TCIOFLUSH)

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass

    def cmd(self, opcode, axis, payload="", timeout=0.5):
        if opcode not in self.allowed:
            raise SystemExit(f"REFUSED opcode {opcode!r} — not in this experiment's allowlist "
                             f"{sorted(self.allowed)}")
        frame = f":{opcode}{axis}{payload}\r".encode()
        termios.tcflush(self.fd, termios.TCIFLUSH)
        t0 = time.monotonic()
        os.write(self.fd, frame)
        buf = b""
        while time.monotonic() - t0 < timeout:
            try:
                c = os.read(self.fd, 64)
            except BlockingIOError:
                c = b""
            if c:
                buf += c
                if b"\r" in buf:
                    break
            else:
                time.sleep(0.001)
        reply = buf.split(b"\r")[0] if b"\r" in buf else None
        return reply.decode() if reply else None


def enc_u24(v):
    """Synta 24-bit: ASCII hex, low byte first. 0x123456 -> '563412'."""
    s = f"{v & 0xFFFFFF:06X}"
    return s[4:6] + s[2:4] + s[0:2]


def dec_u24(p):
    return int(p[4:6] + p[2:4] + p[0:2], 16) if p and len(p) >= 6 else None


def pos(link, axis):
    r = link.cmd("j", axis)
    return dec_u24(r[1:]) if r and r.startswith("=") else None


def status(link, axis):
    r = link.cmd("f", axis)
    if not r or not r.startswith("="):
        return None
    n = r[1:]
    if len(n) < 3:
        return {"raw": r}
    n1, n2, n3 = ord(n[0]), ord(n[1]), ord(n[2])
    return {
        "raw": n,
        "mode": "SLEW" if n1 & 0x01 else "GOTO",
        "dir": "BACKWARD" if n1 & 0x02 else "FORWARD",
        "speed": "HIGH" if n1 & 0x04 else "LOW",
        "running": bool(n2 & 0x01),
        "initialized": bool(n3 & 0x01),
    }


def emergency(link, why):
    """Independent of experiment logic. Best effort, both axes, L then K."""
    log(f"\n  *** EMERGENCY STOP: {why} ***")
    for op in ("L", "K"):
        for ax in ("1", "2"):
            try:
                link.allowed.add(op)
                r = link.cmd(op, ax, timeout=0.4)
                log(f"      :{op}{ax} -> {r!r}")
            except Exception as e:
                log(f"      :{op}{ax} FAILED: {e}")


def watch(link, axis, start, deadline_s, fence=FENCE, settle=0.4):
    """Poll until motion stops, the fence trips, or the deadline expires.
    Returns (samples, verdict)."""
    samples = []
    t0 = time.monotonic()
    last_change = t0
    last = start
    while True:
        now = time.monotonic()
        p = pos(link, axis)
        if p is None:
            continue
        samples.append((now - t0, p))
        if abs(p - start) > fence:
            emergency(link, f"FENCE: axis {axis} moved {p-start:+} counts (limit {fence})")
            return samples, "FENCE"
        if p != last:
            last, last_change = p, now
        elif now - last_change > settle and len(samples) > 5:
            return samples, "STOPPED"
        if now - t0 > deadline_s:
            emergency(link, f"DEADLINE: {deadline_s}s elapsed, still moving")
            return samples, "DEADLINE"


def save(name, payload=None):
    os.makedirs(OUT, exist_ok=True)
    with open(f"{OUT}/{name}.log", "w") as f:
        f.write("\n".join(log_lines) + "\n")
    if payload is not None:
        with open(f"{OUT}/{name}.json", "w") as f:
            json.dump(payload, f, indent=2)


# ---------------------------------------------------------------- experiments

def E1():
    """F initialise, both axes. Prediction: no counter movement; status -> initialized."""
    log("=== E1 · F initialise ===")
    link = Link(allowed={"j", "f", "F"})
    try:
        for ax in ("1", "2"):
            before_p, before_s = pos(link, ax), status(link, ax)
            log(f"\naxis {ax} before: pos={before_p} status={before_s}")
            t0 = time.monotonic()
            r = link.cmd("F", ax)
            dt = (time.monotonic() - t0) * 1000
            log(f"  :F{ax} -> {r!r}  ({dt:.0f} ms)")
            time.sleep(0.3)
            after_p, after_s = pos(link, ax), status(link, ax)
            log(f"axis {ax} after : pos={after_p} status={after_s}")
            moved = (after_p is not None and before_p is not None and after_p != before_p)
            log(f"  MOVED: {'** YES — ABORT **' if moved else 'no'}")
            if moved:
                emergency(link, "F caused motion")
                return False
            log(f"  initialized: {before_s['initialized'] if before_s else '?'}"
                f" -> {after_s['initialized'] if after_s else '?'}")
        return True
    finally:
        save("E1"); link.close()


def E3(increment=1000, period=620, axis="1", fence=FENCE, deadline=None):
    """Smallest self-terminating goto. K is NOT sent — the mount must stop itself."""
    log(f"=== E3 · bounded goto: axis {axis}, +{increment} counts, step period {period} ===")
    link = Link(allowed={"j", "f", "G", "I", "H", "M", "J"})
    try:
        start = pos(link, axis)
        st = status(link, axis)
        log(f"start pos={start} status={st}")
        if not st or not st["initialized"]:
            log("  axis not initialized — run E1 first"); return False

        seq = [("G", "20"), ("I", enc_u24(period)),
               ("H", enc_u24(increment)), ("M", enc_u24(max(increment // 2, 1)))]
        for op, payload in seq:
            r = link.cmd(op, axis, payload)
            log(f"  :{op}{axis}{payload} -> {r!r}")
            if r is None or r.startswith("!"):
                log("  *** command rejected — aborting before J ***"); return False

        predicted = increment / (64935 / period)
        log(f"\n  predicted duration at timer_freq 64935: {predicted:.1f} s"
            f"   (at 460800 it would be {increment/(460800/period):.1f} s)")
        log("  sending :J — motion starts, K will NOT be sent\n")
        t_start = time.monotonic()
        r = link.cmd("J", axis)
        log(f"  :J{axis} -> {r!r}")
        samples, verdict = watch(link, axis, start, deadline_s=(deadline or max(predicted*3,30)), fence=fence)
        elapsed = time.monotonic() - t_start

        end = pos(link, axis)
        log(f"\n  verdict   : {verdict}")
        log(f"  elapsed   : {elapsed:.2f} s   ({len(samples)} samples)")
        log(f"  start->end: {start} -> {end}   delta {end-start:+} counts (target {increment:+})")
        log(f"  goto error: {end - start - increment:+} counts")
        log(f"  end status: {status(link, axis)}")
        save(f"E3_axis{axis}", {"start": start, "end": end, "increment": increment,
                                "period": period, "elapsed": elapsed,
                                "verdict": verdict, "samples": samples})
        return verdict == "STOPPED"
    finally:
        save(f"E3_axis{axis}"); link.close()



def _stop_test(op, increment=10000, axis="1", fence=30000):
    """E7/E8: bounded goto, interrupt at half travel with `op`. If `op` does nothing the
    mount still stops at the target — the failure mode is disarmed by construction."""
    log(f"=== {'E7' if op=='K' else 'E8'} · :{op} mid-travel, axis {axis}, +{increment} counts ===")
    link = Link(allowed={"j", "f", "G", "I", "H", "M", "J", op, "K", "L"})
    try:
        start = pos(link, axis)
        for o, pl in [("G","20"), ("I",enc_u24(620)), ("H",enc_u24(increment)), ("M",enc_u24(increment//2))]:
            r = link.cmd(o, axis, pl)
            if r is None or r.startswith("!"):
                log(f"  :{o} rejected -> {r!r}"); return False
        log(f"start={start}, target={start+increment}, interrupting at +{increment//2}")
        link.cmd("J", axis)
        t0 = time.monotonic()
        sent_at = None; t_sent = None; samples = []
        while True:
            now = time.monotonic() - t0
            p = pos(link, axis)
            if p is None: continue
            samples.append((now, p))
            d = p - start
            if abs(d) > fence:
                emergency(link, f"FENCE {d:+}"); return False
            if sent_at is None and abs(d) >= increment // 2:
                t_sent = time.monotonic()
                r = link.cmd(op, axis)
                sent_at = p
                log(f"  :{op}{axis} sent at delta {d:+} -> {r!r}")
            if sent_at is not None and len(samples) > 3:
                recent = [x for t, x in samples[-6:]]
                if now - (t_sent - t0) > 0.5 and len(set(recent)) == 1:
                    break
            if now > 20:
                emergency(link, "deadline"); return False
        end = pos(link, axis)
        overshoot = end - sent_at
        reached_target = (end - start) >= increment
        log(f"  stopped at {end}  (delta {end-start:+} of {increment:+} commanded)")
        log(f"  OVERSHOOT after :{op}: {overshoot:+} counts")
        log(f"  {'*** reached target — the stop did NOT act ***' if reached_target else f':{op} ARRESTED the motion'}")
        log(f"  end status: {status(link, axis)}")
        save(f"{'E7' if op=='K' else 'E8'}", {"start": start, "end": end, "sent_at": sent_at,
             "overshoot": overshoot, "increment": increment, "arrested": not reached_target,
             "samples": samples})
        return True
    finally:
        link.close()

def E7(): return _stop_test("K")
def E8(): return _stop_test("L")


def E10(period=620, axis="1", duration=30.0, fence=20000):
    """SLEW-mode rate. Unbounded motion — requires K proven (E7). This is the experiment
    that settles the timer-frequency question, since GOTO ignores the step period."""
    log(f"=== E10 · SLEW rate, axis {axis}, step period {period}, up to {duration}s ===")
    link = Link(allowed={"j", "f", "G", "I", "J", "K", "L"})
    try:
        start = pos(link, axis)
        for o, pl in [("G","10"), ("I",enc_u24(period))]:      # "10" = SLEW, low speed, forward
            r = link.cmd(o, axis, pl)
            log(f"  :{o}{axis}{pl} -> {r!r}")
            if r is None or r.startswith("!"): return False
        pred = 64935/period
        log(f"  predicted rate if timer_freq=64935: {pred:.2f} c/s"
            f"   (if 460800: {460800/period:.1f} c/s)")
        log("  UNBOUNDED SLEW — K proven by E7; fence and deadline armed\n")
        link.cmd("J", axis)
        t0=time.monotonic(); samples=[]; tripped=False
        while time.monotonic()-t0 < duration:
            p = pos(link, axis)
            if p is None: continue
            samples.append((time.monotonic()-t0, p))
            if abs(p-start) > fence:
                emergency(link, f"FENCE {p-start:+}"); tripped=True; break
        if not tripped:
            r = link.cmd("K", axis); log(f"  :K{axis} -> {r!r}")
        time.sleep(0.5)
        end = pos(link, axis)
        # linear fit over the middle 70% (skip ramps)
        n=len(samples); lo,hi=int(n*0.15), int(n*0.85)
        seg=samples[lo:hi]
        if len(seg)>10:
            xs=[t for t,_ in seg]; ys=[p for _,p in seg]
            mx=sum(xs)/len(xs); my=sum(ys)/len(ys)
            slope=sum((x-mx)*(y-my) for x,y in zip(xs,ys))/sum((x-mx)**2 for x in xs)
        else: slope=float("nan")
        SID=CPR/86164.0905
        log(f"  samples {n}, travelled {end-start:+} counts")
        log(f"  MEASURED RATE: {slope:.3f} counts/s  ({slope/SID:.3f}x sidereal)")
        log(f"  implied timer_freq = period x rate = {period*slope:,.0f}")
        log(f"    vs 64,935 -> {'MATCH' if abs(period*slope-64935)/64935<0.05 else 'MISMATCH'}")
        log(f"  final status: {status(link, axis)}")
        save("E10", {"period":period,"rate":slope,"start":start,"end":end,"samples":samples})
        return not tripped
    finally:
        try: link.allowed.add("K"); link.cmd("K", axis)
        except Exception: pass
        link.close()

EXPERIMENTS = {"E1": E1, "E3": E3, "E7": E7, "E8": E8, "E10": E10}

if __name__ == "__main__":
    names = sys.argv[1:]
    if not names:
        raise SystemExit("name the experiments to run, e.g.: motion.py E1")
    for n in names:
        if n not in EXPERIMENTS:
            raise SystemExit(f"unknown experiment {n}; have {sorted(EXPERIMENTS)}")
        ok = EXPERIMENTS[n]()
        log(f"\n--- {n}: {'PASS' if ok else 'STOPPED/FAILED'} ---\n")
        if not ok:
            raise SystemExit(1)
