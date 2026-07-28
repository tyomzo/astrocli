# M1-T10 — Command envelope: staleness + idempotency

**Milestone:** M1 · **Track:** A+C · **Depends on:** M1-T05, M1-T08 · **Crates:** astroctl-field, astroctl-core, frontend/

> Depends on T05 and T08 deliberately: the envelope must be applied to the *complete* mutation
> surface in one pass. Landing it while camera or slew routes are still arriving guarantees a
> partially-covered API, which is worse than no envelope — it looks enforced and isn't.
**Size:** M · **Status:** not started
**Spec:** SDD §5.8.1 (staleness paragraph), §8.3(4)
**Tests gated:** T-STALE-1

## Objective

Late starts refused, late stops always honored, retries idempotent — the command envelope
from SDD §5.8.1 applied uniformly.

## Scope

- Envelope extraction middleware: `issued_at` + `command_id` on every state-changing request; classification per route via `RouteMeta` extension: `MotionInitiating | Stopping | Neutral`
- Staleness: `MotionInitiating` older than `max_command_age_ms` (config, default 2000) → 422 `COMMAND_STALE`; `Stopping` never rejected; missing envelope on mutation routes → 422 (closed rollout: PWA updated in this task)
- Idempotency: bounded LRU of `command_id → outcome` (per-process, capacity ~1024, TTL 5 min); replay returns original outcome with `replayed: true` marker
- Clock-skew handling: every response carries `server_time`; PWA measures skew, offsets `issued_at`, warns in UI beyond 30 s skew
- PWA command layer updated to attach envelope on all mutations

## Acceptance criteria

- [ ] T-STALE-1: 5 s-old goto → `COMMAND_STALE`, simulator got no command; equally old slew/stop → executed; duplicate `command_id` → original outcome, single execution (assert via simulator command log)
- [ ] E-stop route exempt from *all* envelope requirements (empty body still valid)
- [ ] Skew injection test: client clock +60 s → UI warning, commands still work via offset
