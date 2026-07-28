# M0 — Scaffolding

**Goal:** repo + workspace + toolchain so every later PR is small. Both binaries start on two
machines, load validated config, refuse to run without an auth token, and serve health; the
PWA shell loads over the VPN from the field node and shows both nodes' health via the proxy.

**Exit criteria (IMP §2/M0):**
- `astroctl-field` and `astroctl-stack` run on separate hosts
- PWA shell loads from the field node over VPN; both nodes' health visible
- Quality gate green: fmt, clippy, tests, frontend build, dependency-rule lint

**If the second host or the VPN is not ready yet.** The exit criterion exists to prove the
two-node VPN topology early, because it is the assumption most expensive to discover broken
later. It is not a code gate — all M0 *code* completes without it. If the stacking machine or
the tunnel is not up, take the partial exit in this order of preference:

1. **Two hosts, no VPN** — both binaries on separate machines over the LAN. Proves the proxy,
   the two-token auth story, and cross-host config. Only the tunnel is unproven.
2. **One host, VPN to the phone** — both binaries on the dev machine (loopback, the documented
   STK-20 degenerate case), but the PWA loaded on a real phone over the VPN. Proves the half
   that touches the UI, service worker, and remote auth.
3. **One host, localhost only** — everything works, nothing about the deployment shape is
   proven. Acceptable, but the debt is real.

Whichever you take, record it in the M1 README as explicit debt and clear it before M1's demo —
M1's exit criterion is a phone-over-VPN two-node demo, so an untested tunnel does not survive
past M1 regardless.

## Tasks and order

| Task | Title | Depends on | ∥ |
|------|-------|-----------|---|
| M0-T01 | Git + Cargo workspace + crate skeletons + dep-lint | — | |
| M0-T02 | Core domain types and error model | T01 | ∥ |
| M0-T03 | Event schema and event bus | T01 | ∥ |
| M0-T04 | Configuration loading and validation | T01 | ∥ |
| M0-T05 | Binary bootstrap: axum, auth, health, proxy stub | T02–T04 | |
| M0-T06 | Frontend pipeline and PWA shell | T01 | ∥ (integrates after T05) |
| M0-T07 | CI pipeline | T01 (extends as tasks land) | |

T02/T03/T04 are independent after T01 and can be assigned to parallel agents; T06 can start
against a mocked health endpoint and integrate once T05 lands.
