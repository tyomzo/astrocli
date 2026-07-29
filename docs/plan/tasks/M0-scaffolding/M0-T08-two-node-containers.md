# M0-T08 — Two-node container harness

**Milestone:** M0 · **Depends on:** M0-T05, M0-T06 · **Crates:** deploy/, scripts/
**Size:** M · **Status:** not started
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
- Two independent config files and two independent auth tokens (SEC-02)
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

- [ ] `scripts/dev-up.sh` brings both containers up; `/api/system/health` returns ok on each
- [ ] The PWA loads from the field container in a browser, and shows **both** nodes' health —
      the stack's arriving through the field node's proxy, never a direct browser→stack call
      (assert in the browser network log)
- [ ] Containers resolve each other by service name; grepping the images for a hardcoded
      `127.0.0.1` or `localhost` in any config finds nothing
- [ ] `docker compose stop stack` → the field node stays up and reports the stack as
      disconnected; restarting it recovers with no field-node restart
- [ ] Wrong or absent token on either service → 401, proving the two tokens are independent
- [ ] `scripts/shape-link.sh 1mbit` measurably constrains throughput between the two containers
      (demonstrate with a timed transfer before and after)
- [ ] The stack image contains `workers/`; the field image does not

## Notes for whoever picks this up

Compose works with Docker or Podman; do not depend on features unique to either. Keep the images
buildable offline once the layer cache is warm — CI will build them on every run and a slow image
build will make the E2E job unpleasant enough that someone will disable it.
