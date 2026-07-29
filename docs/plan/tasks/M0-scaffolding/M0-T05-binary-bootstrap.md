# M0-T05 — Binary bootstrap: axum, auth, health, proxy stub

**Milestone:** M0 · **Depends on:** M0-T02, T03, T04 · **Crates:** astroctl-field, astroctl-stack
**Size:** M · **Status:** done
**Spec:** SDD §4.5 (auth), §8.1 (startup sequence), §8.2 (route metadata), §5.8.2 pattern; ADD ADR-07

## Objective

Both binaries boot per the SDD startup sequence, enforce auth from the first route, and the
field node proxies the stack — the deployment shape is real from M0.

## Scope

- `astroctl-field` and `astroctl-stack` mains: config load → auth startup check (token env present, else refuse unless bind addr is loopback — SEC-01/02) → tracing init → axum serve → graceful shutdown (SIGTERM) skeleton per SDD §7 ordering
- Bearer-token middleware, constant-time compare, applied to all routes incl. WS upgrades
- `RouteMeta { tier, audit }` typed layer (SDD §8.2) — wired, audit-logs only for now
- `/api/system/health` on both (status, disk_free_gb, clock_synced stub, versions) and `/api/system/info` skeleton
- Field node: reverse proxy `/stack/*` → stack base URL from config, forwarding auth
- **Explicitly sized tokio runtime** per SDD §7: build with `Builder::new_multi_thread().worker_threads(n)` from `server.runtime_worker_threads`, defaulting to `min(2, cores-2)` floor 1 on the field node and one-per-core on the stack. Never take tokio's default on the field node — reserving cores for the camera thread and decode pool is the whole point. Report the resolved value in `/api/system/info`
- Structured `tracing` setup: console + file per config `log_dir`

Out of scope: WS hub (M1), confirmation tiers enforcement (Phase 2c).

## Acceptance criteria

- [x] Missing token env + non-loopback bind → startup refusal with explanatory error
- [x] Wrong/absent bearer → 401 envelope with `code: "AUTH"` on every route incl. proxy
- [x] `curl` health on field, on stack directly, and on field's `/stack/api/system/health` proxy all succeed with token
- [x] SIGTERM exits cleanly within 2 s at idle
- [x] `/api/system/info` reports the resolved worker-thread count, and setting `runtime_worker_threads: 1` demonstrably changes it

## Result notes

**SDD amended in this change set** (rule 2): §5.11.1 was missing `/api/system/info`, which §7
requires on both binaries. SDD v1.7.1.

Deliberate deviations and interpretations, all argued in the code they affect:

- **SEC-01 refusal keys on loopback only.** §4.5 says "not a loopback/VPN address", but a bind
  address cannot be classified as "VPN" — an overlay interface's address is an ordinary private
  address. The task file's own wording ("else refuse unless bind addr is loopback") is what is
  implemented. An empty token counts as absent, or `export ASTROCTL_TOKEN=` would satisfy the check.
- **`/api/system/info` is on both nodes**, and both report the resolved worker-thread count.
- **`/api/mount/estop` is not registered.** §5.8.2 is a middleware-shape requirement, and the shape
  is satisfied (auth + route metadata, no body parsing on any path); a route that answers 200
  without stopping a motor would be worse than its absence. It lands with M1-T03.
- **`worker: null` on stack health** until M1-T13 supplies a supervisor — not a fabricated
  `{state: "stopped", restarts: 0}`.
- **HTTP-layer code is duplicated between the two binaries** (auth, route metadata, telemetry,
  vitals, watchdog, CLI). ADD §5.6 gives them no shared home: rule 5 forbids them depending on
  each other, and SDD §4.2 forbids axum in `astroctl-core`. If this grows past M1, §5.6 needs an
  `astroctl-api` crate.
