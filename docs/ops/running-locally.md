# Running the stack locally

Two nodes, two ways to run them. Pick by what you are trying to exercise.

| | `scripts/dev-up.sh` (containers) | bare processes |
|---|---|---|
| topology | two network namespaces, field proxies stack — the real shape | two processes on loopback |
| devices | simulator only | **real camera and mount** |
| TLS | no (plain HTTP) | yes, if configured |
| use it for | the proxy, link shaping, e2e, anything topology-shaped | hardware bring-up, the PWA on a phone |

If you are not sure, start with the container harness. It needs no staged libraries and cannot
silently build a binary with no drivers in it, which the bare path can and does.

---

## 1. Container harness — the two-node shape

```bash
scripts/dev-up.sh              # builds images, starts both, waits until both answer
scripts/dev-down.sh
```

It publishes **one** port, `18470`, mapped to the field node's 8470. That is deliberate: the
stacking server is never published, and the harness waits for it *through* the field node's
`/stack/*` proxy, so the topology of ADR-07 is exercised before the script finishes printing.

```bash
TOKEN=$(grep ASTROCTL_TOKEN deploy/.env | cut -d= -f2)
curl -fsS -H "Authorization: Bearer $TOKEN" http://localhost:18470/api/system/health
curl -fsS -H "Authorization: Bearer $TOKEN" http://localhost:18470/stack/api/system/health
```

**On the token.** Both nodes read one shared token from `ASTROCTL_TOKEN` — one, not two
(PRD §8.1/§8.2, SEC-02). If none is set the first run ends in the SEC-01 startup refusal, because a
node bound to a non-loopback address with no token refuses to start. That is correct behaviour and a
poor introduction, so `dev-up.sh` generates one into `deploy/.env` (mode 0600, git-ignored). A token
already in your environment always wins.

Other things that live here: `scripts/e2e.sh` runs the end-to-end suite against this harness, and
`scripts/shape-link.sh` puts `tc` constraints on the link between the namespaces, which is how the
latency and head-of-line-blocking requirements are tested honestly rather than asserted.

---

## 2. Bare processes — the only way to reach real hardware

### 2.1 Configuration

Copy the examples and edit:

```bash
cp config/field-node.example.yaml      /path/to/field-node.yaml
cp config/stacking-server.example.yaml /path/to/stacking-server.yaml
```

Then, at minimum:

- **`site`** — latitude, longitude, elevation, timezone. Nothing measures this and everything
  depends on it: the altitude limit that refuses a slew, the alt/az readout, the sidereal time
  behind every hour angle. A node carrying the example's site computes a horizon for somewhere else
  and stays perfectly self-consistent doing it. The **Site** card in the PWA's System screen will
  compare it against your phone's GPS in one tap.
- **`mount.driver`** / **`camera.driver`** — `simulator` unless you have the hardware attached. With
  `skywatcher` you also need `mount.port` (`auto` or e.g. `/dev/ttyUSB0`); see §2.2, because a
  default build cannot open it.
- **`storage.sessions_dir`**, **`server.log_dir`**, **`stacking_server.queue_dir`** — these are
  created if absent but not cleaned up.
- **`server.tls`** — see [tls-setup.md](tls-setup.md). Optional for loopback, **required** for a
  phone: wake lock, service workers and installability are all gated on a secure context, and
  without it the PWA degrades in three ways at once that look like unrelated bugs.

Configuration is loaded and validated **once at startup** and nothing re-reads the file (SDD §4.4).
Every change needs a restart. Unknown keys are rejected rather than ignored, so a config carrying a
key that has been removed — `mount.park_position`, deleted in M3-T07 — stops the node at startup
with a message naming the key.

### 2.2 Building

For the simulator, nothing special:

```bash
cargo build -p astroctl-field -p astroctl-stack
```

**For real hardware the feature flags are not optional, and leaving them off fails late.**

```bash
cargo build -p astroctl-field --bin astroctl-field --features libgphoto2,serialport
```

Without `libgphoto2` the node dies at startup naming the missing feature. Without `serialport` it is
worse: the mount driver constructs successfully and only fails on `connect`, so a config that looks
fine gets you a node that will not talk to the mount for a reason that is not visible until you try.

Both features need `-dev` packages. If they are not installed system-wide see
[`tools/devenv/README.md`](../../tools/devenv/README.md), which stages them without root. **Verify
the binary before you trust it** — both must print `0`:

```bash
strings target/debug/astroctl-field | grep -c "has no serial port implementation"
strings target/debug/astroctl-field | grep -c "has no libgphoto2 support"
```

The PWA is compiled *into* the binary with `include_dir!`, so a frontend change needs three steps,
not one:

```bash
(cd frontend && npm run build)
touch crates/astroctl-field/src/pwa.rs      # include_dir! does not see dist/ change on its own
cargo build -p astroctl-field --bin astroctl-field --features libgphoto2,serialport
```

Confirm the running node serves what you just built by comparing the asset hash — and confirm on the
phone with the build stamp at the bottom of the System screen, because a service worker will happily
keep serving yesterday's bundle:

```bash
curl -sSk https://localhost:8470/ | grep -o 'assets/index-[A-Za-z0-9_-]*\.js'
ls frontend/dist/assets/*.js
```

### 2.3 Running

The stacking server first — the field node proxies it, and a field node whose stack is absent
reports it rather than failing, but there is no reason to look at that:

```bash
export ASTROCTL_TOKEN='<a token you choose>'

./target/debug/astroctl-stack --config /path/to/stacking-server.yaml &
./target/debug/astroctl-field --config /path/to/field-node.yaml &
```

Both binaries take `-c`/`--config`, fall back to `$ASTROCTL_STACK_CONFIG` / `$ASTROCTL_FIELD_CONFIG`,
and then to `/etc/astroctl/*.yaml`. `--help` prints the rest.

Health, directly and through the proxy:

```bash
curl -sSk -H "Authorization: Bearer $ASTROCTL_TOKEN" https://localhost:8470/api/system/health
curl -sSk -H "Authorization: Bearer $ASTROCTL_TOKEN" https://localhost:8470/stack/api/system/health
```

`/api/system/info` reports the resolved config, the drivers that were actually built, the route
table and the runtime sizing. It is the first thing to read when behaviour does not match the
config you think you edited.

### 2.4 Stopping

Take the PID from the port's owner:

```bash
kill "$(ss -tlnp | grep ':8470' | grep -oP 'pid=\K[0-9]+')"
```

**Not `pkill -f astroctl`.** `-f` matches against the full command line, and the shell running your
`pkill` has the pattern in its own command line — so it matches itself and kills the caller. This
has happened four times in this project. Kill by PID, or by exact name with `pkill -x`.

Neither node stops the mount on shutdown, and that is deliberate (SDD §7): a service restart during a
session must leave a tracking mount tracking, because a mount is safe while tracking and a stopped
one has lost the target. Stop motion first if that is what you want.

---

## 3. Connecting a phone

The PWA is served by the field node at `/`, so the phone needs to reach the node's address over
**HTTPS** — see [tls-setup.md](tls-setup.md). Then:

1. Open the address; enter the token when prompted. If it says the token was *rejected* rather than
   *absent*, the token is wrong; the two cases are distinguished on purpose.
2. `System` → check the build stamp matches what you built, and use the **Site** card to compare the
   configured site against the phone's GPS.
3. Set `server.deployment_label` on a non-production node. It changes the installed app's name and
   icon so a development app driving a real mount cannot be mistaken for the production one. Origin
   already separates the installs; this is what keeps them distinguishable once installed.

---

## 4. When it does not come up

| symptom | cause |
|---|---|
| refuses to start, mentions the token | SEC-01: bound to a non-loopback address with no `ASTROCTL_TOKEN` |
| refuses to start, names a config key | unknown key — usually a setting removed in a later milestone |
| starts, camera driver fails immediately | built without `--features libgphoto2` |
| starts, mount fails only on connect | built without `--features serialport` |
| PWA loads but is an old version | the service worker; close the app fully and reopen, and check the build stamp |
| wake lock / install prompt missing | not a secure context — TLS is not configured or the certificate is not trusted |
| `/stack/*` returns 502 | the stacking server is not running or not reachable at `stacking_server.host`/`port` |

Two log destinations, and confusing them wastes an afternoon: **diagnostics** go to the tracing log
in `server.log_dir`, and **operator-facing alerts** go to `events.jsonl` in the same directory. An
alert that fired will not appear in the tracing log. Grepping the wrong one for `MOUNT_LINK_LOST`
once produced a false report that the watchdog had never fired, when it had fired six times.
