# Round-5 JFR audit (2026-05-17)

Audits round-4 SPSC ring + RwLock + StringPool + taxonomy changes in `jfr/`.

## 1. [CRIT] Multi-consumer race on `SpscEventRing::try_pop` — UB hazard

**File:** `jfr/src/repository.rs:545-560` (`ThreadRingRegistry::drain_all`).

`drain_all` is callable from any thread and snapshots every shard under a
*read* lock, then pops each one. But `drain_all` itself is invoked from
three unsynchronized paths:

- `recording.rs:268` (`drain_per_thread_into_repository`) — any caller of
  `dump_recording`.
- `recording.rs:696,718` — test fixtures running in parallel.
- `dump.rs:1309,1571,1602,1651,1674` — standalone dump paths.

Because `parking_lot::RwLock::read()` permits concurrent readers, two
threads can call `drain_all` simultaneously and BOTH call `try_pop` on the
*same* `SpscEventRing` shard. That violates the single-consumer contract
declared at `repository.rs:308`, and the unsynchronised slot read at
`repository.rs:403` (`assume_init_read`) becomes a data race → UB.

**Fix:** add a `consumer_busy: AtomicBool` (CAS-acquire) on `SpscEventRing`
that `try_pop`/`drain_into` must take, OR serialize `drain_all` itself
behind a `Mutex<()>` taken before the read-snapshot. The latter is simpler
and matches the actual usage (drains are cold-path, contention is fine).

## 2. [CRIT] Producer can become consumer of its own shard during a concurrent drain

**File:** `jfr/src/repository.rs:609-628` (`push_to_thread_ring`) +
`jfr/src/recording.rs:267` (`drain_per_thread_into_repository`).

Thread T1 registers its shard as producer. If T1 then calls
`drain_per_thread_into_repository` (e.g. from `dump_recording`), T1
becomes the *consumer* of its own shard. SPSC ring rules technically allow
"same thread is both" — but the danger is concurrent: while T1 is
consuming its own shard via `drain_all`, T2 may also be inside
`drain_all` consuming the *same* shard (finding 1). Combined with the
test-only `register_current_thread()` re-fetch at `repository.rs:913` and
`lib.rs:449`, the "single declared consumer" invariant in the module
header (lines 263-269) is unenforced.

**Fix:** as for finding 1 — serialize the consumer side. Additionally
document that `register_current_thread()` returning the producer's own
`Arc<SpscEventRing>` makes `drain_into` callable from the producer; tests
should NOT also race with `drain_all`.

## 3. [HIGH] `Arc::from(&str)` allocation still on every emit at 25+ sites

**File:** `jfr/src/builtin.rs` — 25 remaining `Arc::from(...)` calls,
notably `:950-952` (class load), `:1103-1104` (monitor wait), `:1138-1139`
(monitor enter), `:1296,1325` (allocation), `:1606` (deopt method),
`:1840-1841` (Java error throw).

Round-4 fixed the *taxonomy* enums (`reason`, `action`, `when`, `cause`)
to `&'static str`, but the *dynamic* identifiers (class names, method
descriptors, thread names) still allocate `Arc<str>` per event on the hot
path. Wave-2's wave-3 TODO calls these out but nothing is wired yet.

**Fix:** add `_interned` variants that take `Arc<str>` and update the
runtime CP / class metadata to hand out the pre-interned `Arc<str>`. The
emit site becomes `fields.push(EventValue::String(Arc::clone(&name)))` —
one refcount bump, no allocation.

## 4. [HIGH] `dump_recording` makes 3 passes over events for header timestamps

**File:** `jfr/src/recording.rs:338-341`.

```rust
let events = rec.repository_mut().events();           // pass 1: make_contiguous
let start_time = events.iter().map(|e| e.start_time).min().unwrap_or(0);  // pass 2
let end_time = events.iter().map(|e| e.end_time).max().unwrap_or(0);      // pass 3
```

Three linear scans plus a `make_contiguous` that may move the entire
VecDeque. With 100k events this is wasted work.

**Fix:** single `fold` returning `(min_start, max_end)`; replace
`events()` with `iter()` to skip `make_contiguous`.

## 5. [MED] Deopt emit site passes `thread_id: 0`

**File:** `vm/src/jit/helpers.rs:2066`.

`emit_deoptimization_event(... 0, // thread_id — TODO)` — every deopt
event carries a bogus zero thread ID, making JMC unable to attribute the
deopt to the JIT compiler thread (or to the deoptimizing app thread,
depending on which is semantically correct).

**Fix:** use `cratonvm_jfr::builtin::current_jfr_thread_id()` (already
exists at `builtin.rs:880`). One-line change.

## 6. [MED] `SpscEventRing` per-emit `Acquire` load of consumer tail is contended on producer hot path

**File:** `jfr/src/repository.rs:362` (in `push`).

Every emit loads `tail` with `Acquire`. On x86 this is just a `mov`, but
on ARM64 it inserts an LDAR. The full-check is only needed when the ring
is actually near-full; in steady-state with a frequently-draining
consumer, we could cache the last-seen `tail` in a producer-only field
and only re-read on the rare almost-full branch (standard
"lazy-shared-counter" SPSC pattern, e.g. rtrb crate).

**Fix:** add `cached_tail: Cell<usize>` (producer-local); only re-load
the atomic when `head - cached_tail >= capacity`.

## 7. [MED] `EventRepository::events_by_type` allocates a `Vec` per call

**File:** `jfr/src/repository.rs:148-161`.

Returns `Vec<&EventInstance>` — every consumer (`stream.rs`, dump path,
metric exporters) pays one allocation. Most callers iterate and discard.

**Fix:** return `impl Iterator<Item = &EventInstance> + '_`. Single API
break, but mechanical to update call sites.

## 8. [LOW] StringPool: `Arc::from(bytes)` allocation on every unique entry

**File:** `jfr/src/dump.rs:208`.

The round-4 change replaced two `Vec<u8>` clones with one `Arc<[u8]>`
allocation — good. But the `FxHashMap::get(bytes)` at line 204 must
borrow `&[u8]` against `Arc<[u8]>` keys, which works only because of
`Borrow<[u8]> for Arc<[u8]>`. Verified OK — no fix needed, just noting
the round-4 reduction from 2 allocs → 1 alloc per unique string is real.

## 9. [LOW] `fsync` on every dump is overkill for periodic snapshots

**File:** `jfr/src/dump.rs:678` (`writer.get_ref().sync_all()?`).

Every snapshot fsyncs the full file before the atomic rename. For
high-frequency rolling dumps (e.g. JFR's `--max-age` periodic chunking),
this stalls on slow disks.

**Fix:** add a `durable: bool` parameter to `dump_to_file`; default
`true` for explicit `dump_recording`, `false` for the periodic-snapshot
path in `recording.rs`. The atomic-rename still ensures crash-consistency
of the *previous* good `.jfr` file regardless.

## 10. [LOW] 11 registered event types lack `emit_*` functions

**File:** `jfr/src/builtin.rs:579-825` (stubs).

`NativeMethodSample`, `SystemProcess`, `InitialEnvironmentVariable`,
`GCReferenceStatistics`, `PhysicalMemory`, `Container{CPU,Memory}Usage`,
`ExceptionStatistics`, `JavaErrorThrow`, `ModuleRequire`, `ModuleExport`
appear in the registry (so JMC sees them in the metadata) but no code
emits them. Only `JavaErrorThrow` and `PhysicalMemory` are realistically
wireable today (the rest need OS/container probes the VM doesn't have).

**Fix:** wire `emit_java_error_throw_event` from the VM exception
throw path (`vm.rs` athrow handlers) and `emit_physical_memory_event`
from a periodic GC-thread sampler reading `/proc/meminfo` (Linux) /
`GlobalMemoryStatusEx` (Windows). The rest can stay registered-only.
