# M0-T07 — CI pipeline

**Milestone:** M0 · **Depends on:** M0-T01 (extend as tasks land) · **Crates:** —
**Size:** S · **Status:** not started
**Spec:** IMP §2/M0, §5 (definition of done); ADD §5.6 (rules the lint enforces)

## Objective

Every push runs the full quality gate; the tree's health signal is one green check.

> **No git remote is configured yet, and that decision is deliberately deferred.** Until one
> exists the hosted workflow cannot run, so this task delivers the gates as a *local* script
> first and the workflow file second. The script is the real deliverable — the workflow should
> be a thin wrapper that calls it, so the two can never drift and so switching forge later is a
> one-file change. Do not block M0 on picking a forge.

## Scope

- `scripts/check.sh` — the quality gate, runnable offline on any dev machine: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, frontend `npm ci && npm run build && tsc --noEmit`, dep-lint script from M0-T01. Non-zero exit on any failure; runnable as a pre-push hook
- Workflow file (GitHub Actions unless another forge is chosen) that installs the toolchain, restores caches, and invokes `scripts/check.sh` — committed now, exercised whenever a remote appears
- **Async-safety lints** (SDD §2): deny `clippy::await_holding_lock` and `clippy::await_holding_refcell_ref` in the **`[workspace.lints.clippy]` table of the root manifest**, not as CI flags — M0-T01 established that pattern so the gate fires on a plain `cargo clippy` and does not depend on CI remembering to pass `-D warnings`. Plus plus a grep gate rejecting `std::thread::sleep`, `std::fs::` and `.blocking_` inside `astroctl-field`/`astroctl-stack` async paths. These catch the common ways a blocking call reaches the runtime; they do not catch all of them, which is why T-ISO-1 exists as the behavioural backstop
- Python workers: `ruff` + syntax check job (activates when `workers/` gains code)
- Cache: cargo + npm caches for reasonable CI times
- Branch discipline note in workflow README: CI must be green before merge; T-E2E-1 job placeholder (activated in M1-T16) marked `continue-on-error: false` from the moment it exists

## Acceptance criteria

- [ ] `scripts/check.sh` passes on the current tree, and each gate demonstrably fails on a seeded violation (fmt, clippy, test, dep-lint — verify once each, then revert)
- [ ] Script runs offline with warm caches in < 5 min; hosted CI < 10 min once a remote exists
- [ ] Workflow file is a wrapper around the script, not a second copy of the gate list
- [ ] Async-safety lints fire on a seeded violation (add a `std::thread::sleep` in an async fn, confirm the gate rejects it, revert)
