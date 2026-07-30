# M1-T07 — Frame store: sessions, durability, ID reservation

**Milestone:** M1 · **Track:** A · **Depends on:** M0 · **Crates:** astroctl-session
**Size:** M · **Status:** done
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

- [x] T-DUR-1: process killed (SIGKILL) between begin/commit and between commit/metadata — on restart no partial frame visible, IDs never reused, session.json parseable
- [x] Committed frame file mtime/content untouched by any later operation (watch-based test)
- [x] Concurrent reserve from two tasks yields unique IDs

## Result note

Built as `crates/astroctl-session/src/{store,manifest,frame,disk,durable}.rs`. SDD §5.5 amended in
the same change set (v1.16.0): §5.11.2 had argued every shared discipline for the *receiving* side
and §5.5 said only "the same tmp-fsync-rename discipline", which is not enough to build a store that
survives T-DUR-1. The finding worth the amendment on its own is that the frame sequence must be **per
session, not per kind** — the sidecar drops the kind prefix, so per-kind counters put `light_00042`
and `dark_00042` on one `quality_00042.json`.

T-DUR-1 is driven by a real child process (`testdata/crash_harness.rs`, built as
`frame-store-crash-harness`) which reaches a window, announces it on stdout, and parks; the test
reads that line, sends SIGKILL, and asserts the child died of signal 9 before reopening the store.
The pattern is `astroctl-ipc`'s, for the same reason: a durability guarantee tested by dropping a
struct is a guarantee about `Drop`. The suite was mutation-checked — reordering `reserve_frame_id`
to grant before persisting fails four of the five tests with the ID-reuse symptom.

The layout fixture `testdata/session-layout.txt` needed no change and is now asserted from both
sides: this crate produces the three paths after 42 real reservations, and `astroctl-stack`'s mirror
tests still pass unchanged (77 tests).

**Deferred, deliberately.** `Session::view()` reads the frames directory and every sidecar on each
call rather than keeping an index. A night is hundreds of frames of a few hundred bytes of metadata,
and an index is a second account of what is stored, free to disagree with the frames after a crash.
If `/api/session/current` is ever polled hard enough for this to matter, the fix is a cache with an
invalidation rule, not a second source of truth.
