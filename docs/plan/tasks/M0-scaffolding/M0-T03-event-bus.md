# M0-T03 — Event schema and event bus

**Milestone:** M0 · **Depends on:** M0-T01 (types from T02 when available) · **Crates:** astroctl-core
**Size:** S · **Status:** done
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

- [x] Serialized event matches the documented JSON shape (golden test incl. topic strings, RFC 3339 ms timestamps)
- [x] Slow subscriber gets an explicit `Lagged` signal, bus never blocks publishers
- [x] JSONL sink: events written in order, file parseable line-by-line after abrupt kill (test with process abort)

## Result notes (implementation)

Delivered in `crates/astroctl-core/src/event.rs` + `src/bus.rs`. Points where SDD §4.3 was
underspecified and this implementation had to choose — each is a candidate SDD edit, none of
them changes a field name or a topic string:

- **`liveview.frame` has no `Topic` variant.** §4.3 lists it but says it is a binary WS frame on
  `/ws/liveview`, never JSON on `/ws` (§8.3(5)). A variant would make an illegal event
  representable; the wire name is exported as `event::LIVEVIEW_FRAME_TOPIC` for the hub's
  subscribe filter instead. `Topic::ALL` therefore has 10 entries, not 11.
- **Enum value sets §4.3 names but does not enumerate:** `pier_side`
  (`east|west|unknown`), `mount.status.state` (`disconnected|idle|slewing|parked|fault`),
  `stack.status.worker_state` (`starting|ready|busy|restarting|failed`, from §5.12.3), and
  `alert.severity` (`info|warning|critical`, matching the §5.9 green/amber/red header).
- **`mount.status` redundancy resolved by construction:** §4.3 carries `state` *and* the
  `slewing`/`parked` booleans. `MountStatus::new(state, tracking)` derives the booleans so the
  payload cannot contradict itself.
- **Unknown-value encoding:** `camera.status.battery_pct`/`storage_free_mb` are `null` while
  disconnected (0 renders as an empty gauge), likewise `transfer.status.oldest_queued_age_s`
  when the queue is empty and `stack.status.worker_state` while the stack node is unreachable.
  Keys are always present.
- **`transfer.status` field set differs from §5.10.4's REST body**, which additionally returns
  `attempts_current`. The event follows §4.3. If they are meant to be the same object, §4.3 or
  §5.10.4 should say so.
- **Integration points left for M0-T02:** `MountPosition`'s coordinates are private `f64` with
  accessors pending `RaHours`/`DecDegrees`, and `Alert.code` is a normalized `String` pending the
  closed `ErrorCode`. Both target types serialize identically, so the swap does not move the wire
  format.
