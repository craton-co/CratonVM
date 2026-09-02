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

> **ZGC is the default collector, not Generational.** If you pin nothing,
> this is what you run. Two things follow.
>
> **(1) Budget more heap.** A non-compacting collector needs more headroom
> than a compacting one, and how much is a property of the workload's
> allocation shapes rather than a fixed multiple — every OOM shape found so
> far under ZGC has turned out to be a fixable allocator defect (a trigger
> asking about live bytes when the binding constraint was allocatable space,
> a TLAB reservation sized flat regardless of thread count) rather than an
> inherent cost of non-compaction. If something throws `OutOfMemoryError`
> under ZGC, raise `-Xmx` to get moving — but file it.
> **(2) The escape hatch is `-XX:+UseGenerationalGC`**, available in every
> build including `--no-default-features`. `-XX:-UseZGC` does the same thing.
>
> ZGC was promoted on measured suite behaviour: across every suite with a
> per-collector sweep, it is at parity with or ahead of Generational on PASS
> count, ties or leads on HANG, and is the only backend that has never
> crashed. Read that as "roughly comparable, slightly ahead," not as a wide
> margin — the gap narrows or closes depending on which suite and run you
> look at.

> The small-object end of the arena is **compacted** at the end of a
> collection by default; `CRATONVM_ZGC_RELOCATE=0` restores the non-moving
> sweep byte for byte, which makes it a usable bisect for anything that looks
> like a stale-reference or relocation defect.
>
> **Parallel marking is off by default.** Driving the mark phase with a
> worker pool costs pause time rather than saving it — on a 1M-object live
> set it measured **+31% pause at one worker and +153% at four**, rising with
> the worker count, because the serial loop is the faster one on this
> workload shape. `CRATONVM_ZGC_PARMARK=<n>` turns it on if you want to
> measure your own workload against it.
>
> **What to watch.** Compaction is the configuration in which a ZGC cycle
> returns a **non-empty pointer map**, so every consumer of one runs for this
> collector: JIT frame maps, monitor tables, external root providers, native
> side tables. Those consumers are collector-agnostic and already run for the
> generational moving-young path, but "runs for another collector" is not
> "has run for this one". **A crash, a stale-reference warning or a
> silently-wrong result that disappears under `CRATONVM_ZGC_RELOCATE=0` is
> that class of bug**, and the flag is the bisect.

### What is actually shipping, in one place

If another page contradicts this block, that page is stale — the reason this
block exists is that a default collector documented as an opt-in experiment
leaves an operator unable to tell what they are running.

| Question | Answer | Where it is decided |
|---|---|---|
| Which collector runs if I set nothing? | **ZGC** | `VmConfig::default`, `vm/src/config.rs` |
| Is the `zgc` Cargo feature on? | **Yes, by default** — it gates the `GcAlgorithm::Zgc` variant, so the default could not be `Zgc` without it | `gc/Cargo.toml`, `vm/Cargo.toml`, `vm-cli/Cargo.toml` (`^default = `) |
| How do I get a build with no ZGC? | `--no-default-features` (name `mimalloc` back if you still want it). Generational becomes the default there and `-XX:+UseZGC` warns and falls back | `vm-cli/Cargo.toml` |
| How do I switch collector at runtime? | `-XX:+UseGenerationalGC` (or `-XX:-UseZGC`); `-XX:+UseG1GC` for G1. Available in every build | `parse_gc_algorithm`, `vm/src/config.rs` |
| Does ZGC move objects? | **No.** Non-moving, non-generational, whole-heap stop-the-world mark-sweep over one arena | `ZgcRealHeap::collect_garbage`, `gc/src/zgc.rs` |
| Does ZGC have TLABs? | **Yes, default-on, and since 2026-09-02 through both doors.** `VmHeap::refill_tlab` hands the VM thread's own buffer a chunk, so the interpreter's `new` and the JIT's inline bump both hit (kill switch `CRATONVM_ZGC_JIT_TLAB=0`); the backend keeps its own buffer under that for the helper and native paths (`CRATONVM_ZGC_TLAB=0`, which switches both off — they carve from one source). Engagement: `vm_tlab_refills` on the `[GC] zgc-features:` line | `gc/src/zgc/vm_tlab.rs`, `ZgcRealHeap::alloc_raw_tlab` |
| Is it concurrent, generational or compacting? | **Concurrent: OPT-IN (`CRATONVM_ZGC_CONC_START=60`).** The strong closure is traced by a worker pool while every mutator runs; the pause replays the SATB ingress, re-scans the roots and sweeps. Measured per-cycle pause **-38% to -58%**, wall clock **+37% to +55%**, and roughly **twice as many cycles** (floating garbage) -- so it is off unless asked for. **Compacting: YES, on by default** (`CRATONVM_ZGC_RELOCATE=0` is the kill switch). **Generational: OPT-IN (`CRATONVM_ZGC_GENERATIONAL=1`).** Every collection between two whole-heap ones is a young cycle: the old generation is pre-marked and never traced, and the remembered set supplies the old-to-young roots. The split is by **object** age, not page age | [the concurrent+generational plan](feature-designs/zgc-concurrent-and-generational-plan-20260813.md) |
| How do I tell whether a collection marked concurrently? | `--verbose:gc`'s **`mark=`** field on each `[GC] zgc-real:` line — `concurrent`, `stw-parallel` or `stw-serial`. At shutdown, `[GC] zgc-concurrent:` gives `cycles_started` / `cycles_completed` / `black_allocations` / `satb_replayed` / `concurrent_phase_ms`. **`cycles_started=0` means the run says nothing about concurrent marking**, which is a different fact from concurrent marking not helping | `ZgcRealHeap::collect_garbage`, `VmHeap::print_gc_summary` |
| When does a concurrent cycle open? | When allocation crosses `CRATONVM_ZGC_CONC_START`% of the collection threshold (default `0`, i.e. never). **A `System.gc()`-driven workload never opens one** — a forced collection is meant to collect now, not to start marking — so a benchmark built out of `System.gc()` calls measures the stop-the-world path however this is configured | `ZgcRealHeap::should_start_concurrent_mark` |
| Do young cycles actually happen? | `CRATONVM_ZGC_GEN_NURSERY_PERCENT` (default 10, `0` disables it) adds a nursery-size clause to `needs_gc`, so a young cycle can fire on its own rather than only when a whole-heap collection would have. `[GC] zgc-nursery:`'s trigger count is the engagement counter: **zero on a generational run means every collection still came from the whole-heap predicate**. The clause bypasses `gc_rearm` deliberately (that floor is a quarter of remaining headroom and would make it unreachable); it cannot storm because the watermark resets on every collection | `ZgcRealHeap::needs_gc` |
| Is the young cycle's SWEEP actually bounded? | `--verbose:gc`'s `gen=young/N **swept=A/B**` field: `A` is the registered objects the sweep walked, `B` the whole registry. `A == B` means the nursery floor never moved and the sweep is O(registry) — the state the first Phase G measurement was in. At shutdown, `[GC] zgc-nursery:` gives `sweep_skipped` / `floor` / `old_live_bytes`; **`sweep_skipped=0` with `young_cycles>0` is the inert state**. The floor is the arena cursor the last WHOLE-HEAP collection ended on, so a young cycle sweeps only what the bump cursor served since then — an object the free list placed below the floor waits for a major (over-retention, never unsoundness) | `ZgcRealHeap::gen_young_floor` |
| How do I tell whether a collection was a young one? | `--verbose:gc`'s **`gen=`** field on each `[GC] zgc-real:` line: `young/N` (a minor, where `N` is the objects it retained **without tracing** — the work it did not do), `major/+N` (whole-heap, `N` objects promoted) or `off`. At shutdown, `[GC] zgc-generational:` gives `young_cycles` / `old_retained` / `remembered_roots` / `promotions` / `recards_after_relocation`. **`young_cycles>0` with `old_retained=0` means the run did full-heap work under a generational name** — which is the vacuous green a "generational is on" claim would otherwise rest on | `ZgcRealHeap::collect_garbage`, `VmHeap::print_gc_summary` |
| When is a collection forced to be whole-heap? | Four cases, and each is a refusal rather than a policy: the mark set came from a **concurrent** cycle (it *is* the whole-heap closure and cannot be scoped after the fact); the collection was driven by **allocation failure** (`headroom_low` / `hard_alloc_failure` — a young cycle retains the whole old generation unexamined, so it is the wrong tool for "the heap is full"); `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` young cycles have run since the last one (default 8); or **nothing has been promoted yet**, in which case a young cycle would be a full one anyway. On a heap too small for `zgc_headroom_margin` the second case fires every cycle and generational never engages — check `young_cycles` before concluding anything | `ZgcRealHeap::collect_garbage` |
| What does a young sweep do per dead object? | It zeroes the 16-byte header only (`CRATONVM_ZGC_GEN_HEADER_ZERO=0` restores a full-body memset, which is redundant with the memset `alloc_raw` already does on every handout) and hands over one free-list span per **run** of adjacent dead objects rather than one span per object (`CRATONVM_ZGC_GEN_DEAD_RUNS=0` restores the per-object calls, each of which then needs sorting in `coalesce_free_list`). Both apply to young cycles only, so a whole-heap sweep is byte-for-byte unchanged. Engagement on `[GC] zgc-sweep-cost:`: **`zero_bytes_skipped=0` with `young_cycles>0` means the first is on and inert, and `dead_runs == dead_objects` means the second is** | `ZgcRealHeap::collect_garbage`, `zgc_gen_header_zero` |
| Is it safe to stop zeroing a dead object's body? | Yes, and the reason is that the header is the whole of the property. The sweep zeroed to stop "a later scan seeing a stale header"; `HEADER_SIZE` **is** the entire `ObjectHeader` (`class_id`, `shape`, `mark_word`) and `ARRAY_DATA_OFFSET == HEADER_SIZE`, so an array's length is in `shape` rather than a body prefix -- a zeroed header is the same `class_id=0, num_slots=0` corpse a reader of a vacated span always saw. The body is only reachable **through** that header: field reads size the object from `num_slots`, extent walks from `alloc_size(header)`, membership from the registry the sweep just removed the base from. And reuse was never the reason -- `alloc_raw` and `tlab_refill` memset unconditionally | `ObjectHeader`, `ZgcRealHeap::alloc_raw` |
| Should I turn generational on? | **Probably not yet, and there is a measurement rather than a guess behind that.** On the shape it is for — 800k retained objects, 48M allocated, an old-to-young store per round — it is **neutral at the default promotion age (+2.5% to +4% total pause) and clearly worse at age 1 (+46% to +55%, reclaim 75.7% → 63.7%)**, with wall clock flat. The split works (4.8M objects skipped per young cycle, every old-to-young edge intact) and does not pay, because `sweep` is 182 ms of a 309 ms pause and walks every registered object whatever the split says. A real young space, reclaimed by resetting a cursor, is what would change that. If you try it anyway: `CRATONVM_ZGC_GEN_PROMOTION_AGE` (default 3, clamped to `1..=15` because the header field is 4 bits) decides how many collections an object must survive; a lower value promotes sooner, so young cycles skip more and old garbage accumulates faster. Measure with `gen=` and `old_retained`, and note that **a lower promotion age is not a stronger version of the same knob** — age 1 promoted 11.7M objects, filling old with garbage no young cycle examines | the plan's §3 |
| Should I turn concurrent marking on? | If your workload has **many mutator threads, a large live set, and a pause budget you are missing**. It trades throughput for pause and the trade is not small; measure your own workload with `--verbose:gc` and compare `mark=concurrent` cycles against `CRATONVM_ZGC_CONC_START=0`. `CRATONVM_ZGC_CONC_WORKERS` (default `cores/4`, capped at 4) is the second knob — 1 worker measured best on a single-threaded probe, 2 on an 8-thread one | the plan's §2b |
| What does it cost me? | Headroom. Not compacting means free memory can be plentiful and still too broken up to serve one large array | the sizing notes below |

| Backend | Module | Status | Best for |
|---|---|---|---|
| **ZGC** (default) | [`gc/src/zgc.rs`](../gc/src/zgc.rs) | Default, and still **not a real ZGC** | Most workloads, on the suite evidence above. Compacting by default, concurrent marking and generational mode opt-in, one arena. Still not OpenJDK ZGC: it marks under a **pre-write** barrier (snapshot-at-the-beginning), not under ZGC's load barrier, so it is conservative about objects that die mid-cycle, and relocation is stop-the-world. Fewest hangs and zero crashes across the Tomcat suite. See [the maturity assessment](feature-designs/zgc-maturity-assessment-and-plan-20260813.md) for what is and is not built, and the plan to close it. |
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
  ZGC: it is stop-the-world, whole-heap by default and non-generational unless
  opted in. It *does* have TLABs — thread-private chunks carved from the
  arena, default-on, kill switch `CRATONVM_ZGC_TLAB=0` or
  `CRATONVM_GC=-zgc-tlab`. The chunk is **not** a fixed size: it is a share of
  the heap divided by the live buffer count, capped at 512 KiB, because a
  fixed chunk times a large thread count would be the whole heap. The
  colored-pointer / `ZPage` code above it in the same file is a metadata-only
  simulation with no production consumer. Across every suite with a
  per-collector sweep, ZGC's PASS/HANG/CRASH counts sit at parity with or
  ahead of Generational's — see the "What is actually shipping" table above
  for the honest current comparison rather than any one suite's headline
  count. The path to a real ZGC is
  [`docs/feature-designs/zgc-production-implementation-plan.md`](feature-designs/zgc-production-implementation-plan.md);
  see [the maturity assessment](feature-designs/zgc-maturity-assessment-and-plan-20260813.md)
  for what is and is not built.
- **ZGC's arena has two ends, and the split is operator-visible.** Small
  objects and TLAB chunks bump upward from the bottom; anything too big for a
  TLAB to serve (>= 64 KiB, i.e. `ZGC_TLAB_MAX_CHUNK / 8`) bumps *downward*
  from the top, with its own free list and a floor of `capacity / 8` reserved
  for it. The reason is that this collector does not compact by default, so
  the largest request it can serve is the largest gap between two survivors —
  and one long-lived object inside a thread's private chunk caps every hole in
  the heap at one chunk. The practical consequence for sizing is that a
  workload dominated by large buffers is served from a region bounded below by
  `-Xmx / 8`, and one that allocates no large objects at all gives that region
  up again (the floor is a preference the small-object end may overrun rather
  than fail).
- The `zgc` feature is **on by default**, because the default `GcAlgorithm` is
  `Zgc` and that variant is `#[cfg(feature = "zgc")]`. A
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
