# M0-T07 — CI pipeline

**Milestone:** M0 · **Depends on:** M0-T01 (extend as tasks land) · **Crates:** —
**Spec:** IMP §2/M0, §5 (definition of done); ADD §5.6 (rules the lint enforces)

## Objective

Every push runs the full quality gate; the tree's health signal is one green check.

## Scope

- GitHub Actions (or forge-appropriate) workflow: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, frontend `npm ci && npm run build && tsc --noEmit`, dep-lint script from M0-T01
- Python workers: `ruff` + syntax check job (activates when `workers/` gains code)
- Cache: cargo + npm caches for reasonable CI times
- Branch discipline note in workflow README: CI must be green before merge; T-E2E-1 job placeholder (activated in M1-T16) marked `continue-on-error: false` from the moment it exists

## Acceptance criteria

- [ ] CI green on current tree; each gate demonstrably fails on a seeded violation (fmt, clippy, test, dep-lint — verify once each, then revert)
- [ ] Total CI time < 10 min with warm caches
