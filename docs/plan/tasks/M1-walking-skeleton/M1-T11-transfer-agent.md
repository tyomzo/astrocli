# M1-T11 — Transfer agent (field side)

**Milestone:** M1 · **Track:** A · **Depends on:** M1-T07 · **Crates:** astroctl-transfer
**Size:** M · **Status:** not started
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

- [ ] Frame → acked flow against a running stack node (T12); sha mismatch → re-upload, alert after N failures
- [ ] Kill stack node: captures continue, queue grows; restart stack: queue drains in order, every frame acked exactly once (dedup verified)
- [ ] Kill *field* node mid-upload: after restart, frame re-uploaded, no journal corruption (SQLite WAL)
