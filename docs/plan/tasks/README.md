# AstroCtl — Implementation Task Breakdown

Task files for AI-agent (or human) implementation, derived from ASTROCTL-IMP-001 v1.0.0.
One folder per milestone, one file per task. Each milestone folder has a README with the
milestone goal, exit criteria, and dependency order.

## Governing documents (read before any task)

| Doc | Path | Role |
|-----|------|------|
| PRD v1.5.0 | `docs/intent/ASTROCTL-PRD-001.md` | requirements (`XXX-nn` IDs) |
| ADD v1.1.0 | `docs/design/ASTROCTL-ADD-001.md` | architecture, ADRs, crate boundaries |
| SDD v1.0.2 | `docs/design/ASTROCTL-SDD-001.md` | detailed design — **the spec for these tasks** |
| IMP v1.0.0 | `docs/plan/ASTROCTL-IMP-001.md` | milestone strategy, definition of done |

## Milestones

| Folder | Delivery | Status |
|--------|----------|--------|
| `M0-scaffolding/` | workspace, core crates, both binaries boot, PWA shell over VPN | not started |
| `M1-walking-skeleton/` | full GUI + two-node orchestration, all devices simulated | not started |
| `M2-camera/` | real Canon R10 driver behind the unchanged `Camera` trait | not started |
| `M3-mount/` | real HEQ5 Skywatcher driver + HIL bring-up | not started |

## Rules for implementing agents

1. **Read the task's "Spec" references first** — the task file states *what*; the SDD section states *how*. On conflict, the SDD wins; if you must deviate, say so explicitly in your result and update the SDD (version bump + change note) in the same change set.
2. **Contracts are frozen**: HAL trait signatures, the API route table, event schema, error codes, and the worker IPC protocol are defined in the SDD. Do not "improve" them inside an implementation task; propose changes as a separate doc change.
3. **Respect crate dependency rules** (ADD §5.6 / SDD §3). The CI dep-lint enforces them; do not weaken the lint to make a build pass.
4. **Definition of done** (IMP §5) applies to every task: tests named in the task green, no `unwrap`/`expect` on I/O paths, errors use the closed code enum, events emitted for every state change, tree stays demoable.
5. **One task = one reviewable change set.** Do not start a task whose `Depends on` list is unfinished. Tasks marked ∥ in the milestone README can run in parallel with their peers.
6. **Tests are deliverables**, not afterthoughts — a task without its named tests is incomplete.
7. Simulators and stubs are permanent products (HAL-11): production code quality, fault-injection hooks included.
