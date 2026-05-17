# Round 4 — JFR crate review

Reviewer: senior Rust performance reviewer
Scope: `jfr/src/{lib,event,repository,recording,dump,stream,builtin}.rs`
Round 3 baseline: `is_enabled()` gate, per-site `OnceLock<EventTypeId>` cache, `Str(&'static str)`, `serialize_event_into` scratch reuse, `StringPool`, smallvec dep, per-thread shards.

The Cargo.toml lists `smallvec` and claims it backs `EventInstance.fields`, but the
actual type is still `Vec<EventValue>`. That mismatch is the root of three of the
findings below; the remaining items are concurrency/correctness bugs and several
event-type stubs.

---

## 1. `[CRIT]` `dump_to_file` consumes the global ring even when called per-recording — events lost to all but the first dumper

`jfr/src/dump.rs:528-536` (the standalone `dump_to_file` function) unconditionally
calls `global_ring_registry().drain_all()` and writes those events into *this one*
output file. `FlightRecorder::dump_recording` (`jfr/src/recording.rs:324-340`)
*also* drains via `drain_per_thread_into_repository` before invoking
`dump_to_file`, so any event emitted in the gap between those two calls is
drained by `dump_to_file`, written into recording A's `.jfr`, and never reaches
recording B's repository. With ≥ 2 concurrent recordings (a documented supported
scenario — see `test_multiple_simultaneous_recordings`), the second/third dump
will be missing events that the first dump silently stole.

**Impact:** silent event loss across concurrent recordings; reproducible whenever
two recordings dump within a short window.

**Fix:** remove the `drain_all()` from `dump_to_file` (make it a pure serializer)
and require callers to have already moved events into the supplied
`EventRepository`. `dump_recording` already does this via
`drain_per_thread_into_repository`; the standalone path should expose a
`drain_into(&mut EventRepository)` helper instead of taking from the shared
global ring inside the writer.

---

## 2. `[CRIT]` `push_to_thread_ring` takes a `std::sync::Mutex` lock per event despite the SPSC-ring docs

`jfr/src/repository.rs:348-385`. The doc-comments in `lib.rs:4-15` and the
audit comment at `repository.rs:10-17` advertise a lock-free per-thread shard,
but `push_one` actually does `ring.lock()` on a `std::sync::Mutex<VecDeque<…>>`
for every event. Each push therefore performs a futex/atomic CAS even though
only the owning thread ever writes; the only reason it's "uncontended" is that
the dumper rarely fires. Every emit also calls
`global_ring_registry().shard_capacity()` (`repository.rs:378`) inside the lock,
re-deref-ing the `OnceLock` and the registry mutex isn't touched but the
capacity could just be cached on the shard.

**Impact:** every JFR-enabled emit pays an atomic RMW plus a function-call
indirection per push (~10-30 ns on modern x86). At 1M events/s/thread that's
~10-30 ms/s of pure lock overhead per thread, *just* to serialize against a
once-per-second dumper.

**Fix:** make the shard `Arc<(Mutex<VecDeque>, AtomicUsize_or_capacity_const)>`
where the cap is captured at shard creation, and consider switching the per-
shard `Mutex` to `parking_lot::Mutex` (already on the abandoned-dep list) or a
true SPSC ring (e.g. `crossbeam_queue::ArrayQueue`) so the producer no longer
syscalls under contention.

---

## 3. `[HIGH]` Every `emit_*` allocates a `Vec<EventValue>` (and an `Arc<str>` per string param)

All 30+ emitters in `jfr/src/builtin.rs:846-1828` build `fields: vec![...]` on
the hot path, e.g. `emit_gc_event` at line 862 allocates a 5-element `Vec` plus
two `Arc::from(name)` heap copies *for every GC event*. The `EventValue::Str`
zero-alloc variant added in round 3 is never used by any emitter — `name`,
`cause`, `state`, `monitor_class`, etc. are wrapped in `Arc::from(...)`
unconditionally. Likewise `EventInstance.fields: Vec<EventValue>`
(`event.rs:75`) is not the `SmallVec<[EventValue; 4]>` the Cargo.toml comment
(line 18-19) claims.

**Impact:** 3-7 heap allocations per emitted event (1 `Vec` + N `Arc<str>`).
On the dominant hot paths (allocation sample, exec sample, monitor wait) this
is 60-200 ns/event of pure allocator traffic, dwarfing the actual ring push.

**Fix:**
1. Change `EventInstance.fields` to `SmallVec<[EventValue; 6]>` (matches the
   widest builtin event); add `impl From<[EventValue; N]>` so emitters can
   write `fields: smallvec![...]`.
2. Add overloads (or accept `Into<Cow<'static, str>>`) on emitters so callers
   passing literal `"G1 Young"` end up in the `Str(&'static str)` variant with
   no heap allocation. The current API forces `&str` → `Arc<str>` even when
   the underlying lifetime is `'static`.

---

## 4. `[HIGH]` `EventRepository.type_index` grows unboundedly with `total_recorded`, eviction is O(n)

`jfr/src/repository.rs:30-73`. `type_index` keys events by absolute index
(`total_recorded as usize`). On eviction the code does
`indices.iter().position(|&i| i == self.base_index as usize)` (line 54) — a
linear scan of the per-type index vector — to find and `swap_remove` the
evicted slot. For a workload that emits 1M events of one hot type into a
ring-buffered repository (default capacity 100k), every push beyond the first
100k pays an O(100k) scan, and the index vector itself keeps growing because
nothing ever shrinks the `Vec<usize>` capacities. Worse, `abs_idx` is a
`u64` cast to `usize` and stored — after `total_recorded > u32::MAX` on a
32-bit target this silently wraps.

**Impact:** repository becomes quadratic at steady-state once the ring is full,
which is the common case for long-lived disk-backed recordings.

**Fix:** replace the `Vec<usize>` per type with a `VecDeque<u64>` (so eviction
is a `pop_front`), or drop the type index entirely on the eviction path —
`events_by_type` is only ever called from cold query/test paths, so a linear
filter over `events` is fine and avoids the maintenance cost on every push.

---

## 5. `[HIGH]` `drain_all` races with concurrent producers — events pushed during the drain are dropped from the dump

`jfr/src/repository.rs:288-303`. The drainer snapshots the `Vec<Arc<…>>` of
shards under the registry lock, then iterates each shard, locks it, and
`drain(..)`s. If thread T pushes an event between the snapshot and the lock
on T's shard, the event is included in *this* drain — fine. But if T pushes
after T's shard has been drained, the event sits in the ring until the *next*
drain, which is normal — *except* the dumper has already started writing
events from T into the current `.jfr` (interleaved order). The drained-events
vector also is *sorted by `start_time`* in `dump.rs:532`, which means the
"second half" of a thread's events end up in the next chunk with overlapping
timestamps but the file readers expect chunks to be monotonic per-thread.
There is also no rotation: a producer that emits between
`drain_per_thread_into_repository` and `dump_to_file`'s own `drain_all`
contributes to file A only — those events vanish from every other
recording's view (see finding #1).

**Impact:** non-deterministic event ordering and per-thread time inversion in
the dump; reader tools that depend on monotonic per-thread timestamps
(e.g. JMC) misrender stacks.

**Fix:** add a per-shard "rotation" — when the dumper wants a snapshot, it
swaps the shard's `VecDeque` with a fresh empty one atomically (single
`mem::swap` under the lock) and walks the swapped-out copy. Producers
continue writing to the new deque with no overlap. Combine with finding #1:
make the dumper a pure consumer of the drained vector, with no further
side effects on the global registry.

---

## 6. `[MED]` `dump_to_file` `fsync`s every chunk; no batching / incremental dump

`jfr/src/dump.rs:647-648`. After every dump the writer flushes and calls
`sync_all()` (Windows: `FlushFileBuffers`; Linux: `fdatasync`), forcing a disk
barrier. For a `dump_on_exit`-only recording this is correct, but the
`drain_per_thread_into_repository → dump_to_file` path is also used for
periodic snapshots (e.g. JMX-triggered, JFR `dumpOnExit=false` with periodic
dumps). On a hot recording this is 5-50 ms of stall *per dump*.

**Impact:** dump throughput capped at disk barrier rate; visible in benchmarks
that dump every few seconds.

**Fix:** make the fsync optional via a `DumpOptions { sync: bool }` parameter
(default true for shutdown / `dump_on_exit`, false for periodic snapshots —
the OS page cache + `.part`-rename already gives crash consistency for
periodic chunks).

---

## 7. `[MED]` Pre-pass `intern_event_strings` copies every string payload into a `Vec<u8>` for the HashMap key

`jfr/src/dump.rs:178-225` (`StringPool::intern`). `intern` does
`bytes.to_vec()` + `.clone()` on insert (line 196-197) — two heap allocations
per unique string at dump time, plus the lookup `by_bytes.get(bytes)` requires
a temporary `Vec<u8>` allocation on misses. Then the pre-pass at
`dump.rs:570-575` walks all repository + drained events *twice* (once here,
once during serialize) before serialization can start.

**Impact:** dump-time CPU cost scales as O(events × strings_per_event); for a
hot recording with 1M GC/alloc events this is 4-8M extra allocations during
the dump barrier.

**Fix:** key the pool by `Arc<str>` ptr-equality + a fallback for `&'static str`
(or use `bytes::Bytes`) so existing `Arc<str>` payloads share their backing
allocation with the pool entry instead of being copied. Equivalently: replace
`FxHashMap<Vec<u8>, u32>` with `FxHashMap<&'a [u8], u32>` using a `&'a` borrow
into the original `EventInstance`, so the pool's lifetime is the dump and
nothing is copied.

---

## 8. `[MED]` 11 registered event types are stubs — never emitted

`jfr/src/builtin.rs`: types registered at lines 580 (`NativeMethodSample`),
615 (`SystemProcess`), 631 (`InitialEnvironmentVariable`), 661
(`GCReferenceStatistics`), 712 (`PhysicalMemory`), 728 (`ContainerCPUUsage`),
745 (`ContainerMemoryUsage`), 762 (`ExceptionStatistics`), 793
(`JavaErrorThrow`), 809 (`ModuleRequire`), 825 (`ModuleExport`) have *no*
`emit_*` function. The `detailed_profile()` (lines 1942-1998) and
`default_profile()` (lines 1907-1940) reference some of them
(`GCReferenceStatistics`, `JavaErrorThrow`) as `enabled: true`, so users will
see "active" settings that produce zero events.

**Impact:** dump files claim 47 event types but only 33 ever appear; JMC users
asking "where's the JavaErrorThrow history?" see nothing.

**Fix:** add the missing `emit_*` helpers (sketch the signatures from the field
lists already in the registry) and wire them at the obvious VM-side
call-sites — or delete the unused registrations until they're actually emitted,
so the metadata reflects reality.

---

## 9. `[MED]` `emit_*` hardcodes `thread_id: 0` for ~16 event types — drops thread attribution

`jfr/src/builtin.rs`: see lines 861, 892, 952, 1098, 1186, 1269, 1294, 1318,
1344, 1399, 1426, 1455, 1483, 1654, 1679, 1769. Functions like
`emit_class_load_event`, `emit_compilation_event`, `emit_gc_phase_pause_event`,
`emit_cpu_load_event` etc. write `thread_id: 0` even though the registered
event types have `has_thread: true` and a real `EventField{name: "thread", …}`
declared. JMC/`jfr print` then groups every class-load, every JIT compile,
every safepoint under "thread 0", which is useless for performance analysis.

**Impact:** thread attribution lost for the bulk of jdk.* events in any dump.

**Fix:** add a `thread_id: u64` parameter to each of those emit helpers and
plumb the current thread ID from the caller (most call-sites already know it
because they're constructing the event from inside the runtime).

---

## 10. `[LOW]` `dump_recording` makes two passes over events for `start/end_time` and forces `make_contiguous`

`jfr/src/recording.rs:334-337`. `events()` calls `VecDeque::make_contiguous`
(`repository.rs:78`), then `.iter().map(…).min()` and `.iter().map(…).max()`
are two more passes. For a 100k-event dump this is ~3 full passes over the
event vector just to compute the file header timestamps; can be one fold:

```rust
let (start, end) = events.iter().fold((u64::MAX, 0), |(s,e), ev|
    (s.min(ev.start_time), e.max(ev.end_time)));
```

and `EventRepository` could expose `iter()` (already exists) so
`make_contiguous` isn't forced just to compute a min/max. Minor (cold path)
but trivial to fix.
