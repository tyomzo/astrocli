# M1 demo — the walking skeleton, on a phone

The M1 exit demo of IMP §2: two nodes, an operator's phone, and one unbroken path from *connect*
to *a stacked preview on the screen*. Twenty minutes, no hardware.

`scripts/demo-m1.sh` brings it up and prints the URL, the token and the same walkthrough in
short form. This file is the long form: what to do, what to say, what to point at, and — the part
that makes a demo worth giving — what to break on purpose.

Everything below is asserted automatically by `scripts/e2e.sh`. If the demo does something this
file does not describe, one of the two is wrong and the suite is the tie-breaker.

---

## Before you start

```
scripts/demo-m1.sh
```

It builds both images, starts the pair, waits for both nodes to answer, and prints a URL and a
token with QR codes for each. Two things worth knowing before an audience is watching:

* **The first run builds two container images** and takes a few minutes. `--no-build` skips it
  once they exist. Do the build before people arrive.
* **The URL is the workstation's LAN address**, not `localhost`, because the phone has to reach
  it. The script guesses from the default route; `--address` overrides when the guess is wrong,
  which it will be on a machine with a VPN and a wired interface and opinions.

The phone needs to be on the same network. In the field this is the VPN of PRD §7; for a demo,
the same Wi-Fi is enough and is not a cheat — nothing in the path below cares which it is.

> **Why there is no `?token=` in the URL.** The PWA asks for the token and keeps it in local
> storage. Putting it in the URL would put it in every access log and browser history between the
> workstation and the phone, which is exactly what SEC-02's shared secret must not do. The second
> QR code is the compromise: any scanner reads it as text and the phone pastes it.

---

## 1. Connect — "nothing is moving, and that is deliberate"

Open the URL, paste the token. The app comes up on the operating view.

Tap **Connect** on the mount, then on the camera.

**Watch:** the pointing readout starts updating once a second. The camera panel fills in battery
and card space. The link badge goes green.

**Say:** the node has been running since before the phone connected, and neither device was
connected until just now. Switching on a field node must never produce motion — SDD §8.1 makes
connecting an explicit operator action, so a power cut at 3 a.m. followed by a restart does not
slew a telescope in the dark.

**Point at:** the position readout between events. It keeps moving smoothly at 1 Hz because the
PWA dead-reckons between them (M1-T15) and marks the value *predicted* rather than *confirmed* —
so a link that stops delivering shows as a stale reading rather than a frozen one that looks live.

---

## 2. Goto — "the limits are the product, not an obstacle"

Enter a target and slew.

**Watch:** status goes `idle` → `slewing` → `idle`. The readout tracks the whole way; it does not
jump at the end.

**Say:** the slew outlives the request. The API answered in milliseconds with a correlation id and
the tube took the next half-minute; the operator's phone can go to sleep, drop off the VPN and
come back, and the slew is still running and still reported.

> **Pick a declination above +45°** if the demo site is northern. The node refuses any target
> below 15° altitude — `LIMIT_ALTITUDE`, from `mount.limits.min_altitude_degrees` — and from Oslo
> anything near the celestial equator spends most of the day under that line. This is the safety
> layer working exactly as designed and it is *very* hard to explain calmly if it surprises you.
> Demonstrating it on purpose is a better move than meeting it by accident: try dec −20°, watch
> the refusal, and point out that the mount never moved.

**Optional, and it lands well:** press the red e-stop mid-slew. Motion stops. The e-stop is the
one route in the system exempt from the staleness and command-envelope rules, because a stop must
never be refused for being late.

---

## 3. Capture — "durable before it is anywhere else"

Take a frame.

**Watch:** the capture strip goes `exposing` → `downloading` → `saved`.

**Say:** at `saved` the frame is on the field node's own disk, fsynced, under a name that cannot
collide with any other — and *nothing has been sent anywhere yet*. That ordering is the whole of
REL-04: the node that took the frame is the node responsible for not losing it, and everything
downstream is an optimisation on top of a frame that is already safe.

**Point at:** the frame counter. It is per session and it never rewinds, even across a crash —
which is the thing step 5 will demonstrate.

---

## 4. Stack — "one image surface, two sources"

Switch the image surface to **STACK**.

**Watch:** the frame uploads, is acknowledged, and a preview appears — stretched, not the raw
frame.

**Say:** that preview was made on the *other machine*. The field node uploaded the frame over the
link, the stacking server stored it, handed it to a supervised Python worker, and pushed the
result back through the field node's proxy to this phone. The phone never talks to the stacking
server — one origin, one token, one socket to hold open (ADR-07).

**The number:** about five seconds from pressing capture to the preview appearing, against a
ten-second budget. `scripts/e2e.sh` asserts it on every run.

Take two more. Watch them arrive in order.

---

## 5. Pull the plug — the part worth staying for

This is the demo. Everything above is a happy path; M1's actual claim is that the field node does
not care whether the stacking server exists.

```
docker compose -f deploy/compose.yaml stop stack
```

**Keep capturing.** Three more frames.

**Watch:**

* Captures complete exactly as before. Nothing hesitates.
* The queue depth climbs 1 → 2 → 3.
* The stack panel says **offline** — and keeps showing the last known frame count, not zero.
* **Exactly one alert appears.** Not one per retry, not one per frame.

**Say:** the transfer agent is retrying on a backoff the whole time and saying so once. An alert
per attempt is an alert the operator learns to ignore by the third minute, and then misses the one
that mattered. The panel showing the *last known* count rather than zero is the same instinct: a
count that dropped to zero would read as "the stacking server lost my session", which is the most
alarming thing this screen could say and would be false.

> If you want the one-line version of why this system is two nodes: the stacking server is a
> machine in a warm room that can be rebooted, upgraded or unplugged, and the thing under the sky
> keeps taking the photographs regardless.

Now bring it back:

```
docker compose -f deploy/compose.yaml start stack
```

**Watch:** the queue drains in order. Every frame is acknowledged exactly once. One recovery
alert. The previews for all three arrive.

---

## 6. Kill the field node — optional, and the strongest one

If the room is still interested:

```
docker compose -f deploy/compose.yaml kill -s SIGKILL field
docker compose -f deploy/compose.yaml start field
```

**Watch:** the phone reconnects on its own. The session is the *same* session — same id, same
frames, and the frame counter continues rather than restarting. Reconnect the mount and camera,
capture again, and the new frame gets the next id, not a reused one.

**Say:** SIGKILL, not a graceful stop — nothing got to flush. What came back is what was already
durable. A restarted node that started a *second* session beside the first would restart the frame
numbering while the old directory still held `light_00001`, and two frames would share an id; the
node asks `CURRENT` first for exactly that reason.

---

## If something goes wrong

| Symptom | Cause |
|---|---|
| The phone cannot reach the URL | Wrong address — `--address` it, or check the phone is on the same network |
| The app asks for the token again | Local storage cleared, or a different origin than last time. Scan the second QR |
| A goto is refused with `LIMIT_ALTITUDE` | The target is below 15°. See step 2 — this is correct behaviour |
| A goto is refused with `BUSY` | A slew is already running. Stop it or wait |
| No preview, everything else fine | The stacking server's worker environment — `docker compose -f deploy/compose.yaml logs stack` names the missing dependency |
| The pair will not start | `docker compose -f deploy/compose.yaml logs`. A config error names the key |

To be sure before an audience rather than during one:

```
scripts/e2e.sh
```

It drives everything above against the same pair and asserts each step, including the fault
scenarios of §5 and §6. If it is green, the demo works.

Stop everything with `scripts/dev-down.sh`. Volumes survive, so the next run continues the same
session — `--volumes` if you want a clean first-light demo.

---

## What this demo does not show

Worth saying out loud, because an audience will ask and the honest answer is better than a vague
one.

* **No hardware.** Simulated mount and camera throughout. The mount's timings are the measured
  HEQ5 and the camera's are the measured R10, so the *pacing* is real — a capture takes two
  seconds to download because that is what the reference body takes. But no photons.
* **No stacking.** The stacking server's worker stretches one frame into a preview. It does not
  register, reject or accumulate anything; that is Phase 2b.
* **No plate solving, no guiding, no sequencing, no catalog.** Targets are entered by hand.
* **No TLS and no VPN** in this harness. Both are real-hardware gates.
