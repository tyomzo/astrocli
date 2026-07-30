# M1-T12 — Stack ingest + session mirror

**Milestone:** M1 · **Track:** B · **Depends on:** M0 · **Crates:** astroctl-stack
**Size:** M · **Status:** done
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

- [x] Corrupted upload (bit-flip test) → negative ack, nothing stored, tmp cleaned
- [x] Duplicate upload → positive ack, single file on disk, journal shows one logical frame
- [x] Frames arriving for a "finished" session (no activity 1 h) still stored correctly
- [x] Mirror layout byte-identical in structure to field session layout (shared fixture test with astroctl-session)

## Result note

Built as `crates/astroctl-stack/src/{ingest,mirror,journal}.rs`. SDD §5.11 amended in the same
change set (v1.14.0) — it specified one side of a two-sided contract, and M1-T11 could not have
been written against it. What changed is listed in the change note; the two that would have been
silent data-loss bugs are the dedup key (`frame_id` alone recurs in every session, §5.5) and the
temporary's name (`.tmp_<frame_id>` is shared by two overlapping retries, and the loser writes
through a rename into the archive — REL-11).

**Deferred, deliberately.** A duplicate is acked only after its body has been drained, so a
re-upload over a shaped link still costs its full transfer. Responding early cannot work — the
sender is mid-write and gets a transport error instead of the ack, so the frame would be retried
forever. Saving the retransmission needs a pre-flight the sender can make *before* it commits to a
body: either `HEAD /api/ingest/{session_id}/{frame_id}`, or the dedup key in request headers plus
`Expect: 100-continue`. Both are route-table changes and belong in a doc change, not here.

**Not implemented.** No idle-read timeout on the ingest body: a peer that opens a request and
stalls holds one task indefinitely. A total timeout is wrong (a legitimate 25 MB upload over
1 Mbit takes ~200 s), so this wants an idle-read timeout, which is a node-wide concern rather than
an ingest one.
