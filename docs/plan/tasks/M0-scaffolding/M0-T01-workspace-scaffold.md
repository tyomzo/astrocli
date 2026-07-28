# M0-T01 — Git + Cargo workspace + crate skeletons + dependency lint

**Milestone:** M0 · **Depends on:** — · **Crates:** all
**Spec:** SDD §3 (crate graph), ADD §5.6 (layout + dependency rules), IMP §2/M0

## Objective

Initialize version control and the full workspace skeleton so all subsequent tasks add code
to compiling, lint-clean crates whose dependency boundaries are machine-enforced.

## Scope

- `git init`; commit the four governing documents and this task tree as the first commit
- Cargo workspace: `crates/astroctl-{core,hal,drivers,safety,session,pipeline,solver,planning,transfer,llm,ipc,field,stack}` — each with a minimal `lib.rs`/`main.rs` that compiles; `workers/` dir with `requirements.txt` placeholder; `frontend/` placeholder
- Workspace-level: rust-toolchain.toml (stable, MSRV pinned), rustfmt.toml, clippy config (deny warnings in CI), shared `[workspace.dependencies]` (tokio, axum, serde, thiserror, tracing)
- Dependency lint: a CI-runnable script (`scripts/check-deps.sh` or cargo-deny bans) asserting the allowed-dependency matrix of ADD §5.6 (e.g. `astroctl-drivers` may depend only on `hal` + `core`; `field`/`stack` never on each other; `llm` never on `session`/`hal`)
- `.gitignore` (target/, node_modules/, dist/, __pycache__/)

Out of scope: any functional code, CI workflow files (M0-T07).

## Acceptance criteria

- [ ] `cargo build --workspace` and `cargo clippy --workspace -- -D warnings` succeed
- [ ] Dep-lint script passes, and **fails** when a forbidden edge is added (prove with a temporary test edge, then remove)
- [ ] Fresh clone builds with only rustup-provided toolchain
- [ ] Git history starts with docs commit, then scaffold commit
