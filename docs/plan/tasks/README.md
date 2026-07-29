# AstroCtl — Implementation Task Breakdown

Task files for AI-agent (or human) implementation, derived from ASTROCTL-IMP-001 v1.1.2.
One folder per milestone, one file per task. Each milestone folder has a README with the
milestone goal, exit criteria, and dependency order.

## Governing documents (read before any task)

| Doc | Path | Role |
|-----|------|------|
| PRD v1.15.1 | `docs/intent/ASTROCTL-PRD-001.md` | requirements (`XXX-nn` IDs), config schema (§8.1/§8.2 are normative) |
| ADD v1.4.0 | `docs/design/ASTROCTL-ADD-001.md` | architecture, ADRs, crate boundaries |
| SDD v1.8.1 | `docs/design/ASTROCTL-SDD-001.md` | detailed design — **the spec for these tasks** |
| IMP v1.2.0 | `docs/plan/ASTROCTL-IMP-001.md` | milestone strategy, definition of done |

Every task's **Spec** line names the sections that govern it. If a task cites a section that
does not exist or does not actually cover the work, stop and fix the document first — building
against absent design is how a pack like this rots.

## Milestones

| Folder | Delivery | Status |
|--------|----------|--------|
| `M0-scaffolding/` | workspace, core crates, both binaries boot, PWA shell over VPN | in progress |
| `M1-walking-skeleton/` | full GUI + two-node orchestration, all devices simulated | not started |
| `M2-camera/` | real Canon R10 driver behind the unchanged `Camera` trait | not started |
| `M3-mount/` | real HEQ5 Skywatcher driver + HIL bring-up | not started |

## Rules for implementing agents

1. **Read the task's "Spec" references first** — the task file states *what*; the SDD section states *how*. On conflict, the SDD wins; if you must deviate, say so explicitly in your result and update the SDD (version bump + change note) in the same change set.
2. **Contracts are frozen**: HAL trait signatures, the API route table, event schema, error codes, and the worker IPC protocol are defined in the SDD. *Changing* one inside an implementation task is forbidden — propose it as a separate doc change. *Additively extending* one (a new event topic, a new route) is allowed only where the task file explicitly says so, and the SDD edit ships in the same change set with a version bump. Silent extension is the failure mode this rule exists to prevent: it makes the document describe a system that no longer exists.
3. **Respect crate dependency rules.** ADD §5.6 is authoritative for the layout and the allowed-dependency matrix; SDD §3 shows only the subset carrying code in M0–M3. The CI dep-lint enforces the matrix; do not weaken the lint to make a build pass.
4. **Definition of done** (IMP §5) applies to every task: tests named in the task green, no `unwrap`/`expect` on I/O paths, errors use the closed code enum, events emitted for every state change, tree stays demoable.
5. **One task = one reviewable change set**, unless the task carries a **PR split** section — a few tasks are deliberately broad because their parts are only demoable together, and those name their intended commits. Do not start a task whose `Depends on` list is unfinished. Tasks marked ∥ in the milestone README can run in parallel with their peers.
6. **Tests are deliverables**, not afterthoughts — a task without its named tests is incomplete.
7. Simulators and stubs are permanent products (HAL-11): production code quality, fault-injection hooks included.
8. **Keep the metadata honest.** Every task carries `Size` (S ≈ half a day, M ≈ 1–2 days, L ≈ 3+ days, at the IMP §4.1 "single developer + AI pair" calibration) and `Status` (`not started` / `in progress` / `done`). Update `Status` when you pick a task up and when you finish it — the milestone READMEs summarize, but these files are the record. If a task turns out to be a size larger than declared, change the field and say why in your result; a plan that silently absorbs overruns stops being a plan.
