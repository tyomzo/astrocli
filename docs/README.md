# AstroCtl — Documentation Pack

AstroCtl is a single self-hosted application for astrophotography: mount control, camera
operation, plate solving, guiding, live stacking, calibration management and post-processing in
one system, driven from a phone or tablet over a VPN. It replaces the usual patchwork of
KStars/Ekos/INDI/PHD2/Siril with one coherent stack. The problem statement and vision are
ASTROCTL-PRD-001 §1–2.

**There is no code in this repository yet.** The pack below is the complete governing
specification; implementation starts at `docs/plan/tasks/M0-scaffolding/M0-T01`.

## The documents

These follow the ISO/IEC/IEEE 12207:2017 chain — intent, then architecture, then design, then
plan. Each document names the versions of its governing documents in its header, and those pins
are expected to be exact.

| Document | ID | Version | Path | Answers |
|----------|-----|---------|------|---------|
| Product Requirements | ASTROCTL-PRD-001 | 1.15.2 | [`intent/`](intent/ASTROCTL-PRD-001.md) | *What must be true?* 288 numbered requirements (`HAL-01`, `PRF-12`, …), hardware scope, configuration schema, phase plan, risk register |
| Architecture Design | ASTROCTL-ADD-001 | 1.4.1 | [`design/`](design/ASTROCTL-ADD-001.md) | *What are the parts and why these parts?* Views, component responsibilities, 13 ADRs with rejected alternatives, crate layout and dependency rules |
| Software Design | ASTROCTL-SDD-001 | 1.11.1 | [`design/`](design/ASTROCTL-SDD-001.md) | *How is each part built?* Rust types, trait signatures, wire protocols, task topology, route tables, storage formats, verification design |
| Implementation Plan | ASTROCTL-IMP-001 | 1.2.0 | [`plan/`](plan/ASTROCTL-IMP-001.md) | *In what order, and how do we know it works?* Milestones M0–M3, workstreams, sizing, definition of done |
| Task breakdown | — | — | [`plan/tasks/`](plan/tasks/README.md) | *What do I do next?* 34 task files, one per reviewable change set |

All four are **Draft** status.

## Where to start

- **Implementing something** → [`plan/tasks/README.md`](plan/tasks/README.md), then the milestone
  README, then the task file. Read the task's `Spec` references before writing code.
- **Deciding whether a change is allowed** → ADD §7 (the ADRs) and ADD §5.6 (dependency rules).
- **Looking up a requirement ID** → PRD; every `XXX-nn` identifier is defined there and nowhere
  else. IDs are stable and never renumbered.
- **Checking how something is meant to behave at runtime** → SDD §5 for the element, §8.3 for
  behavior over a slow or lossy link, §9 for what test proves it.
- **Understanding the shape of the system in five minutes** → ADD §4 and §5.1.

## Conventions that matter

**Requirement IDs.** `HAL`, `MNT`, `CAM`, `SES`, `PLN`, `GDE`, `PLS`, `STK`, `CAL`, `IPP`, `PPR`,
`MLR`, `LLM`, `CMP` (functional); `ARC`, `PRF`, `REL`, `USB`, `EXT`, `SEC` (non-functional).
Ranges are written `HAL-01..07` and mean every ID in the range.

**Test IDs.** `T-COD-1`, `T-SER-3`, `T-E2E-1`, `T-HIL-1`, … are defined in SDD §9. A task that
names a gated test is not complete until that test is green.

**Milestones vs. phases.** The PRD describes *phases* (scope); the IMP describes *milestones*
M0–M3 (sequence). They deliberately differ — IMP §1 lists three declared deviations and states
that the plan's order supersedes the PRD's. When they conflict on sequence, the IMP wins; on
scope, the PRD wins.

**Repository directory vs. artifact names.** The repo directory is `astrocli/`; every artifact
identifier — product, binaries, crates, document IDs — is `astroctl`. This is intentional (ADD
§5.6); do not "fix" either to match the other.

## Keeping the pack truthful

The rule from IMP §5: *the documents stay truthful or they die.* Concretely —

- If implementation deviates from the SDD, update the SDD in the same change set: bump the
  version, add a change note to the header, update the governing-document pin in any document
  that cites it.
- If a task cites a section that does not exist or does not cover the work, that is a defect in
  the pack, not a detail to work around. Fix the document first.
- Contracts (HAL traits, route tables, event schema, error codes, worker IPC) are frozen. See
  [`plan/tasks/README.md`](plan/tasks/README.md) rule 2 for the difference between changing one
  (forbidden inline) and additively extending one (allowed when the task says so).
- Configuration is normative in PRD §8.1/§8.2. Because the config structs use
  `deny_unknown_fields`, a key that exists in the design but not in the PRD is a startup failure,
  not a documentation nit.

## License

MIT — see [`../LICENSE`](../LICENSE). Every crate inherits it via `license.workspace = true`
(M0-T01). The MIT license covers AstroCtl's own code and documentation; the C libraries it binds
to or vendors (libgphoto2, cfitsio, libudev, libsep, and the ERFA source vendored by `erfars`)
carry their own terms — libgphoto2 is LGPL and ERFA is BSD-3-Clause. This matters only if you
distribute binaries, not for personal use.
