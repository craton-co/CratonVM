# Garbage Collector Tuning Guide

Audience: developers tuning CratonVM heap behaviour for a specific workload
— Quarkus boot, JFR-recorded benchmarks, low-latency request handlers,
long-running daemons, or test fixtures with tight budget envelopes.

Source: [`gc/src/`](../gc/src/) (34 files, ~62 k LOC, measured 2026-07-30).
The collector
dispatches through the `VmHeap` enum
([`gc/src/vm_heap.rs`](../gc/src/vm_heap.rs)); each backend is a separate
module.

## Choosing a backend

CratonVM ships three collector backends, selected via `VmConfig::gc_algorithm`
([`vm/src/config.rs`](../vm/src/config.rs)):

| Backend | Module | Status | Best for |
|---|---|---|---|
| **Generational** (default) | [`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs) | Production | Latency-sensitive workloads. Young copying + old free-list + write barriers + card table. STW pauses bounded by live young-set size. |
| **G1** (Garbage-First) | [`gc/src/g1.rs`](../gc/src/g1.rs) | Production | Throughput-oriented workloads on larger heaps. Region-based, mixed young/old collections, optional concurrent marking. STW today; parallel evacuator deferred. |
| **ZGC** | [`gc/src/zgc.rs`](../gc/src/zgc.rs) | Experimental, gated behind `--features zgc` | Not yet wired into `VmConfig::GcAlgorithm`; treat as a research vehicle. |

Trade-offs at a glance:

- **Generational** has the simplest configuration surface and the tightest
  minor-GC pause envelope. The default young from-space is 64 MiB
  (`DEFAULT_YOUNG_SEMI_SIZE` in [`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs))
  which keeps copy latency in single-digit ms for typical workloads.
- **G1** breaks the heap into 1 MiB regions and selects collection sets
  per pause target. It pays a per-store remembered-set cost (~10 ns) but
  amortises full-heap compaction. Use it when the old generation is large
  and reclamation latency matters more than minor-GC throughput.
- **ZGC** is stub-only today. Do not depend on it in production.

## Sizing knobs

### Heap totals (`VmConfig`)

- `max_heap_size` — `-Xmx` equivalent. Default 256 MiB. Sizes the entire
  managed heap; the backend partitions it into young/old (generational) or
  regions (G1).
- `initial_heap_size` — `-Xms` equivalent. Default 16 MiB. Initial
  commitment for the young space and starting old free-list capacity.

### Generational (`gc/src/gen_heap.rs`)

| Constant | Value | Rationale |
|---|---|---|
| `DEFAULT_YOUNG_SEMI_SIZE` | 64 MiB | Bumped from 16 MiB after RealWorldBench GC-stress finding (commit `69fa153`). Smaller young = more frequent minor GCs; larger young = longer copy latency but more time for objects to die. |
| `YOUNG_GC_THRESHOLD_PERCENT` | 75 | Minor GC fires when from-space utilisation crosses 75 %. Raising it (e.g. 90) reduces GC rate but increases the average pause copy-set. |
| `MAX_HEAP_EXPANSION_FACTOR` | 4 | Young from-space can grow up to 4× initial when minor GC reclaims < 25 %. **No shrink path today** — bursty workloads keep the larger footprint. |
| `PROMOTION_AGE` | 3 | Survive this many minor GCs before being promoted to old gen. Lower (1–2) sends short-lived but slightly-tenured objects to old prematurely; higher (4–6) costs more copy work but keeps the old gen denser. Bump when minor GC pauses are fine but the old gen fills too quickly. |

There is no public setter for the four constants today; embedders who need
non-default sizing should construct `GenerationalHeap::with_capacity` (or
`with_sizes`) directly, then build the `VmHeap` around it. The defaults
are deliberate and cover the vast majority of workloads.

### G1 (`gc/src/g1.rs`)

`G1CollectorConfig` is exposed publicly:

| Field | Default | Effect |
|---|---|---|
| `heap_size` | 256 MiB | Total managed heap. |
| `region_size` | 1 MiB | Granularity for evacuation and remembered-set tracking. Power-of-two; raising to 4 MiB reduces RSet overhead at the cost of more wasted bytes per part-full region. |
| `max_gc_pause_ms` | 200 | Target STW pause; the heuristic in `update_ihop` adjusts the IHOP threshold to try to honour it. |
| `ihop_percent` | 70 | Heap-occupancy trigger for the concurrent marking cycle. Raised from HotSpot's 45 % because our default heap is small (256 MiB); on a 16 GiB heap you'd want this back near 45. |
| `promotion_age` | 15 | G1's own tenuring threshold (independent of generational's `PROMOTION_AGE`). |
| `gc_worker_threads` | 4 | **Currently a no-op** — every backend STW-collects on the calling thread. Documented foot-gun; future parallel evacuator will honour it. |
| `string_dedup_enabled` | false | Off by default. |
| `mixed_gc_count_target` | 8 | Number of mixed-collection cycles after marking. |
| `old_cset_region_threshold_percent` | 10 | Cap on old regions evacuated per mixed GC. |

### TLAB (Thread-Local Allocation Buffer)

Thread-local allocation lives in [`gc/src/tlab.rs`](../gc/src/tlab.rs).

| Constant | Value | Rationale |
|---|---|---|
| `DEFAULT_TLAB_SIZE` | 256 KiB | Raised from 64 KiB for allocation-storm workloads. |
| `MIN_TLAB_SIZE` | 8 KiB | Floor for the adaptive shrinker. |
| `MAX_TLAB_SIZE` | 1 MiB | Ceiling for the adaptive grower. |

The `TlabPressureTracker` doubles on fast-fill and halves on idle; the
JIT-friendly fast path reads cursor/end fields directly from the TLAB
struct. Larger TLABs reduce contention on the shared allocator at the cost
of internal fragmentation (every thread carries up to `MAX_TLAB_SIZE` of
unallocated reserve).

## `gpu-offload` interaction

When CratonVM is built with `--features gpu-offload`, a method that runs on
the GPU holds a `SafepointToken`
([`gc/src/safepoint.rs`](../gc/src/safepoint.rs)) for the duration of the
kernel launch + download. While *any* token is alive:

- `Heap::collect_garbage` and `GenerationalHeap::collect_garbage` spin on
  the live-token counter before entering the STW window.
- G1 honours the same counter in [`gc/src/g1.rs`](../gc/src/g1.rs)
  before `collect_garbage` proceeds.

A token is `RAII`: dropping it decrements the counter; if a worker thread
panics, drop-order still releases the GC. The interaction is documented at
the [`Heap::enter_gpu_critical`](../gc/src/heap.rs) API. The practical
effect for tuning: a long GPU kernel can defer GC, growing the young set
and forcing a larger copy when GC finally runs — keep GPU work bounded or
checkpoint between launches.

## Concurrent vs STW

Generational and G1 are stop-the-world today. Concurrent marking is
optionally enabled via `GenerationalHeap::enable_concurrent_gc`
([`gc/src/gen_heap.rs:1374`](../gc/src/gen_heap.rs)) and
`VmHeap::enable_concurrent_gc`
([`gc/src/vm_heap.rs:703`](../gc/src/vm_heap.rs)); it wires the SATB queue
and tri-colour mark state ([`gc/src/concurrent_mark.rs`](../gc/src/concurrent_mark.rs))
into the collector, allowing the mark phase to run alongside mutators
between two short STW pauses.

The trade-off:

- **Concurrent off (default)**: simpler, predictable, single-STW. Pause is
  proportional to live-set size.
- **Concurrent on**: two short STW pauses (initial-mark + remark) plus a
  longer wall-clock marking interval where mutators do extra
  pre-store-barrier work (SATB log write per reference store). Lower
  pauses, higher throughput overhead.

Enable concurrent marking only when latency budgets demand it; the
write-barrier cost is non-trivial on allocation-heavy workloads.

## Diagnostics

The collector emits JFR events
([`jfr/src/builtin.rs`](../jfr/src/builtin.rs)) for every minor and major
collection:

- `jdk.GarbageCollection` — wall-clock pause, generation, cause.
- `jdk.GCPhasePause` — per-phase breakdown.
- `jdk.YoungGarbageCollection` / `jdk.OldGarbageCollection` — bytes
  reclaimed, before/after sizes.

Start a recording with the standard `JFR.start` JMX command or `--Xlog
gc*=info:stdout:time,level,tags`. Dump with `JFR.dump`. The recording can be
opened in JDK Mission Control or any JFR-aware analyser.

For an in-process view:

- `--verbose:gc` prints a one-line summary per collection to stderr (see
  [`docs/CONFIG.md`](CONFIG.md)).
- `GenerationalHeap::stats()` returns a `HeapStatsSnapshot`
  ([`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs)) with allocation counters,
  bytes promoted, dirty-card scan counts, and pause histograms.
- Allocation-rate spikes show up as elevated `bytes_allocated_since_last_gc`
  / dirty-card-scan counters in the snapshot.

In `release` builds, `tracing::debug!` and `tracing::trace!` calls compile
out (workspace `release_max_level_info` lint); the JFR path is your
production diagnostic channel.

## Common symptoms and remedies

| Symptom | Likely cause | First thing to try |
|---|---|---|
| Long minor-GC pauses (50 ms+) | Young space too large, or too many surviving objects | Shrink `DEFAULT_YOUNG_SEMI_SIZE` *or* lower `PROMOTION_AGE` so survivors leave the young set sooner. |
| Frequent minor GCs (multiple per second) | Young space too small, or allocation storm | Grow `DEFAULT_YOUNG_SEMI_SIZE`. Confirm with `bytes_allocated_since_last_gc` rate. |
| Allocation stall / `OutOfMemoryError` under steady-state load | Old gen filling without reclaim | Switch to G1 (better old-gen compaction), or bump `max_heap_size`. Check for retention leaks via HPROF dump (`-XX:+HeapDumpOnOutOfMemoryError`). |
| Long pauses every Nth collection | Old-gen full GC (compaction) | Raise `max_heap_size`, or move to G1 with `enable_concurrent_gc` so old-gen work overlaps mutators. |
| Steady RSS growth over hours/days | Slow-burn retention leak, *or* young from-space expanded to 4× initial and never shrank | Check `GenerationalHeap::stats()`: if young from-space is at `MAX_HEAP_EXPANSION_FACTOR × DEFAULT_YOUNG_SEMI_SIZE`, this is expected (no shrink path). Otherwise HPROF dump and look for reference cycles outside collectable roots. |
| Throughput regression after enabling `gpu-offload` | GC deferred by a long-lived `SafepointToken` | Break GPU work into shorter kernels; observe the token-alive counter in `Heap::stats()`. |
| Per-object `RemSet` overhead in G1 | Cross-region write storm | Raise `region_size` to 2 MiB or 4 MiB to reduce the number of distinct regions an old object can point into. |

## Further reading

- Architecture overview: [`ARCHITECTURE.md`](../ARCHITECTURE.md).
- Per-module algorithm notes: every `gc/src/*.rs` file starts with a `//!`
  doc block (e.g. [`gc/src/g1.rs`](../gc/src/g1.rs),
  [`gc/src/concurrent_mark.rs`](../gc/src/concurrent_mark.rs),
  [`gc/src/satb.rs`](../gc/src/satb.rs)).
- Lock hierarchy (heap is L8): [`vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs).
- Embedding the VM from Rust: [`docs/EMBEDDING.md`](EMBEDDING.md).
- Profiling: [`docs/PROFILING.md`](PROFILING.md).
