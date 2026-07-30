# M1-T11 — Transfer agent (field side)

**Milestone:** M1 · **Track:** A · **Depends on:** M1-T07 · **Crates:** astroctl-transfer, astroctl-field
**Size:** M · **Status:** done
**Spec:** SDD §5.10 (journal schema, state machine, upload loop, backoff, recovery, reclaim marking), §8.3(7) pacing rule; ADD ADR-05/06; PRD STK-17, ARC-11, REL-06/13, PRF-07
**Tests gated:** T-XFER-1

## Objective

Durable, resumable frame delivery to the stacking server: queue on disk, checksum ack,
retry forever, survive restarts. Minimal-but-real (IMP §2/M1).

## Scope

- On `frame.saved`: enqueue (SQLite journal per ADR-06: frame path, sha256, state machine `queued→uploading→acked`)
- Upload loop: HTTP multipart to stack `/api/ingest` (auth token), sha256 in the `meta` part; ack must echo matching sha → mark `acked` + emit `transfer.acked` (topic already declared in SDD §4.3 — no schema change needed)
- `transfer.status` events on state change and every 30 s (SDD §4.3), so the PWA never polls
- Retry with capped exponential backoff (config `retry_interval` base); stack unreachable is a *normal* state — queue grows, one warning event on state change, not per attempt
- Restart recovery: journal scan resumes `queued`/`uploading` (re-upload is safe — ingest dedups by sha)
- Reclaim marking only: `acked` frames flagged reclaim-eligible in journal; actual deletion is explicitly **not** implemented (REL-13 policy arrives Phase 2b)
- Queue status API for UI: depth, oldest age, last ack, state (`/api/transfer/status`)

## Acceptance criteria

- [x] Frame → acked flow against a running stack node (T12); sha mismatch → re-upload, alert after N failures
- [x] Kill stack node: captures continue, queue grows; restart stack: queue drains in order, every frame acked exactly once (dedup verified)
- [x] Kill *field* node mid-upload: after restart, frame re-uploaded, no journal corruption (SQLite WAL)

## Result note

`crates/astroctl-transfer/src/{journal,upload,meta,agent}.rs` is the agent; the `frame.saved`
subscription and `/api/transfer/status` are `crates/astroctl-field/src/transfer.rs`. SDD §5.10 is
amended in the same change set (v1.21.0) — its `CREATE TABLE` does not parse, and its terminal-state
rule had been overtaken by §5.11.2. Details in the change note.

**Verified live**, two real nodes, 48 MB simulated frames, five frames:

| Criterion | Evidence |
|---|---|
| frame → acked | ack in ~700 ms, echoed sha verified, `reclaimable=1` |
| stack killed | 3 captures completed while down, depth 1→2→3, `state: offline`, **one** `STACK_UNREACHABLE` alert across a 49 s outage and 5 attempts, one `STACK_ONLINE` on recovery |
| stack restarted | drained `light_00002,3,4` in order, 4 `transfer.acked` events, one per frame, all checksums equal end to end |
| field `kill -9` mid-upload | row was `uploading`, `PRAGMA integrity_check` = `ok` before and after, restart logged `resumed=1`, frame re-uploaded and acked |
| dedup | re-offered two already-stored frames → `duplicate: true` for both; the stack's ingest journal kept its original five rows and their original timestamps |

The `HEAD` pre-flight is implemented and asked before every upload. **The stack node answers `404`
today** — M1-T12 deferred the route — so every duplicate still costs its full body, which is the
cost the pre-flight exists to remove. The sender treats the `404` as "not stored, upload", so it is
correct now and becomes an optimisation the day T12 lands it; `tests/wire.rs` covers both branches.

**Not implemented.** §8.3(7) pacing, per §5.10.4 — the keys parse and validate, nothing enforces
them, and the agent now logs that at startup so the deviation is visible in the night's log. The
only thing bounding the transfer's share of the link is §5.10.2's one-upload-in-flight rule.
`transfer_method: rsync` is refused explicitly rather than silently served over HTTP.
