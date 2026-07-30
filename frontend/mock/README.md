# `frontend/mock/` — the dev-only field node that M1-T03 retires

**Delete this directory when M1-T03 lands.** It exists because M1-T04 (the WS store and the mount
panel) was built before the routes it consumes, which its task header explicitly authorises. It is
a Node process that impersonates `astroctl-field` closely enough to drive every state the PWA can
be in, and nothing more: no camera, no capture, no transfers, no live view, no command envelope
(M1-T10), and deliberately **no `/api/mount/estop`** — that route is M1-T05's, and a mock that
answered it would have made the header's unarmed e-stop button a lie.

## Running it

```sh
npm run mock     # 127.0.0.1:8470 — the port vite.config.ts already proxies to
npm run dev      # second terminal; open http://localhost:5173/
```

The mock listens on the field node's port on purpose, so switching between it and a real node is
starting or stopping a process rather than editing configuration. They cannot both run: if a real
`astroctl-field` is already there, you do not need the mock.

| Env | Effect |
|-----|--------|
| `ASTROCTL_MOCK_TOKEN=…` | require exactly this bearer token. Unset means any token (or none) is accepted, matching SDD §4.5's loopback exception. Set it to exercise the 401 path |
| `ASTROCTL_MOCK_PORT=…` | default 8470. Changing it means changing `vite.config.ts` too |
| `ASTROCTL_MOCK_NULL_ALTAZ=1` | report `alt`/`az` as `null` — the shape M1-T03 emits until M1-T05's topocentric transform lands |
| `ASTROCTL_MOCK_STACK_OFFLINE=1` | report the stack node unreachable |

Two things worth doing by hand once:

```sh
curl -XPOST localhost:8470/api/mock/drop-clients   # closes every socket: reconnect + resnapshot
curl -XPOST localhost:8470/api/mount/disconnect    # nudge badge goes hollow, position leaves the snapshot
```

`drop-clients` is not a field-node route. It exists so REL-10's reconnect path can be exercised
without killing the process; killing and restarting the process works too and is closer to the
acceptance criterion.

## The contract

Everything below is what the mock implements and what the PWA is written against. Where the SDD
already pins something down, the section is cited and the mock follows it. **Two frame shapes are
not in the SDD** — the snapshot and the ping answer — and are marked so; those are the parts
M1-T03 should either adopt or change deliberately, rather than arrive at independently.

### Auth (SDD §4.5)

```
POST /api/auth/ws-ticket        Authorization: Bearer <token>
  → 200 {"ticket":"<32 hex chars>","expires_in":30}
  → 401 {"v":1,"code":"AUTH","message":"…","retryable":false}

GET  /ws?ticket=<opaque>        upgrade; the ticket is validated and CONSUMED
  → 101, or 401 before the upgrade if missing, expired or already spent
```

The mock enforces all four §4.5 rules: single use, 30 s TTL, 128 bits of `randomBytes`, and a
bounded store (cap 64, swept on issue). It **refuses a bearer header on the upgrade** even when one
is present — §4.5 says the ticket is the only way a browser authenticates `/ws`, and accepting the
header would have hidden a PWA bug.

The PWA fetches a fresh ticket immediately before *every* `WebSocket` construction, including every
reconnect. `connection.test.ts` asserts it.

### Mount routes (SDD §5.8.1)

| Route | Body | Response |
|---|---|---|
| `GET /api/mount/position` | | `{"ra":h,"dec":deg,"alt":deg\|null,"az":deg\|null,"pier_side":"east"\|"west"\|"unknown"}` |
| `GET /api/mount/status` | | `{"state":"disconnected"\|"idle"\|"slewing"\|"parked"\|"fault","tracking":bool,"slewing":bool,"parked":bool}` |
| `POST /api/mount/connect` | `{}` | 200 MountStatus |
| `POST /api/mount/disconnect` | `{}` | 200 MountStatus |
| `POST /api/mount/goto` | `{"ra_hours":n,"dec_degrees":n}` | 202 `{"correlation_id":"<hex>","watch_topic":"mount.position"}` |
| `POST /api/mount/tracking` | `{"mode":"sidereal"\|"lunar"\|"solar"\|"off"}` | 200 MountStatus |
| `POST /api/mount/slew` | `{"axis":"ra"\|"dec","direction":"positive"\|"negative","speed":1..5,"ttl_ms":500}` | 200 `{"axis":…,"expires_in_ms":n}` |
| `POST /api/mount/slew/stop` | `{"axis"?:"ra"\|"dec"}` | 200 MountStatus |

Three parameter shapes SDD §5.8.1 names but does not define, decided here because the D-pad needed
them:

- **`axis` is `"ra"` \| `"dec"`** and **`direction` is `"positive"` \| `"negative"`.** Compass words
  were rejected: on the RA axis, "east" means "the direction in which right ascension increases",
  which is one indirection nobody reading a bug report wants to perform. The UI maps N/S to
  dec±, E/W to ra±.
- **`speed` is an ordinal 1–5**, matching the five dots of §5.9's sketch. Not a rate: what a step
  means in degrees per second belongs to the driver, and a client sending °/s would be asserting a
  capability it cannot check.
- **`ttl_ms` defaults to 500 and is clamped to 2000 server-side** (§5.8.1). The PWA renews at
  `ttl/2` while a direction is held and sends `slew/stop` on release.

Error envelopes are SDD §4.2 verbatim (`{v, code, message, retryable}`) with the §4.2 status
mapping. The mock produces `LIMIT_ALTITUDE` (403) below 15°, `BUSY` (409) for a second goto,
`NOT_CONNECTED` (409), `VALIDATION` (422) and `NOT_FOUND` (404).

### `/ws` frames

Text frames only. Binary belongs on `/ws/liveview` (§8.3(5)) and the PWA ignores anything binary
that arrives here.

**Server → client, kind 1: an event.** Exactly `astroctl_core::event::Event`, because SDD §4.3
requires the WS frame and the session-log line to be the identical serialization:

```json
{"v":1,"ts":"2026-07-30T21:04:05.123Z","topic":"mount.position","data":{…}}
```

**Server → client, kind 2: a control frame.** *(not in the SDD — proposed here)* An `Event` has no
room for an envelope, so control frames are told apart by carrying `type` where an event carries
`topic`. Nothing carries both.

```json
{"v":1,"type":"snapshot","ts":"…","events":[ <Event>, … ]}
{"v":1,"type":"pong","ts":"…","id":<echoed>,"server_time":"…"}
```

The **snapshot is the first frame after the upgrade**, always (§5.8.3). It carries the latest event
for every *stateful* topic:

```
mount.status  mount.position  camera.status  capture.progress
transfer.status  stack.status  system.health
```

`alert`, `frame.saved` and `transfer.acked` are never in it — they are occurrences, not values that
are true. **A topic absent from the snapshot means the node has no value for it**, and the PWA
reduces it back to unknown: disconnect the mount and `mount.position` leaves the snapshot, so a
reconnecting client shows no coordinates rather than the ones from before the drop. This is §5.9's
"resnapshot rather than resume from a hole" made concrete, and `telemetry.test.ts` asserts it.

**Client → server.** *(not in the SDD — proposed here)*

```json
{"type":"ping","id":<int>}
{"type":"subscribe","topics":[…]}
{"type":"unsubscribe","topics":[…]}
```

§5.8.3 says "the hub answers `ping` frames immediately", and this **cannot mean RFC 6455 control
frames**: the browser `WebSocket` API has no way to send a ping or observe a pong. So ping is an
application-level message and `pong` echoes its `id`. The PWA sends one every 5 s and derives RTT
from the round trip (M1-T15 renders it); more importantly it treats 12 s with *no frame of any
kind* as a dead link and reconnects, because a dropped VPN does not close TCP connections — it
stops carrying them, and `onclose` never fires.

The default subscription is **every topic**; `subscribe` narrows it. The PWA sends neither message
in M1, so a hub that required an explicit subscribe would deliver nothing.

## What the simulated mount actually does

Enough to make the UI's in-motion states real rather than theoretical:

- **Goto is a ramp**, ~6°/s with a smoothstep ease and a 2 s floor, so `mount.status` goes
  `idle → slewing → idle` and positions stream through the motion at 1 Hz. Tracking is suspended
  for the duration and resumes on settle.
- **Tracking off means the sky moves.** A stationary mount holds alt/az, so the RA it points at
  climbs at the sidereal rate. Coordinates that froze when tracking stopped would have made the
  tracking control look inert.
- **The slew TTL expires.** An unrenewed lease stops the axis and emits
  `alert{severity:"warning", code:"SLEW_TTL_EXPIRED"}` — checked at 100 Hz, because a 1 Hz loop
  would miss a 500 ms deadline by half a second.
- **alt/az are computed** from a mean-sidereal transform at 50°N 8°E. M1-T05 owns the real one,
  shared with the altitude limit so a display bug and a limit bug cannot disagree.
- The camera reports `connected: false`, which is true of this build.
