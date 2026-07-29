# M0 — Scaffolding

**Goal:** repo + workspace + toolchain so every later PR is small. Both binaries start on two
machines, load validated config, refuse to run without an auth token, and serve health; the
PWA shell loads over the VPN from the field node and shows both nodes' health via the proxy.

**Exit criteria (IMP §2/M0):**
- `astroctl-field` and `astroctl-stack` run as **two containers on separate network namespaces**
  (M0-T08); the PWA loads from the field container and shows both nodes' health via the proxy
- Quality gate green: fmt, clippy, tests, frontend build, dependency-rule lint

**Why containers rather than two physical hosts.** Development happens on one workstation; the
field deployment is two real machines. Containers let a single host exercise the two-machine shape
honestly — real TCP across a network boundary, the proxy working host-to-host, two independent
tokens, and node-death as a testable event rather than a thought experiment. They also make
`tc`-shaped links available, which is what turns T-HOL-1 from a manual bench exercise into a CI
job.

This replaces an earlier ranked ladder of loopback compromises. Loopback would have proved nothing
about the deployment shape; containers prove everything about it except the tunnel.

**What still needs real hardware,** and is therefore *not* an M0 gate:
- The VPN itself — NetBird/Tailscale MTU, NAT traversal, tunnel reconnection
- The Raspberry Pi — ARM, USB under load, the PRF-05 512 MB budget in practice
- A phone on the actual tunnel, which is M1's demo and remains the human-facing proof

Those land with the field deployment. Recording them here so nobody mistakes a green M0 for a
proven tunnel.

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
