# M0 — Scaffolding

**Goal:** repo + workspace + toolchain so every later PR is small. Both binaries start on two
machines, load validated config, refuse to run without an auth token, and serve health; the
PWA shell loads over the VPN from the field node and shows both nodes' health via the proxy.

**Exit criteria (IMP §2/M0):**
- `astroctl-field` and `astroctl-stack` run on separate hosts
- PWA shell loads from the field node over VPN; both nodes' health visible
- CI green: fmt, clippy, tests, frontend build, dependency-rule lint

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
