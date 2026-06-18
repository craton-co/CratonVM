# Round-7 JFR Review

Scope: `jfr/` crate. Audit of round-6 wave-1 SPSC consumer serialization fix plus carry-over and new findings.

---

## (A) Audit of round-6 SPSC fix in `jfr/src/repository.rs`

**Verdict: CORRECT.** All four invariants hold:

1. **CAS ordering** (`repository.rs:459-462`, `:503-506`): `compare_exchange(false, true, Acquire, Relaxed)`. Acquire on success synchronizes-with the prior consumer's Release-store on unlock (lines 477/490/529). Loser uses Relaxed and just returns `None`.
2. **No slot read before CAS**: in both `try_pop` (line 454) and `drain_into` (line 501), the CAS is the very first operation. Loser returns immediately before reading `tail`, `head`, or any slot.
3. **Unlock release ordering** (`:477`, `:490`, `:529`): the Release store of `false` on `consumer_busy` happens after the slot `assume_init_read` AND after the `tail.store(_, Release)`. Correct.
4. **Producer never touches `consumer_busy`**: `push` (lines 391-441) only touches `head`, `tail`, `cached_tail`, and slots. Verified by grep.
5. **`cached_tail` is producer-exclusive**: read/written only in `push` (lines 407, 416); never accessed by `try_pop`/`drain_into`/`drain_all`. Sound.

---

## Findings

### CRIT-1 — Producer's `Drop` reads `consumer_busy`-protected `tail` without acquiring the gate
**`jfr/src/repository.rs:544-559`** — `Drop::drop` reads `*self.tail.get_mut()` and drains, but `Drop` runs when the last `Arc<SpscEventRing>` is released. If a slow drainer holds the `consumer_busy` gate via `drain_into` and the registry's read-lock is dropped before that returns, no UB occurs (the gate-holder still owns its `Arc` clone via the `shards` snapshot in `drain_all`). However, `get_mut` requires `&mut self` — sound only because `Arc::get_mut` already proves exclusivity. **OK, but add a debug_assert** that `consumer_busy == false` at drop, to catch leaked gate locks during testing.

### HIGH-2 — `dump_recording` triple-pass over events + `make_contiguous`
**`jfr/src/recording.rs:338-341`** — calls `repository_mut().events()` (which does `make_contiguous`, may shift up to `max_events` bytes), then two more iterations for `min(start_time)` and `max(end_time)`. Fold into one pass:
```rust
let (start_time, end_time) = rec.repository().iter().fold((u64::MAX, 0u64),
    |(s, e), ev| (s.min(ev.start_time), e.max(ev.end_time)));
let start_time = if start_time == u64::MAX { 0 } else { start_time };
```
Drops `make_contiguous` entirely (the subsequent `dump_to_file` uses `iter()` not `events()`).

### HIGH-3 — `events_by_type` allocates a `Vec<&EventInstance>` per call
**`jfr/src/repository.rs:148-161`** — return `impl Iterator<Item = &EventInstance>` instead; callers can `.collect()` if they truly need a Vec. Avoids hot-path allocation in any future per-type dump pass.

### HIGH-4 — `dump_to_file` always `fsync`s; no opt-out for periodic snapshots
**`jfr/src/dump.rs:678`** — `sync_all()` on every dump (cost: 10-50ms on SSD, much worse on HDD). Add `durable: bool` param (or wrapper `dump_to_file_periodic` skipping `sync_all`); the atomic rename still gives crash-consistency between dumps even without fsync. Reserve fsync for `dump_on_exit`/`stop_recording`.

### HIGH-5 — `Arc::from(&str)` still on ~25 hot emit paths
**`jfr/src/builtin.rs` lines 950-952, 979-980, 1013, 1044, 1103-1104, 1138-1139, 1167-1168, 1229, 1296, 1325, 1460-1461, 1606, 1841, 1922** — each call reallocates+copies the UTF-8 body per emit. Change the hot signatures from `&str` to `Arc<str>` and have runtime pre-intern at the CP/symbol layer (already done for class-load per the round-5 comment at line 911). Highest payoff: `emit_class_load_event`, `emit_allocation_sample_event`, `emit_execution_sample_event`.

### MED-6 — Missing emit fns for `jdk.JavaErrorThrow` and `jdk.PhysicalMemory`
**`jfr/src/builtin.rs:790-804` (JavaErrorThrow), `:709-723` (PhysicalMemory)** — registered but no emit fn. `emit_java_exception_throw_event` already exists at line 1828; clone it for `JavaErrorThrow` (identical schema). `PhysicalMemory` is one-shot per chunk and trivial: 2 long fields, no thread, no stacktrace.

### MED-7 — `serialize_event_into` writes absolute u64 timestamps; per-event delta would halve event bytes
**`jfr/src/dump.rs:~620-654`** — JFR v2 supports varint encoding; once interned, a `start_time - chunk_start` delta is typically <2^24 ns (16ms) and compresses to 3 bytes instead of 8. Apply same to `end_time - start_time` duration. Estimated 30-40% size reduction for steady-state allocation/execution-sample dumps.

### MED-8 — `drain_per_thread_into_repository` snapshot lost on drop with zero running recordings
**`jfr/src/recording.rs:267-300`** — when `running_len == 0` (line 273), the drained `Vec<EventInstance>` is silently dropped. With the round-6 serialization fix, a producer-becomes-consumer race that loses its CAS skips the shard — but if recordings stopped between drain start and this method's snapshot, events drained from shards are now gone. Two fixes: (a) check `running_ids.is_empty()` BEFORE calling `drain_all`; (b) document explicitly that this is intended drop-on-no-recordings.

### LOW-9 — `drain_all` does not preserve per-thread block locality for SmallVec<EventValue; 8>
**`jfr/src/repository.rs:666-681`** — Wave-3 TODO from round-5 (SmallVec<EventValue; 8>) is still open. Without this, every `EventInstance.fields` (typically 2-5 values) allocates a heap `Vec`. Confirm sizeof(EventValue)*8 ≤ 64 bytes before committing; if larger, use `[EventValue; 4]`.

### LOW-10 — `extra_events.sort_by_key` in dump is admitted unnecessary
**`jfr/src/dump.rs:561-562`** — comment at lines 552-560 explicitly states JFR v2 doesn't require global ordering. With `dump_recording` passing `Vec::new()` (recording.rs:351), the sort is dead code on the production path; only test/standalone dumps trigger it. Either gate behind `#[cfg(test)]` or note it as a debug aid.

---

(C) Notes on new angles: StringPool already exists (`dump.rs:185`) and dedups all `String`/`Str` payloads chunk-wide — finding 7 (delta-encoded timestamps) is the remaining big size win. Zero-padding in header (`dump.rs:506`) is 7 bytes per chunk — not worth chasing.
