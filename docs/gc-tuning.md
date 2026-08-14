# Garbage Collector Tuning Guide

Audience: developers tuning CratonVM heap behaviour for a specific workload
— Quarkus boot, JFR-recorded benchmarks, low-latency request handlers,
long-running daemons, or test fixtures with tight budget envelopes.

Source: [`gc/src/`](../gc/src/) (34 files, ~62 k LOC).
The collector
dispatches through the `VmHeap` enum
([`gc/src/vm_heap.rs`](../gc/src/vm_heap.rs)); each backend is a separate
module.

## Choosing a backend

CratonVM ships three collector backends, selected via `VmConfig::gc_algorithm`
([`vm/src/config.rs`](../vm/src/config.rs)):

> **The default collector changed on 2026-08-10: it is now ZGC, not
> Generational.** If you pinned nothing, your runs moved. Two things follow.
>
> **(1) Budget more heap — RETIRED as a standing rule, 2026-08-13.** This
> paragraph used to say ZGC needs roughly **1.5x** the heap the generational
> collector needed, on the strength of one class: `ZipContentTests` OOMed under
> ZGC at `-Xmx 2g` and passed from 3g up, where Generational passed at 2g.
>
> **That class now passes 29/29 at `-Xmx 2g` under ZGC**, and 29/29 under
> Generational on the same heap
> (`fixed-suite-bugs/vm/zgc-oom-with-84-percent-of-the-heap-free-FIXED-20260810.md`).
> Two defects were under it, both specific to a non-compacting collector and
> both fixed: the GC trigger asked about live bytes when the binding constraint
> is allocatable space, and the arena's bump cursor was a one-way ratchet.
> **The sole measurement behind the 1.5x figure is gone, so the figure is
> withdrawn rather than restated.**
>
> The same happened to the other two instances of the shape. Hibernate's
> `DFAState[8192]` OOM (2026-08-11) was an allocator sizing bug; Tomcat's 2 MB
> `char[]` OOM (2026-08-13) was mostly a TLAB *reservation* bug —
> 4,000 threads x 512 KiB claiming the whole heap behind a trigger that counts
> only object bytes. **All three known instances were allocator defects, not
> the price of not compacting.**
>
> What still holds, and is the honest general statement: **a non-compacting
> collector needs more headroom than a compacting one, and how much is a
> property of the workload's allocation shapes rather than a constant.** If
> something throws `OutOfMemoryError` under ZGC, raise `-Xmx` to get moving —
> but file it, because every instance so far has had a fixable cause.
> **(2) The escape hatch is `-XX:+UseGenerationalGC`**, available in every
> build including `--no-default-features`. `-XX:-UseZGC` does the same thing.
>
> Why the flip, as measured on 2026-08-10: on the 651-class Tomcat suite, one
> commit, all three backends — ZGC 604 PASS / 29 HANG / 0 CRASH in 247 min
> against Generational's 519 / 115 / 1 in 356 min. 63 classes are non-PASS
> under Generational while passing under *both* other backends, and 62 of those
> log `[moving-young] fallback`.
>
> **That margin did not survive the next day, and this is the current number.**
> The same three arms re-run on 2026-08-11 give ZGC **629 PASS / 11 HANG / 0
> CRASH in 178.4 min** against Generational's **628 / 11 / 0 in 177.5 min** —
> a one-class lead, not an 85-class one. ZGC is still the top row and still the
> only backend that has never crashed here, but size up your heap on the
> paragraph above, not on a pass-rate gap that has closed. The cross-suite
> picture is
> [the Phase 1 baseline](feature-designs/zgc-phase1-empirical-baseline-20260813.md).
> See
> [`docs/known-issues/tomcat/gc-backend-3way-fullsuite-comparison-20260810.md`](known-issues/tomcat/gc-backend-3way-fullsuite-comparison-20260810.md).

> **One ZGC default changed for the gauntlet, not two.** The small-object end
> of the arena is now **compacted** at the end of a collection;
> `CRATONVM_ZGC_RELOCATE=0` restores the non-moving sweep byte for byte, so it
> is a re-run rather than a rebuild and therefore usable as a bisect.
>
> **Parallel marking was flipped on 2026-08-13 and flipped back on 2026-08-14**,
> by the measurement it should have had first: on a 1M-object live set a driven
> mark costs **+31% pause at one worker and +153% at four**, rising with the
> worker count. `CRATONVM_ZGC_PARMARK=<n>` still turns it on for anyone
> measuring the fix; unset means the serial loop, which is the faster one.
>
> **What to watch.** Compaction is the first configuration in which a ZGC cycle
> returns a **non-empty pointer map**, so every consumer of one now runs for
> this collector: JIT frame maps, monitor tables, external root providers,
> native side tables. Those consumers are collector-agnostic and already run
> for the generational moving-young path, but "runs for another collector" is
> not "has run for this one". **A crash, a stale-reference warning or a
> silently-wrong result that disappears under `CRATONVM_ZGC_RELOCATE=0` is that
> change**, and the flag is the bisect.
>
> Turning parallel marking on found two things worth the day it was on. A
> defect that had been invisible while it was opt-in — the parallel path did
> not open a mark cycle, so `visit_refs` traced every weak/soft/phantom
> referent as a strong edge and no reference could be cleared — and then the
> pause measurement above, which is why it is off again.

### What is actually shipping, in one place

Everything in this block was re-derived from the tree on 2026-08-13. If another
page contradicts it, that page is stale — several were, and the reason this
block exists is that a default collector documented as an opt-in experiment
leaves an operator unable to tell what they are running.

| Question | Answer | Where it is decided |
|---|---|---|
| Which collector runs if I set nothing? | **ZGC**, since 2026-08-10 | `VmConfig::default`, `vm/src/config.rs` |
| Is the `zgc` Cargo feature on? | **Yes, by default** — it gates the `GcAlgorithm::Zgc` variant, so the default could not be `Zgc` without it | `gc/Cargo.toml`, `vm/Cargo.toml`, `vm-cli/Cargo.toml` (`^default = `) |
| How do I get a build with no ZGC? | `--no-default-features` (name `mimalloc` back if you still want it). Generational becomes the default there and `-XX:+UseZGC` warns and falls back | `vm-cli/Cargo.toml` |
| How do I switch collector at runtime? | `-XX:+UseGenerationalGC` (or `-XX:-UseZGC`); `-XX:+UseG1GC` for G1. Available in every build | `parse_gc_algorithm`, `vm/src/config.rs` |
| Does ZGC move objects? | **No.** Non-moving, non-generational, whole-heap stop-the-world mark-sweep over one arena | `ZgcRealHeap::collect_garbage`, `gc/src/zgc.rs` |
| Does ZGC have TLABs? | **Yes, default-on.** Not through `VmHeap::refill_tlab` (which returns `None` here) but inside the backend. Kill switch `CRATONVM_ZGC_TLAB=0` or `CRATONVM_GC=-zgc-tlab` | `ZgcRealHeap::alloc_raw_tlab`, `gc/src/zgc/tlab.rs` |
| Is it concurrent, generational or compacting? | **Compacting: YES, on by default since 2026-08-13** (`CRATONVM_ZGC_RELOCATE=0` is the kill switch). **Marking is DRIVABLE but serial by default** — the `zgc_concurrent` driver runs the cycle when `CRATONVM_ZGC_PARMARK=<n>` asks, but adding workers currently makes the pause worse, so unset means the serial loop. **Concurrent: NO — the mutators are still stopped.** **Generational: NO — not built**; page ages, a card barrier and a young scope are computed, but there is no young-only collection | [the concurrent+generational plan](feature-designs/zgc-concurrent-and-generational-plan-20260813.md) |
| What does it cost me? | Headroom. Not compacting means free memory can be plentiful and still too broken up to serve one large array | the sizing notes below |

| Backend | Module | Status | Best for |
|---|---|---|---|
| **ZGC** (default since 2026-08-10) | [`gc/src/zgc.rs`](../gc/src/zgc.rs) | Default, and still **not a real ZGC** | Most workloads, on the suite evidence above. A stop-the-world, non-moving, non-generational whole-heap mark-sweep. Fewest hangs and zero crashes across the Tomcat suite. The "costs ~1.5x heap" rule was withdrawn on 2026-08-13 — its one supporting class now passes at the same heap Generational does. See [the maturity assessment](feature-designs/zgc-maturity-assessment-and-plan-20260813.md) for what is and is not built, and the plan to close it. |
| **Generational** | [`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs) | Production; the escape hatch (`-XX:+UseGenerationalGC`) | Tight heap budgets, and anything that regressed on the flip. Young copying + old free-list + write barriers + card table. Carries the `[moving-young] fallback` throughput problem the flip exists to escape. |
| **G1** (Garbage-First) | [`gc/src/g1.rs`](../gc/src/g1.rs) | Production | Throughput-oriented workloads on larger heaps. Region-based, mixed young/old collections, optional concurrent marking. STW today; parallel evacuator deferred. |

Trade-offs at a glance:

- **Generational** has the simplest configuration surface and the tightest
  minor-GC pause envelope. The default young from-space is 64 MiB
  (`DEFAULT_YOUNG_SEMI_SIZE` in [`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs))
  which keeps copy latency in single-digit ms for typical workloads.
- **G1** breaks the heap into 1 MiB regions and selects collection sets
  per pause target. It pays a per-store remembered-set cost (~10 ns) but
  amortises full-heap compaction. Use it when the old generation is large
  and reclamation latency matters more than minor-GC throughput.
- **ZGC** is **not** stub-only. `ZgcRealHeap` (`gc/src/zgc.rs`) is a real
  memory-backed collector — `Arena` storage, real `ObjectHeader`s, real
  reference processing — and `-XX:+UseZGC` really selects it
  (`GcAlgorithm::Zgc` → `GcBackend::Zgc` → `VmHeap::Zgc`). What it is *not* is
  ZGC: it is stop-the-world, non-moving, whole-heap and non-generational. (It
  *does* have TLABs — thread-private chunks carved from the arena, default-on,
  kill switch `CRATONVM_ZGC_TLAB=0` or `CRATONVM_GC=-zgc-tlab`. The claim that
  "every allocation takes the arena lock" was true before the chunked TLAB
  landed and is not true now. The chunk is **not** a fixed 512 KiB: since
  2026-08-13 it is a share of the heap divided by the live buffer count, capped
  at 512 KiB, because a fixed chunk times a large thread count is the whole
  heap.) The colored-pointer /
  `ZPage` code above it in the same file is a metadata-only simulation with no
  production consumer. On the 1975-class Spring Boot suite:
  1860 PASS vs. Generational's 1902, with 49 HANG vs. 18 — see
  `fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260808.md`.
  The path to a real one is
  [`docs/feature-designs/zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md).
  **Re-attributed 2026-08-13, and this is the part to read before sizing a
  heap:** the Tomcat instance of the "ZGC wants more heap" shape
  (`TestNonBlockingAPI`, `OutOfMemoryError` with 1.99 GB of a 2.15 GB heap
  free) turned out to be mostly *not* the price of not compacting. A TLAB chunk
  is RESERVED space no collection can reclaim while its thread lives, its size
  was flat at 512 KiB however many threads a workload ran, and that class runs
  ~4,000 of them — `4,000 x 512 KiB` is the whole heap. The chunk is now sized
  against the live thread count. The headroom premium is real, but it is not a
  single constant, and the 1.5x figure was WITHDRAWN on 2026-08-13 when its
  one supporting class turned out to pass at 2g under ZGC after two allocator
  fixes; see
  fixed-suite-bugs/tomcat/zgc-nonblockingapi-fragmentation-oom-double-fault-hang-FIXED-20260813.md
  and
  [the maturity assessment](feature-designs/zgc-maturity-assessment-and-plan-20260813.md).

  **Those Spring Boot numbers are superseded, not merely stale.** They are the
  2026-08-08 pre-fix run. Two ZGC-only defects behind them were fixed on
  2026-08-10, and the same day the 26 classes that were the *entire*
  ZGC-vs-default delta were re-run on one binary at `-Xmx 2g`: **ZGC 16 PASS /
  7 HANG / 3 FAIL against the default collector's 14 / 10 / 2**, with the
  record concluding "no functional ZGC-vs-default difference is left". Quote
  those figures, not 1860-vs-1902.
- **ZGC's arena has two ends, and the split is operator-visible.** Small
  objects and TLAB chunks bump upward from the bottom; anything too big for a
  TLAB to serve (>= 64 KiB, i.e. `ZGC_TLAB_MAX_CHUNK / 8`) bumps *downward*
  from the top, with its own free list and a floor of `capacity / 8` reserved
  for it. The reason is that this collector does not compact, so the largest
  request it can serve is the largest gap between two survivors — and one
  long-lived object inside a thread's private chunk caps every hole in
  the heap at one chunk. Measured before the split, on Tomcat's
  `TestNonBlockingAPI`: a 2 MB `char[]` raised `OutOfMemoryError` with 1.99 GB
  of a 2 GB heap free, held out by **544 live bytes in four AQS nodes**. The
  practical consequence for sizing is that a workload dominated by large
  buffers is served from a region bounded below by `-Xmx / 8`, and one that
  allocates no large objects at all gives that region up again (the floor is a
  preference the small-object end may overrun rather than fail).
- The `zgc` feature is **on by default** as of 2026-08-10, because the default
  `GcAlgorithm` is `Zgc` and that variant is `#[cfg(feature = "zgc")]`. A
  `--no-default-features` build has no ZGC at all and falls back to
  Generational; `-XX:+UseZGC` there warns and falls back too.

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
