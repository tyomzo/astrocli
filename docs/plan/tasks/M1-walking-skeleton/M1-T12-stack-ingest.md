# M1-T12 — Stack ingest + session mirror

**Milestone:** M1 · **Track:** B · **Depends on:** M0 · **Crates:** astroctl-stack
**Size:** M · **Status:** not started
**Spec:** SDD §5.11 (route table, ingest procedure, dedup and conflict semantics, session mirror, journal); ADD §5.2.2, ADR-05; PRD IPP-15, REL-11/12
**Tests gated:** T-ING-1

## Objective

The stacking server's receiving side: verified, deduplicated frame ingestion into a mirrored
session archive.

## Scope

- `POST /api/ingest`: multipart frame + metadata (session id, frame id, sha256, capture meta); stream to tmp, verify sha, fsync-rename into mirrored session layout (`sessions/<id>/frames/…` identical to field layout), respond ack `{sha256, stored: true}`
- Dedup: same sha + frame id already stored → immediate positive ack, no rewrite (safe re-upload)
- Session mirror: creates/extends `session.json` mirror from ingest metadata; late arrivals accepted any time (IPP-15)
- Ingest journal (SQLite): received frames, timestamps, source — the future authority for REL-13 reclaim decisions
- Disk monitoring on stack volume with warn/critical events (REL-12)
- Status API: `/api/stacking/stats` skeleton — session frame count, last ingest ts (real stats in Phase 2b)

## Acceptance criteria

- [ ] Corrupted upload (bit-flip test) → negative ack, nothing stored, tmp cleaned
- [ ] Duplicate upload → positive ack, single file on disk, journal shows one logical frame
- [ ] Frames arriving for a "finished" session (no activity 1 h) still stored correctly
- [ ] Mirror layout byte-identical in structure to field session layout (shared fixture test with astroctl-session)
