# M1-T07 — Frame store: sessions, durability, ID reservation

**Milestone:** M1 · **Track:** A · **Depends on:** M0 · **Crates:** astroctl-session
**Spec:** SDD §5.5, §6 (data design); PRD §5.9 layout, REL-04/05/11/12, IPP-09/10 subset
**Tests gated:** T-DUR-1

## Objective

The durability backbone: session directories, write-once frames, crash-safe metadata, and
frame-ID reservation. Everything downstream trusts this layer absolutely.

## Scope

- Session lifecycle: create (`YYYY-MM-DD_<slug>`), open existing, `CURRENT` symlink; `session.json` v1 (id, created, equipment snapshot from config, frame counter, reserved `sequence_state`)
- `reserve_frame_id()`: atomic, persisted before grant is returned — crash never reuses an ID
- Frame ingestion API used by capture flow: `begin_frame(id) → tmp path`, `commit_frame(tmp)` doing fsync-file → rename → fsync-dir; committed frames are immutable (no API to modify/delete — REL-11)
- Per-frame metadata write (`control/quality_<id>.json`: ts, exposure params, sha256, size) with same tmp-rename discipline
- Disk monitoring: free-space query for the watchdog; refuse `begin_frame` below critical threshold with distinct error (REL-12 pause semantics live in capture flow)
- Session frame listing for the API (`/api/session/current` backing)

## Acceptance criteria

- [ ] T-DUR-1: process killed (SIGKILL) between begin/commit and between commit/metadata — on restart no partial frame visible, IDs never reused, session.json parseable
- [ ] Committed frame file mtime/content untouched by any later operation (watch-based test)
- [ ] Concurrent reserve from two tasks yields unique IDs
