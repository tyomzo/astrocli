# M0-T07 — CI pipeline

**Milestone:** M0 · **Depends on:** M0-T01 (extend as tasks land) · **Crates:** —
**Size:** S · **Status:** done
**Spec:** IMP §2/M0, §5 (definition of done); ADD §5.6 (rules the lint enforces)

## Objective

Every push runs the full quality gate; the tree's health signal is one green check.

> **The remote now exists** — `github.com/tyomzo/astrocli`, default branch `main`, pushed
> 2026-07-29 — so GitHub Actions can genuinely run and this task is no longer working around a
> missing forge. The script-first structure still stands on its own merits: `scripts/check.sh` is
> the real deliverable and the workflow is a thin wrapper that invokes it, so the two cannot drift
> and the gate is runnable offline on any dev machine exactly as CI runs it.

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

## Result

`scripts/check.sh` is the gate; `.github/workflows/ci.yml` wraps it and lists no gates of its own.
Six gates in ascending cost order — fmt, clippy, deps, async, test, frontend — **10 s** on a warm
tree, against a 5-minute budget.

Each gate was verified to *fail* on a seeded violation and then reverted: malformed source (fmt,
and clippy caught it too), a forbidden `astroctl-hal → astroctl-drivers` edge (deps), a
`std::thread::sleep` in `vitals.rs` (async), a broken assertion (test).

**A limitation found by seeding, worth knowing.** `check-async.sh` treats everything after the
first `#[cfg(test)]` as test code, so the first seeded `thread::sleep` — appended to the end of a
file — was silently exempt. The script documents this trade, and it is the right one for a grep,
but it means **production code placed below a test module is unscanned**. Rust convention puts
`mod tests` last, so this is narrow; T-ISO-1 remains the behavioural backstop.

The two async clippy lints are in `[workspace.lints.clippy]`, not CI flags, so they fire on a
plain `cargo clippy`. `check-async.sh` supports a `check-async: allow <reason>` waiver — six are
in use, all in `telemetry.rs`, where `std::fs` is correct because `telemetry::init` is step 3 of
SDD §8.1's startup and the runtime is not built until step 4. Waivers are counted and printed so
they cannot accumulate unnoticed.

Not verified: the workflow has never executed on GitHub. It is committed but unrun until the next
push.
