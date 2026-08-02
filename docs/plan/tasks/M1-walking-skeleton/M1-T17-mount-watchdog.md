# M1-T17 — Mount watchdog: link loss becomes an alert

**Milestone:** M1 (post-exit addendum) · **Depends on:** M1-T03, M1-T05 · **Crates:** astroctl-field, frontend/
**Size:** S · **Status:** done
**Spec:** PRD REL-02; SDD §5.4 (watchdog), §4.3 `alert`; config `mount.serial.heartbeat_misses`

## Why this task exists, added after M1's exit

M1-T16's fault suite injected a mount link loss during a slew and asserted **what actually
happens**: the status flips, the position stream goes silent, the REST route answers 502 — and
**no alert fires, while the tube keeps moving**. REL-02 is a Phase 1 Must. The config key that
governs it — `mount.serial.heartbeat_misses: 3`, "consecutive poll failures before watchdog
fires (REL-02)" — has been shipped, documented and range-validated since M0, and is read by
nothing. A validated config key that does nothing is worse than a missing one: it tells the
operator a protection exists.

The gap survived because of a placement assumption, worth recording so it is not re-made:
`watchdog.rs` says the serial watchdog "arrives with the drivers in M1–M3", and M3-T02 does own
the *serial heartbeat* (three missed heartbeats → `HeartbeatLost` on the watchdog channel). But
the **consumer** of that signal was never assigned to any task — and it does not need a serial
port to exist. The mount facade polls position at 1 Hz; consecutive poll failures are observable
today, and the simulator's `Fault::DisconnectAfter` injects exactly the failure. Everything this
task needs has existed since M1-T03.

## Scope

- Watchdog arm in `astroctl-field` (the existing `watchdog.rs` pattern, or the facade's poll
  loop — argued, not assumed): `mount.serial.heartbeat_misses` consecutive position-poll
  failures while the mount is nominally connected → one `critical` alert (`MOUNT_LINK_LOST` —
  a new §4.2 code; the enum, its mirrors and the golden fixture move together), `mount.status`
  reflecting the loss, and the counter reset on the first successful poll.
- **Edge-triggered, once per transition** — the T-XFER/T14 alert discipline. Recovery emits one
  `info`. A poll failure during a deliberate disconnect is not link loss and must not alert.
- If a slew is in flight when the link is declared lost, say so in the alert message: "the mount
  was slewing when contact was lost" is the sentence that changes what the operator does next
  (go look at the rig) versus a loss at idle.
- PWA: the alert renders through the existing alert strip; the mount badge already goes hollow
  from `mount.status` — verify the pair reads coherently rather than adding a new surface.
- M3-T02 seam: when the real serial task lands, its `HeartbeatLost` feeds this same arm — the
  driver-level heartbeat is a *better* signal (it distinguishes "port gone" from "mount mute"),
  not a different consumer. Leave the seam named.

## Acceptance criteria

- [ ] **Flip the standing absence-assertion**: `tests/e2e/tests/faults.rs` currently asserts no
      alert fires on link loss, with a comment saying the assertion inverts when this task lands
      (its author: "the day the watchdog lands it fails and tells its reader to tighten it").
      That test must now assert exactly one `MOUNT_LINK_LOST` critical alert — and the suite's
      ×20 soak stays green.
- [ ] Link loss mid-slew (simulator `DisconnectAfter` under the e2e harness): alert within
      `heartbeat_misses` poll periods + one, naming the slew; recovery alerts `info` once.
- [ ] Deliberate `POST /api/mount/disconnect` produces **no** link-loss alert.
- [ ] `heartbeat_misses` is honoured from config — set it to 5 in a test and count the polls.


## Verified in the field, 2026-08-02 — and a correction to a doubt I raised

When the serial link dropped during a live session, I grepped the *tracing log* for
`MOUNT_LINK_LOST`, found nothing, and recorded a doubt that the watchdog had stayed silent in the
one scenario it was built for.

It had not. It fired correctly, six times across the session — `critical` on loss, `info` on
recovery:

```
{"topic":"alert","data":{"code":"MOUNT_LINK_LOST","severity":"critical",
 "message":"the mount has not answered 3 consecutive polls — check the cable at the mount
            and its power (REL-02)"}}
{"topic":"alert","data":{"code":"MOUNT_LINK_LOST","severity":"info",
 "message":"the mount is answering again"}}
```

Alerts are published on the event bus and land in `events.jsonl`; the tracing log carries
diagnostics. That separation is deliberate — an alert is operator-facing telemetry that must reach
a reconnecting client through the snapshot, not a line in a file nobody is reading at 3am. So the
evidence I checked could not have shown the alert either way, and the doubt was an artefact of the
test, not of the code.

Worth keeping as a note on method: **a null result from an instrument that cannot observe the thing
is not a null result.** The same shape has now cost this project twice — once concluding an SSH key
was not on the account, when the test had disabled the agent that held it.

The behaviour recorded here is therefore now verified on real hardware, in the unplanned way: an
actual link loss, mid-session, with the alert reaching the operator's phone.
