# M3-T05 — HIL bring-up (hardware, operator present)

**Milestone:** M3 · **Depends on:** M3-T04, M2 complete · **Crates:** scripts/, docs/evidence/
**Size:** L · **Status:** step 2 done read-only (2026-07-29, `spikes/skywatcher-heq5/FINDINGS.md`); steps 1, 3–6 outstanding
**Spec:** SDD §9 T-HIL-1 (six-step sequence); IMP §2/M3; PRD §4.2 verification note
**Tests gated:** T-HIL-1

## Objective

Scripted, evidence-producing hardware bring-up of the real HEQ5 Pro, strictly ordered from
read-only to full motion. This task is executed *with the operator at the mount*; the agent's
role is preparing the scripts/checklist and analyzing captured results between steps.

## Scope

Execute SDD §9 T-HIL-1 steps, each gated on the previous:
1. **No power:** T-COD-1 suite green; serial traffic logger tool ready (`scripts/synta-sniff`)
2. **Power, read-only:** handshake session — version/CPR/timer-freq; compare against EQMOD reference for HEQ5; upgrade T01's `derived` vectors to `verified` or fix the codec (any mismatch stops here and updates SDD §5.2.2). **Already executed 2026-07-29 ahead of the driver**: CPR and home confirmed, timer frequency corrected 460,800 → 64,935 in PRD §4.2, nine verified vectors captured. Re-run once the real codec exists, to confirm it reproduces them
3. **Bare mount / clutches loose / no payload:** motion bring-up per `spikes/skywatcher-heq5/MOTION-PLAN.md`, which is **bounded-motion-first by design**: a self-terminating goto proves `G`/`S`/`J` before `K` is ever relied on, then `K` and `L` are proven *inside* a bounded motion where their failure is harmless, and only then are open-ended slews permitted. The plan's E5 re-measures the rate against the corrected timer frequency (predicted 104.73 counts/s at step period 620; the old constant would predict 743 — a 57 s versus 8 s difference in the same commanded goto) — the plan measures the step-period relation inside a *bounded* goto (E5), so that confirmation lands before the stop path is ever relied on. E-stop wire latency and TTL expiry follow once the stop path is verified
   **Gate:** the plan's Phase 0 — action-opcode encodings, especially `G`'s direction/speed-class bit layout, derived from the EQMOD source — must be complete before any powered motion. Guessing the motion-mode byte is how a low-speed test becomes a high-speed slew
4. **Tracking:** sidereal on, overnight position-poll soak (log drift, comms errors)
5. **Under the sky, payload mounted:** goto accuracy loop (known bright stars, manual centering assessment — plate solving arrives Phase 2a), park/unpark
6. **Limits:** altitude rejection and meridian auto-stop demonstrated with real geometry
- Evidence bundle `docs/evidence/m3/`: serial logs per step, timing measurements, checklist signed off, deviations → SDD updates
- Update example config: `mount.driver: skywatcher` with simulator documented

## Acceptance criteria

- [ ] All six steps pass in order, evidence archived; any opcode/parameter deviation reflected in SDD + codec vectors before proceeding past step 2
- [ ] E-stop budget **(b)** of ADD §9.1 demonstrated on real hardware: ≤ 100 ms from API call to motion ceasing, log-timestamped. This is a different quantity from budget (a) (≤ 20 ms handler-to-wire, asserted in CI by T-SER-3 in M3-T02/T04) — (b) adds 9600-baud transmission and motor response. Record both numbers; quoting one where the other is meant is how the two thresholds drifted apart in the first place
- [ ] **IMP §2/M3 exit = PRD Phase 1 exit criteria demonstrated end-to-end on real hardware and archived as the M3 demo recording**
