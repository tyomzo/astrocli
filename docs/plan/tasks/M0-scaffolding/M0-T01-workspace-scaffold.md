# M0-T01 — Git + Cargo workspace + crate skeletons + dependency lint

**Milestone:** M0 · **Depends on:** — · **Crates:** all
**Size:** M · **Status:** not started
**Spec:** SDD §3 (crate graph), ADD §5.6 (layout + dependency rules), IMP §2/M0

## Objective

Initialize version control and the full workspace skeleton so all subsequent tasks add code
to compiling, lint-clean crates whose dependency boundaries are machine-enforced.

## Scope

- Version control and `LICENSE` (MIT, © 2026 Artiom Klimov) already exist at the repo root, as do the governing documents — start from the existing history, do not re-initialize
- Cargo workspace, all 14 crates of ADD §5.6: `crates/astroctl-{core,hal,drivers,safety,session,pipeline,solver,planning,guiding,transfer,llm,ipc,field,stack}` — each with a minimal `lib.rs`/`main.rs` that compiles; `workers/` dir with `requirements.txt` placeholder; `frontend/` placeholder
- Workspace-level: `rust-toolchain.toml` pinning **1.94.0** (channel, not `stable` — an unpinned channel means your machine, CI and the Pi can silently compile with different compilers), `rustfmt.toml`, clippy config (deny warnings in CI), shared `[workspace.dependencies]` (tokio 1.53, axum 0.8, serde, thiserror, tracing)
- Workspace `[workspace.package]`: `license = "MIT"`, `rust-version = "1.94"`, `edition = "2021"`, authors — inherited by every crate via `license.workspace = true`
- Dependency lint: a CI-runnable script (`scripts/check-deps.sh` or cargo-deny bans) asserting the allowed-dependency matrix of ADD §5.6 (e.g. `astroctl-drivers` may depend only on `hal` + `core`; `field`/`stack` never on each other; `llm` never on `session`/`hal`)
- `.gitignore` (target/, node_modules/, dist/, __pycache__/)

Out of scope: any functional code, CI workflow files (M0-T07).

## Acceptance criteria

- [ ] `cargo build --workspace` and `cargo clippy --workspace -- -D warnings` succeed
- [ ] Dep-lint script passes, and **fails** when a forbidden edge is added (prove with a temporary test edge, then remove)
- [ ] Fresh clone builds with only the rustup-provided toolchain, and `rustc --version` inside the repo reports 1.94.0 regardless of the user's default (proves the pin works)
- [ ] Every crate's `cargo metadata` reports `license: MIT`
