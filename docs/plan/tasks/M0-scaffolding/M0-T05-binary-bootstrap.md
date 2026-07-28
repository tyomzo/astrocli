# M0-T05 — Binary bootstrap: axum, auth, health, proxy stub

**Milestone:** M0 · **Depends on:** M0-T02, T03, T04 · **Crates:** astroctl-field, astroctl-stack
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
- Structured `tracing` setup: console + file per config `log_dir`

Out of scope: WS hub (M1), confirmation tiers enforcement (Phase 2c).

## Acceptance criteria

- [ ] Missing token env + non-loopback bind → startup refusal with explanatory error
- [ ] Wrong/absent bearer → 401 envelope with `code: "AUTH"` on every route incl. proxy
- [ ] `curl` health on field, on stack directly, and on field's `/stack/api/system/health` proxy all succeed with token
- [ ] SIGTERM exits cleanly within 2 s at idle
