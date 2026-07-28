# M1-T11 — Transfer agent (field side)

**Milestone:** M1 · **Track:** A · **Depends on:** M1-T07 · **Crates:** astroctl-transfer
**Spec:** SDD §5.5 adjacency + ADD ADR-05/06, ARC-11; PRD STK-17, REL-06/13, PRF-07

## Objective

Durable, resumable frame delivery to the stacking server: queue on disk, checksum ack,
retry forever, survive restarts. Minimal-but-real (IMP §2/M1).

## Scope

- On `frame.saved`: enqueue (SQLite journal per ADR-06: frame path, sha256, state machine `queued→uploading→acked`)
- Upload loop: HTTP multipart to stack `/api/ingest` (auth token), sha256 in headers; ack must echo matching sha → mark `acked` + emit `transfer.acked` event (add topic to §4.3 enum — this is a designed extension point, note in change set)
- Retry with capped exponential backoff (config `retry_interval` base); stack unreachable is a *normal* state — queue grows, one warning event on state change, not per attempt
- Restart recovery: journal scan resumes `queued`/`uploading` (re-upload is safe — ingest dedups by sha)
- Reclaim marking only: `acked` frames flagged reclaim-eligible in journal; actual deletion is explicitly **not** implemented (REL-13 policy arrives Phase 2b)
- Queue status API for UI: depth, oldest age, last ack, state (`/api/transfer/status`)

## Acceptance criteria

- [ ] Frame → acked flow against a running stack node (T12); sha mismatch → re-upload, alert after N failures
- [ ] Kill stack node: captures continue, queue grows; restart stack: queue drains in order, every frame acked exactly once (dedup verified)
- [ ] Kill *field* node mid-upload: after restart, frame re-uploaded, no journal corruption (SQLite WAL)
