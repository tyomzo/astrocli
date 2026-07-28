# M2-T05 — Desk integration: real-camera E2E + CR3 preview + soak

**Milestone:** M2 · **Depends on:** M2-T03, M2-T04 · **Crates:** astroctl-pipeline, config, tests
**Spec:** IMP §2/M2 exit criteria; SDD §5.7 (CR3 decode variant), §9 T-SOAK subset

## Objective

The M1 demo, rerun with a real camera: prove the swap changed nothing outside the driver,
and add the CR3 decode path the preview pipeline was structured for.

## Scope

- Preview decoder: add CR3 variant (libraw half-size decode → quarter-res → stretch) to the M1-T09 `SourceFormat` enum; JPEG sibling used when RAW+JPEG format active (cheaper)
- Config: switch example field config camera driver to `gphoto2` with `simulator` documented as the alternative; sim remains CI default
- Desk E2E (scripted, evidence-captured): PWA session — connect R10, settings, live view, 5 timed + 1 bulb capture, previews ≤ 3 s after exposure end (PRF-timing log), frames acked by stack node, stack preview returns
- 2 h soak: capture every 60 s; field node RSS ≤ 512 MB steady (PRF-05 with real decode spikes excluded per definition), no wedges, zero lost frames
- Update `DEMO.md` with the real-camera variant

## Acceptance criteria

- [ ] E2E script passes with evidence bundle (timings, logs, RSS plot) committed under `docs/evidence/m2/`
- [ ] Zero code changes outside astroctl-drivers, the decoder variant, config, and tests — diffstat attached as proof of the contract's integrity
- [ ] IMP §2/M2 exit criteria all demonstrated
