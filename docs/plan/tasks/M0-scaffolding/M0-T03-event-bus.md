# M0-T03 — Event schema and event bus

**Milestone:** M0 · **Depends on:** M0-T01 (types from T02 when available) · **Crates:** astroctl-core
**Size:** S · **Status:** not started
**Spec:** SDD §4.3 (schema + topic table), §7 (bus capacity/lagging semantics), ADD §6.2

## Objective

One event pipeline: the same `Event` struct feeds WS clients and the session JSONL log.

## Scope

- `Event { v, ts, topic, data }` and the closed `Topic` enum with dotted serialization (`mount.position`, …) — all Phase 1 topics from SDD §4.3 table
- Typed payload structs per topic (serialize into `data`); constructor helpers so call sites can't emit malformed payloads
- `EventBus` wrapping `tokio::sync::broadcast` (capacity 256); publish + subscribe; lagged-receiver detection surfaced to the subscriber (they resync via snapshot — hub's job, later)
- JSONL sink task: subscribes, appends one line per event to a session log path handed to it; flush discipline suitable for crash-tolerant logs (line-buffered)

Out of scope: WS hub (M0-T05/M1), snapshot mechanism, `liveview.frame` binary path (M1-T09).

## Acceptance criteria

- [ ] Serialized event matches the documented JSON shape (golden test incl. topic strings, RFC 3339 ms timestamps)
- [ ] Slow subscriber gets an explicit `Lagged` signal, bus never blocks publishers
- [ ] JSONL sink: events written in order, file parseable line-by-line after abrupt kill (test with process abort)
