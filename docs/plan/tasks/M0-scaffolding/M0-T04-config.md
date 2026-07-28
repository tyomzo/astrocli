# M0-T04 — Configuration loading and validation

**Milestone:** M0 · **Depends on:** M0-T01 · **Crates:** astroctl-core
**Size:** M · **Status:** not started
**Spec:** SDD §4.4; PRD §8.1/§8.2 (YAML shapes, verbatim)

## Objective

Typed, validated, fail-loud configuration for both nodes; a typo is a startup error naming
the offending key, never silent default behavior.

## Scope

- `FieldConfig` / `StackConfig` serde structs mirroring PRD §8.1/§8.2 exactly, `#[serde(deny_unknown_fields)]` at every level (Phase-1-unused sections like `llm:`/`ml:` still parse and validate — the operator's file must not need trimming)
- Post-parse validation pass: ranges (baud, ports, `min_altitude_degrees ∈ [0,45]`, TTL bounds, thresholds), cross-field rules, path expansion
- Error reporting: unknown key / bad value errors include YAML path and an actionable message
- Loader returns `Arc<FieldConfig>`; no re-read API (SDD §4.4)
- Ship `config/field-node.example.yaml` and `config/stacking-server.example.yaml` copied from the PRD examples — these must parse

## Acceptance criteria

- [ ] Both PRD example YAMLs load and validate unchanged — this is the drift guard: `deny_unknown_fields` means PRD §8.1/§8.2 and these structs must agree exactly, in both directions. If the SDD references a key the PRD examples lack, the PRD is wrong and gets fixed (version bump + change note); do not add the field silently
- [ ] Keys the design depends on are present and validated, not merely parsed: `mount.limits.slew_ttl_default_ms`/`slew_ttl_max_ms` (SDD §5.8.1), `server.max_command_age_ms` (§5.8.1), `camera.ops_via_cli` (§5.3.3), `camera.timeouts` (§5.3.1), `mount.serial` (§5.2.4), `storage.*` (§5.5, REL-12), `stacking_server.pacing` (§8.3.7), stack-side `workers.*` (§5.12.3)
- [ ] Fixture tests: unknown key, out-of-range value, missing required section — each error names the YAML path
- [ ] `auth_token_env` handling: env var name captured, value never stored in the config struct debug output (redacted Debug impl)
