# M0-T08 — Two-node container harness

**Milestone:** M0 · **Depends on:** M0-T05, M0-T06 · **Crates:** deploy/, scripts/
**Size:** M · **Status:** done
**Spec:** ADD §5.5 (deployment view), ADR-07 (proxy); IMP §2/M0 exit

## Objective

Run the field node and the stacking server as **two containers on separate network namespaces**,
so the two-node topology is exercised on every developer machine and in CI — without a second
physical host and without a VPN.

Development happens on one workstation; the field deployment is two real machines. Containers are
what lets a single host test the two-machine shape honestly, and they replace what would otherwise
have been a loopback compromise that proves nothing about the deployment.

## What this proves, and what it does not

**Proves** — everything that is really about *two nodes talking*:
- Real TCP across a network boundary, not `127.0.0.1`
- The `/stack/*` reverse proxy working host-to-host, including WS upgrades (ADR-07)
- Two independent config files, sharing **one** token (SEC-02: both nodes name the same `auth_token_env`, and the field node presents it when proxying)
- Hostname resolution rather than hardcoded loopback addresses
- Node-down behaviour as a first-class test: stop a container and the field node must carry on
- **Traffic shaping.** `tc` on the veth pair gives a genuinely constrained link, which is what
  makes T-HOL-1's "shaped 1 Mbit" requirement runnable in CI instead of a manual bench exercise

**Does not prove** — and these stay real-hardware gates:
- The VPN itself: NetBird/Tailscale MTU, NAT traversal, tunnel reconnection
- Raspberry Pi reality: ARM, the USB stack under load, the PRF-05 512 MB budget on real hardware
- True cellular link behaviour (`tc` approximates loss and latency; it does not reproduce a
  handover)

Containers are strictly better than loopback for everything except the tunnel, and the tunnel is a
deployment concern that belongs with the field hardware.

## Scope

- `deploy/Dockerfile.field` and `deploy/Dockerfile.stack` — multi-stage: build with the pinned
  toolchain, run on a slim base. The field image carries the embedded PWA; **the stack image is
  the only one that gets `workers/`** (ADD §5.6 rule 6)
- `deploy/compose.yaml` — two services on a user-defined bridge network, addressed by service
  name (`field`, `stack`), each with its own mounted config and its own token from the
  environment. Named volumes for `/data/astro` on each so restart-recovery is testable
- `scripts/dev-up.sh` / `dev-down.sh` — bring the pair up, wait for both `/api/system/health`, and
  print the field node's URL
- `scripts/shape-link.sh` — apply and remove `tc netem`/`tbf` on the field↔stack path; parameters
  for bandwidth, latency and loss. This is the T-HOL-1 enabler
- Both images must build from a clean checkout with no host toolchain assumptions

Out of scope: Kubernetes, registries, image publishing, production hardening. This is a test and
development harness, not a deployment artefact.

## Acceptance criteria

- [x] `scripts/dev-up.sh` brings both containers up; `/api/system/health` returns ok on each
- [x] The PWA loads from the field container in a browser, and shows **both** nodes' health —
      the stack's arriving through the field node's proxy, never a direct browser→stack call
      (assert in the browser network log) — *substituted, see the result notes*
- [x] Containers resolve each other by service name; grepping the images for a hardcoded
      `127.0.0.1` or `localhost` in any config finds nothing
- [x] `docker compose stop stack` → the field node stays up and reports the stack as
      disconnected; restarting it recovers with no field-node restart
- [x] Wrong or absent token on either service → 401. Note this is **one shared token**, not two: PRD §8.1/§8.2 both name `ASTROCTL_TOKEN`, and the field node must present it to the stack when proxying. An earlier draft of this task said "two independent tokens", which was wrong and would have left the proxy and the M1 transfer agent with no credential to present
- [x] `scripts/shape-link.sh 1mbit` measurably constrains throughput between the two containers
      (demonstrate with a timed transfer before and after) — 10 605 Mbit/s → 0.96 Mbit/s
- [x] The stack image contains `workers/`; the field image does not

## Notes for whoever picks this up

Compose works with Docker or Podman; do not depend on features unique to either. Keep the images
buildable offline once the layer cache is warm — CI will build them on every run and a slow image
build will make the E2E job unpleasant enough that someone will disable it.

## Result notes

**ADD amended in this change set** (rule 2): §5.5 claimed the container topology exercises "two
independent tokens". No deployment has two — see the criterion above. ADD v1.5.1.

Measured, on the workstation this landed on, 1 MiB field→stack in both states:

| link | throughput | note |
|------|-----------|------|
| unshaped | 10 605 Mbit/s | veth pair; the number is "as fast as memcpy", not a network |
| `shape-link.sh 1mbit` | **0.96 Mbit/s** | 8.74 s for the same MiB; tbf overhead is the 4 % |
| `--latency 80ms --loss 1% 2mbit` | 1.74 Mbit/s | proxied health round trip 160.8 ms, as configured |

Deliberate deviations and interpretations:

- **NET_ADMIN is on a throwaway sidecar, never on a service.** The task asks to prefer shaping the
  veth from the host; that needs root on the workstation, and a harness every developer runs must
  not need sudo, so it was rejected rather than overlooked. `shape-link.sh` starts a container that
  joins the target's network namespace, holds the capability for about a second, and exits. The
  field and stack containers run unprivileged as uid 10001, which is what their systemd units will
  do — a service that can rewrite its own routing table is a service whose failures do not mean
  what they mean in production.
- **The qdisc is filtered on the peer's address, not attached to the root.** Both containers reach
  the workstation through the same `eth0` the other node is on, so an unfiltered root qdisc would
  throttle the operator's own path to the PWA — a "1 Mbit link" that also makes the UI crawl proves
  the opposite of what T-HOL-1 asks. Verified: with the link at 1 Mbit, a 206 kB asset still loads
  from the field node in 0.9 ms.
- **Configs are baked into the images, not bind-mounted.** The task says "mounted"; the images are
  self-contained instead, so `docker run astroctl-field:dev` works with nothing else present and
  the "no loopback literal in any config in the image" check has something to grep. compose.yaml
  documents the bind mount for iterating on config. The harness config also drops the example's
  commented-out `tls:` and `ollama_host:` blocks: a config that has decided against a thing should
  not ship the instructions for it, and those comments were the only loopback literals left.
- **`scripts/lib/harness.sh` is a fourth file**, holding what all three scripts must agree about
  (which compose, which engine, which container, which address). Three copies of that would not
  fail when they diverged — they would report that the containers are not running.
- **python3 is installed in the stack image only.** `workers.python_interpreter` has to name a real
  interpreter or M1-T13's first spawn is a puzzle rather than a step. The CUDA stack in
  `workers/requirements.txt` is deliberately not installed.
- **The browser criterion was substituted** — no browser is attached to this environment. What was
  run instead is stronger in one respect and weaker in another. Stronger: the shipped bundle was
  grepped for every URL it contains, and it holds exactly two health paths
  (`/api/system/health`, `/stack/api/system/health`), no absolute URL but XML namespaces, and no
  mention of port 8471 — so a direct browser→stack call is not merely unused, it is not in the
  code; and the stack service publishes no port, so it is not reachable from the workstation at
  all. Weaker: nobody has watched React render two node cards. That wants a browser, and it also
  wants M1-T04's real panels.
- **`/api/system/health` requires the token**, so the compose healthcheck carries it. There is no
  unauthenticated liveness endpoint. That follows from SEC-02 and is not obviously wrong, but any
  future orchestrator probe inherits it.
- **`tbf burst` is fixed at 32 kbit** (`--burst` to change it). It must be at least rate/HZ, so
  above roughly 30 Mbit the configured rate is silently not reached. The named case is 1 Mbit.

**"What this proves" overstates one line.** It claims the proxy working host-to-host "including WS
upgrades (ADR-07)". It does not, and cannot yet: `proxy.rs` refuses an upgrade explicitly until
M1-T14 rather than silently downgrading it, and on the live harness `/stack/ws/preview` answers
`409 UNSUPPORTED`. The harness proves the refusal, which is the correct behaviour for today. Worth
noting for whoever picks up M1-T14: the refusal's advice — "connect to the stacking server directly
until then" — has no meaning in this topology, because the stack service publishes no port. There
is no direct route to fall back to here, so M1-T14 is what makes stack WebSockets reachable at all
from a browser on the workstation.

Unverified, and honestly so:

- **Podman.** It is not installed here. The compose file uses nothing outside the Compose spec and
  the scripts resolve `podman compose`/`podman-compose`, but no line of that path has been run.
  The one part most likely to need work is `run --network container:` under rootless podman, where
  the sidecar and the target may not share a user namespace and CAP_NET_ADMIN would then not apply.
- **A literal offline build.** `docker build --network none` is not evidence: BuildKit includes the
  network mode in the RUN cache key, so it invalidates the cache it is meant to test. What was
  measured instead is a source-change rebuild against a warm cache — 6 s, with cargo performing no
  registry update and no download, and the npm stage untouched. The one step that still reaches the
  network is `FROM` tag resolution; pinning digests would close it, at the price of a number nobody
  will ever update.
