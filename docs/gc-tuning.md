# Garbage Collector Tuning Guide

Audience: developers tuning CratonVM heap behaviour for a specific workload
— Quarkus boot, JFR-recorded benchmarks, low-latency request handlers,
long-running daemons, or test fixtures with tight budget envelopes.

Source: [`gc/src/`](../gc/src/) — **60 files** (43 top-level plus 17 under
`gc/src/zgc/`), **~211 k LOC** as of 2026-09-21. Re-derive rather than trusting
it: `find gc/src -name '*.rs' | wc -l` and `… | xargs cat | wc -l`. This line
read "34 files, ~62 k LOC" until 2026-09-20 — off by more than a factor of
three — and two concurrent review rounds then corrected it to two *different*
wrong numbers in the same week, which is why the command is written out here
rather than the figure alone.
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
> **(1) Budget some extra heap, but not for the reason this block used to
> give.** Until 2026-09-21 it opened *"A non-compacting collector needs more
> headroom than a compacting one"* — which contradicted the very next block on
> this page, and the FAQ row below it, both of which say compaction has been
> **on by default since 2026-08-13**. ZGC here compacts. What is true is
> narrower: **a cycle may decline to compact** (the cost gate, the per-cycle
> JIT coverage proof, or `CRATONVM_ZGC_RELOCATE=0`), and a run of declined
> cycles fragments like a non-moving collector until one is admitted. How much
> headroom that wants is a property of the workload's allocation shapes rather
> than a fixed multiple, and **every OOM shape found so far under ZGC has
> turned out to be a fixable allocator defect** — a trigger asking about live
> bytes when the binding constraint was allocatable space; a TLAB reservation
> sized flat regardless of thread count — rather than an inherent cost. If
> something throws `OutOfMemoryError` under ZGC, raise `-Xmx` to get moving,
> check `moved=` on `[GC] zgc-relocate:` to see whether the cycles were
> declining — and file it.
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
> collection by default; `CRATONVM_GC=-zgc-relocate` restores the non-moving
> sweep byte for byte, which makes it a usable bisect for anything that looks
> like a stale-reference or relocation defect.
>
> **Parallel marking is off by default.** Driving the mark phase with a
> worker pool costs pause time rather than saving it — on a 1M-object live
> set it measured **+31% pause at one worker and +153% at four**, rising with
> the worker count, because the serial loop is the faster one on this
> workload shape. `CRATONVM_GC=zgc-parmark=<n>` turns it on if you want to
> measure your own workload against it.
>
> **What to watch.** Compaction is the configuration in which a ZGC cycle
> returns a **non-empty pointer map**, so every consumer of one runs for this
> collector: JIT frame maps, monitor tables, external root providers, native
> side tables. Those consumers are collector-agnostic and already run for the
> generational moving-young path, but "runs for another collector" is not
> "has run for this one". **A crash, a stale-reference warning or a
> silently-wrong result that disappears under `CRATONVM_GC=-zgc-relocate` is
> that class of bug**, and the flag is the bisect.

### Is it a relocation defect? The Generational bisect

The Generational backend's equivalent of `CRATONVM_GC=-zgc-relocate` is
**`CRATONVM_GC=-moving-young`** (alias `CRATONVM_NO_MOVING_YOUNG=1`). It turns
the young generation back into an in-place mark-sweep, and because the old-gen
mark-compact (`gen_heap::major_gc`) has exactly one caller and it is on the
moving young path, it stops the old generation compacting too. Nothing in the
heap relocates. A crash, a stale-reference warning or a silently-wrong result
that disappears under it is a relocation defect; one that survives is not.

> **`-moving-young` is NOT a clean no-relocation lever on ZGC, and the name does
> not say so.** ZGC reads the same `gc_quiescence::moving_young_enabled()` as
> one term of its relocation refusal — deliberately, because the whole
> rewritable-root contract its refusal chain leans on is maintained only while
> moving-young is on. But that term, like three of its five neighbours, is
> applied *only on a cycle with a live compiled frame*. So with
> `CRATONVM_NO_MOVING_YOUNG=1` a ZGC run **still compacts on every JIT-cold
> cycle** and refuses on every JIT-warm one, which is not "nothing in the heap
> relocates" and is not a bisect. Measured 2026-09-21: `--nojit` plus
> `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` gives `compaction_cycles=1` with
> the term firing and stopping nothing.
>
> On ZGC use **`CRATONVM_GC=-zgc-relocate`** (alias `CRATONVM_ZGC_RELOCATE=0`),
> which has no such gate. `[GC] zgc-relocation-refusal-term:` counts the
> verdicts and `[GC] zgc-relocation-skip-reason:` counts the ones that stopped a
> slide, so the gap between them is the population this warning is about. See
> the "Which collector a token binds" section of `docs/flag-tokens.md`.

```
CRATONVM_NO_MOVING_YOUNG=1 cratonvm -XX:+UseGenerationalGC -Xmx256m \
    --verbose:gc -cp <classes> <Main>
```

**Confirm the lever engaged before you believe either answer.** Every line must
read `kind=non-moving`, and the shutdown census must read
`[GC] decision histogram: moving=0 non_moving=N skipped=0`. (`skipped=` is the
count of cycles that refused to run at all — neither collector — and it is a
third term, not a slice of `non_moving`; a non-zero one is a separate finding,
not a diversion.) Do **not** look for
`[GC] moving_young:` — that line is printed only when moving-young is enabled
(`VmHeap::print_gc_summary`, `gc/src/vm_heap.rs:4202`), so it is absent by design on exactly this run.

> **If you ran this bisect before 2026-09-21, re-run it.** The flag did not work
> on a process with no live compiled frame: the term that diverts the cycle
> required a live JIT frame *as well as* the gate being off, so a short repro,
> a unit-test-shaped reproduction — anything that had not tiered up — moved on
> every collection with the flag set. Measured at `d6d5cd262`: 1512 of 1512
> cycles `kind=moving`, `reason=moving-no-jit-frames-live`, on a run whose own
> decision record said `moving_young_requested=false`. **Any "not a relocation
> defect" conclusion drawn from that lever on a JIT-cold workload is void.**
> (`docs/internal/gaps/gengc-probe-no-moving-young-optout-does-not-opt-out-20260921.md`.)

Two things it is *not*. It is not a throughput setting — giving up compaction
in both generations costs fragmentation headroom, which is the whole
`[moving-young] fallback` trade. And it is not the lever for "why is my young
generation not compacting?": that question is answered by the `divert_non_moving`
table in [`docs/GC.md`](GC.md), and on a JIT-warm workload the dominant term is
`unrewritable_conservative_jit_roots`, whose only lever is
`CRATONVM_GC=-peer-pin-divert`.

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
| Does ZGC move objects? | **Yes, by default — and each cycle decides again.** After the sweep, a stop-the-world slide compacts the small-object end of the arena and returns a **non-empty** `PointerMap`; `CRATONVM_ZGC_RELOCATE=0` (`off`/`false`/`no`) restores the non-moving behaviour byte for byte, so an A/B is a re-run and not a rebuild. Whole-heap and stop-the-world remain the defaults, and it is non-generational unless `CRATONVM_ZGC_GENERATIONAL=1`. **"Default-on" is not "it happened":** `relocate_stw_admitted` re-decides per collection and may decline — on the per-cycle JIT coverage proof (`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` restores the older blanket refusal: compact only when no compiled frame is live) or on the cost gate, which declines a low slide whose selected garbage is under 1/32 of the low region's live bytes (`CRATONVM_ZGC_RELOCATE_COST_GATE=0`; default-on since round 9 wave 9). Check `moved=` on `[GC] zgc-relocate:` before concluding a cycle compacted; `live=0 moved=0` is a refusal. **This row said "No. Non-moving" for five weeks after the default moved** (default-on 2026-08-13, row corrected 2026-09-20), which is the exact reading that makes a stale-reference crash look impossible | `ZgcRealHeap::relocation_requested_by_default` and `::relocate_stw_admitted`, `gc/src/zgc.rs` |
| Does ZGC have TLABs? | **Yes, both (default-on since 2026-09-18).** The VM-thread buffer (`CRATONVM_ZGC_JIT_TLAB`, `=0` turns it off) makes `VmHeap::refill_tlab` hand the VM thread's own buffer a chunk, so the interpreter's `new` and the JIT's inline bump both hit (bintrees ~6x faster than the helper path; see `ZGC_VM_TLAB_DEFAULT_ON`); the backend keeps its own buffer under that for the helper and native paths (`CRATONVM_ZGC_TLAB=0`, which switches both off — they carve from one source). Engagement: `vm_tlab_refills` on the `[GC] zgc-features:` line. **Chunk sizing counts the VM-thread buffer since 2026-09-20** (`CRATONVM_ZGC_VM_TLAB_SHARE=0` restores the old arithmetic): the one sizing entry point divides a sixteenth of the arena by the buffers claiming one, and that divisor used to count only the backend's own cells — so once the VM buffer became default-on and carried nearly all allocation without registering a cell, **every thread was sized as if it were alone**. That is the Tomcat `TestNonBlockingAPI` defect verbatim: ~4 000 threads × 512 KiB is the whole heap, 1.99 GB of it on the free list in pieces, and a 2 MB `char[]` with nowhere to go. Below ~256 buffers the 512 KiB clamp decides and the divisor is inert, so on a small thread count the switch changes nothing measurable | `gc/src/zgc/vm_tlab.rs`, `ZArenaTlabRegistry::chunk_bytes_now`, `ZgcRealHeap::alloc_raw_tlab` |
| Is it concurrent, generational or compacting? | **Concurrent: OPT-IN (`CRATONVM_ZGC_CONC_START=60`).** The strong closure is traced by a worker pool while every mutator runs; the pause replays the SATB ingress, re-scans the roots and sweeps. Measured per-cycle pause **-38% to -58%**, wall clock **+37% to +55%**, and roughly **twice as many cycles** (floating garbage) -- so it is off unless asked for. **Compacting: YES, on by default** (`CRATONVM_ZGC_RELOCATE=0` is the kill switch). **Generational: OPT-IN (`CRATONVM_ZGC_GENERATIONAL=1`).** Every collection between two whole-heap ones is a young cycle: the old generation is pre-marked and never traced, and the remembered set supplies the old-to-young roots. The split is by **object** age, not page age | [the concurrent+generational plan](feature-designs/zgc-concurrent-and-generational-plan-20260813.md) |
| How do I tell whether a collection marked concurrently? | `--verbose:gc`'s **`mark=`** field on each `[GC] zgc-real:` line — `concurrent`, `stw-parallel` or `stw-serial`. At shutdown, `[GC] zgc-concurrent:` gives `cycles_started` / `cycles_completed` / `black_allocations` / `satb_replayed` / `concurrent_phase_ms`. **`cycles_started=0` means the run says nothing about concurrent marking**, which is a different fact from concurrent marking not helping | `ZgcRealHeap::collect_garbage`, `VmHeap::print_gc_summary` |
| When does a concurrent cycle open? | When allocation crosses `CRATONVM_ZGC_CONC_START`% of the collection threshold (default `0`, i.e. never). `=60` is the value the 2026-08-16 measurement was taken at and the one to opt in with. **`CRATONVM_ZGC_CONC_START=auto`** is a third setting, not a boolean: it seeds the window at 60 and then lets `refresh_adaptive_conc_start` move it from each cycle's own measurements, which is the answer to "a fixed percentage is why concurrent marking loses on *total* pause while winning 38-58% per cycle". **A `System.gc()`-driven workload never opens one** — a forced collection is meant to collect now, not to start marking — so a benchmark built out of `System.gc()` calls measures the stop-the-world path however this is configured | `ZgcRealHeap::should_start_concurrent_mark`, `conc_start_percent_setting` |
| Do young cycles actually happen? | `CRATONVM_ZGC_GEN_NURSERY_PERCENT` (default 10, `0` disables it) adds a nursery-size clause to `needs_gc`, so a young cycle can fire on its own rather than only when a whole-heap collection would have. `[GC] zgc-nursery-trigger:`'s **`fired=`** is the engagement counter — a **separate line** from `[GC] zgc-nursery:`, which carries `sweep_skipped` / `floor` / `old_live_bytes` and no trigger count at all: **zero on a generational run means every collection still came from the whole-heap predicate**. The clause bypasses `gc_rearm` deliberately (that floor is a quarter of remaining headroom and would make it unreachable); it cannot storm because the watermark resets on every collection | `ZgcRealHeap::needs_gc` |
| Is the young cycle's SWEEP actually bounded? | `--verbose:gc`'s `gen=young/N **swept=A/B**` field: `A` is the registered objects the sweep walked, `B` the whole registry. `A == B` means the nursery floor never moved and the sweep is O(registry) — the state the first Phase G measurement was in. At shutdown, `[GC] zgc-nursery:` gives `sweep_skipped` / `floor` / `old_live_bytes`; **`sweep_skipped=0` with `young_cycles>0` is the inert state**. The floor is the arena cursor the last WHOLE-HEAP collection ended on, so a young cycle sweeps only what the bump cursor served since then — an object the free list placed below the floor waits for a major (over-retention, never unsoundness) | `ZgcRealHeap::gen_young_floor` |
| How do I tell whether a collection was a young one? | `--verbose:gc`'s **`gen=`** field on each `[GC] zgc-real:` line: `young/N` (a minor, where `N` is the objects it retained **without tracing** — the work it did not do), `major/+N` (whole-heap, `N` objects promoted) or `off`. At shutdown, `[GC] zgc-generational:` gives `enabled` / `young_cycles` / `minors_since_major` / `old_retained` / `remembered_roots` / `promotions` / `recards_after_relocation`, printed unconditionally so a run that never engaged says so rather than printing nothing. **`young_cycles>0` with `old_retained=0` means the run did full-heap work under a generational name** — which is the vacuous green a "generational is on" claim would otherwise rest on | `ZgcRealHeap::collect_garbage`, `VmHeap::print_gc_summary` |
| When is a collection forced to be whole-heap? | **Six cases**, and each is a refusal rather than a policy: the mark set came from a **concurrent** cycle (it *is* the whole-heap closure and cannot be scoped after the fact); the collection was driven by **hard allocation failure** (`hard_alloc_failure` — a young cycle retains the whole old generation unexamined, so it is the wrong tool for "the heap is full"; **`headroom_low` is deliberately *not* one of these**, and the code says so in as many words); a previous young cycle **reclaimed nothing** (`gen_force_major_next` — an escalation latch, and the reason a generational run can show a major it did not ask for); the `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` ceiling (default 8) has been reached; **nothing has been promoted yet** (`has_old_objects`), in which case a young cycle would be a full one anyway; or the **young page set is incomplete** (`young_pages_are_complete` — an allocation that landed off the page grid, which fails closed, because an unrecorded page is retained until a major and therefore forever if majors are rare). Check `young_cycles` before concluding anything. **This row named four cases and listed `headroom_low` among them until 2026-09-20**, which sent an operator chasing `young_cycles=0` on a small heap to a knob that has nothing to do with it | `ZgcRealHeap::collect_garbage`, the `force_major` / `young_cycle` pair in PHASE G |
| What does a sweep do per dead object? | It zeroes the 16-byte header only (`CRATONVM_ZGC_SWEEP_HEADER_ZERO=0` restores a full-body memset, which is redundant with the memset `alloc_raw` already does on every handout) and hands over one free-list span per **run** of adjacent dead objects rather than one span per object (`CRATONVM_ZGC_SWEEP_DEAD_RUNS=0` restores the per-object calls, each of which then needs sorting in `coalesce_free_list`). **Both apply to every cycle**, and the saving is on the whole-heap one: a young cycle's sweep is 6.3 ms of a 112 ms pause where a whole-heap cycle's is 170 ms of 265 ms, because it walks 13.0M dead objects against a young cycle's 137k. They were scoped to young cycles when they landed on 2026-08-17 — a mid-gauntlet caution that has since expired — and the restriction pointed the change away from the cost. Engagement on `[GC] zgc-sweep-cost:`: **`zero_bytes_skipped=0` with collections having run means the first is on and inert, and `dead_runs == dead_objects` means the second is**. The flags were `CRATONVM_ZGC_GEN_*` before the scoping was removed; **the old names are dead and set silently to nothing** | `ZgcRealHeap::collect_garbage`'s `ZSweepCfg`, `zgc_sweep_header_zero` / `zgc_sweep_dead_runs` |
| Is it safe to stop zeroing a dead object's body? | Yes, and the reason is that the header is the whole of the property. The sweep zeroed to stop "a later scan seeing a stale header"; `HEADER_SIZE` **is** the entire `ObjectHeader` (`class_id`, `shape`, `mark_word`) and `ARRAY_DATA_OFFSET == HEADER_SIZE`, so an array's length is in `shape` rather than a body prefix -- a zeroed header is the same `class_id=0, num_slots=0` corpse a reader of a vacated span always saw. The body is only reachable **through** that header: field reads size the object from `num_slots`, extent walks from `alloc_size(header)`, membership from the registry the sweep just removed the base from. And reuse was never the reason -- `alloc_raw` and `tlab_refill` memset unconditionally | `ObjectHeader`, `ZgcRealHeap::alloc_raw` |
| Should I turn generational on? | **Probably not yet, and there is a measurement rather than a guess behind that.** On the shape it is for — 800k retained objects, 48M allocated, an old-to-young store per round — it is **neutral at the default promotion age (+2.5% to +4% total pause) and clearly worse at age 1 (+46% to +55%, reclaim 75.7% → 63.7%)**, with wall clock flat. The split works (4.8M objects skipped per young cycle, every old-to-young edge intact) and does not pay, because `sweep` is 182 ms of a 309 ms pause and walks every registered object whatever the split says. A real young space, reclaimed by resetting a cursor, is what would change that. If you try it anyway: `CRATONVM_ZGC_GEN_PROMOTION_AGE` (default 3, clamped to `1..=15` because the header field is 4 bits) decides how many collections an object must survive; a lower value promotes sooner, so young cycles skip more and old garbage accumulates faster. Measure with `gen=` and `old_retained`, and note that **a lower promotion age is not a stronger version of the same knob** — age 1 promoted 11.7M objects, filling old with garbage no young cycle examines | the plan's §3 |
| Should I turn concurrent marking on? | If your workload has **many mutator threads, a large live set, and a pause budget you are missing**. It trades throughput for pause and the trade is not small; measure your own workload with `--verbose:gc` and compare `mark=concurrent` cycles against `CRATONVM_ZGC_CONC_START=0`. `CRATONVM_ZGC_CONC_WORKERS` (default `cores/4`, floor 1, cap 4; an explicit value is clamped to `1..=32`) is the second knob — 1 worker measured best on a single-threaded probe, 2 on an 8-thread one. Deliberately a different knob from the stop-the-world mark pool: that one wants every core because nothing else is running, this one is stealing cycles from the mutators beside it | the plan's §2b |
| Why did a collection run? | `[GC] zgc-trigger:` carries four reason tallies plus `hard_alloc_refusals` — `stress` (`CRATONVM_DBG_GC_STRESS`), `live_bytes_threshold` (75% of `-Xmx`), `headroom_low` (the arena cannot serve a `zgc_headroom_margin` request) and `alloc_budget` (`CRATONVM_ZGC_ALLOC_TRIGGER`, percent of capacity allocated since the last cycle, **default 0 = off** — the implicit floor a pause target used to supply was withdrawn on 2026-09-04, see the flag notes below). They are not exclusive: a cycle can be charged to the threshold *and* to headroom, which is the whole question when two arms differ in collection **count**. **As of 2026-09-20 they count collections; before that they counted agreeing polls of `needs_gc`** — one per native boundary — so any archived comparison of these numbers against a current run is meaningless, not merely rescaled | `ZgcRealHeap::needs_gc`, `ZgcCounters::trigger_seen` |
| Can I measure the reference-slot mix? | `CRATONVM_ZGC_CENSUS=1` turns on the sweep-time walk that answers "what fraction of live reference slots are legacy 16-byte cells?", and `CRATONVM_ZGC_CENSUS_ACCESS=1` adds the per-access arm (and implies the first). The summary prints when the heap is dropped, and only if the census was enabled. **It was unreachable before 2026-09-20** — `ZSlotCensus::enable()` had no caller outside its own tests, so `is_enabled()` was `false` for the life of every process and a 2 900-line instrument had never run. Read `legacy_share` off the **`last_walk`** block, not the cumulative `walk` one: the cumulative figure is lifetime-weighted and reads high on legacy. A `VERDICT=no_data` row is the instrument saying so, and is a different fact from "no legacy slots" | `ZSlotCensus::enable_from_env`, `gc/src/zgc/census.rs` |
| What does it cost me? | Headroom, and less of it than this row used to claim. It said "**not** compacting means free memory can be plentiful and still too broken up to serve one large array" — written when the slide was off. The slide is on, so the steady state is defragmented; what remains is that **a cycle may decline to compact** (the cost gate, the JIT coverage proof, `CRATONVM_ZGC_RELOCATE=0`), and that a declined cycle leaves exactly the fragmentation the sentence describes, with `headroom_low` as the symptom and an aborting `alloc_object` as the endgame. The large-object end is compacted separately (`CRATONVM_ZGC_HIGH_COMPACTION=0` is its kill switch) and reserves `capacity / 8` | the sizing notes below; `[GC] zgc-relocate:`'s `moved=` |

| Backend | Module | Status | Best for |
|---|---|---|---|
| **ZGC** (default) | [`gc/src/zgc.rs`](../gc/src/zgc.rs) | Default, and still **not a real ZGC** | Most workloads, on the suite evidence above. Compacting by default, concurrent marking and generational mode opt-in, one arena. Still not OpenJDK ZGC: it marks under a **pre-write** barrier (snapshot-at-the-beginning), not under ZGC's load barrier, so it is conservative about objects that die mid-cycle, and relocation is stop-the-world. Fewest hangs and zero crashes across the Tomcat suite. See [the maturity assessment](feature-designs/zgc-maturity-assessment-and-plan-20260813.md) for what is and is not built, and the plan to close it. |
| **Generational** | [`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs) | Production; the escape hatch (`-XX:+UseGenerationalGC`) | Tight heap budgets, and anything that regressed on the flip. Young copying + old free-list + write barriers + card table. Carries the `[moving-young] fallback` throughput problem the flip exists to escape — and note that on a JIT-warm workload the young half is in practice **not** a copying collector at all, because a live compiled frame diverts the cycle to the non-moving sweep; see the `divert_non_moving` table in [`docs/GC.md`](GC.md). Both the young mark and the young evacuation are parallel when they do run. |
| **G1** (Garbage-First) | [`gc/src/g1.rs`](../gc/src/g1.rs) | Production, opt-in (`-XX:+UseG1GC`) | Workloads that need COMPACTION more than they need short pauses: a live set small relative to the heap, fragmenting allocation shapes, and a large old generation. Region-based, evacuating, mixed young/old collections, a real SATB concurrent-mark cycle, and a **parallel evacuator that is on by default** (`CRATONVM_G1_PARALLEL_EVAC=0` is the bisect). Evacuation is stop-the-world; marking is not. Its structural limit is that the per-pause reference fix-up walk is O(live heap) rather than O(collection set) — see [`docs/GC.md`](GC.md) for the measured phase split. |

Trade-offs at a glance:

- **Generational** has the simplest configuration surface and the tightest
  minor-GC pause envelope. The default young from-space is 64 MiB
  (`DEFAULT_YOUNG_SEMI_SIZE` in [`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs))
  which keeps copy latency in single-digit ms for typical workloads.
- **G1** breaks the heap into regions and selects collection sets per pause
  target. It pays a per-store remembered-set cost (~10 ns) but amortises
  full-heap compaction. Use it when the old generation is large and
  reclamation latency matters more than minor-GC throughput. The region size
  is an **ergonomic, not a constant**: `g1_ergonomic_region_size`
  (`gc/src/vm_heap.rs`) targets ~2048 regions at any heap size, so it is 1 MiB
  up to a 2 GiB heap and then 2/4/8/16/32 MiB, clamped to
  `[256 KiB, 32 MiB]`. That bound matters because almost every per-pause cost
  in the collector is linear in the REGION COUNT and the remembered set's
  worst case is quadratic in it. `-XX:G1HeapRegionSize=<bytes>` overrides it
  (rounded up to a power of two, then clamped to the same range).
- **ZGC** is **not** stub-only. `ZgcRealHeap` (`gc/src/zgc.rs`) is a real
  memory-backed collector — `Arena` storage, real `ObjectHeader`s, real
  reference processing — and `-XX:+UseZGC` really selects it
  (`GcAlgorithm::Zgc` → `GcBackend::Zgc` → `VmHeap::Zgc`). What it is *not* is
  ZGC: it is stop-the-world, whole-heap by default and non-generational unless
  opted in. It **is** compacting by default — see the row above; "not a real
  ZGC" is a statement about the pause and the barrier, never about the motion.
  It *does* have TLABs — thread-private chunks carved from the
  arena, default-on, kill switch `CRATONVM_ZGC_TLAB=0` or
  `CRATONVM_GC=-zgc-tlab`. The chunk is **not** a fixed size: it is a
  sixteenth of the arena divided by the buffers actually claiming one — the
  VM-thread buffer included, since 2026-09-20 — and clamped up to 512 KiB,
  because a fixed chunk times a large thread count would be the whole heap. The
  colored-pointer / `ZPage` code above it in the same file is **mostly, but no
  longer entirely**, a metadata-only simulation: `forwarding::ZRelocationSet::select`
  is a real production consumer — it ranks the slide's pages by descending
  garbage ratio — while the load-barrier layer is still unreached, no
  `ZGoodMask` is ever constructed by a running heap, and `ZgcRealHeap` stores
  plain machine pointers whose liveness lives in the header's `GC_FLAG_MARKED`
  bit rather than in a pointer's metadata bits. Do not guess at this:
  `scripts/zgc-submodule-adoption.sh` prints the current production reference
  count per submodule and `types/tests/zgc-submodule-adoption.txt` is the
  recorded answer a test holds it to, precisely because a hand-written version
  of this sentence has been wrong twice. Across every suite with a
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
  for it. The reason is the regime the split was designed for and still has to
  survive: **a cycle that does not compact** (the cost gate declines it, the
  JIT coverage proof refuses it, or `CRATONVM_ZGC_RELOCATE=0`). There, the
  largest request the arena can serve is the largest gap between two survivors —
  and one long-lived object inside a thread's private chunk caps every hole in
  the heap at one chunk. The split is what keeps a large array's supply
  independent of that. (This paragraph read "this collector does not compact by
  default" until 2026-09-20, **five weeks** after the default moved — compaction
  has been on by default since **2026-08-13**, `gc/src/zgc.rs:6175`, pinned by
  `compaction_is_on_by_default_and_zero_is_the_kill_switch`. It said "three
  months" until 2026-09-21, which was itself a drifted number in a sentence
  about drift.) The split's *reason* survives the flip, its premise does not. The large end has its own
  compactor, `CRATONVM_ZGC_HIGH_COMPACTION` (default on, `=0` is the kill
  switch), distinct from the low slide's `CRATONVM_ZGC_RELOCATE`. The practical consequence for sizing is that a
  workload dominated by large buffers is served from a region bounded below by
  `-Xmx / 8`, and one that allocates no large objects at all gives that region
  up again (the floor is a preference the small-object end may overrun rather
  than fail).
- The `zgc` feature is **on by default**, because the default `GcAlgorithm` is
  `Zgc` and that variant is `#[cfg(feature = "zgc")]`. A
  `--no-default-features` build has no ZGC at all and falls back to
  Generational; `-XX:+UseZGC` there warns and falls back too.

### ZGC flags added or renamed in the 2026-09-20 round

A flag that exists and is undocumented is the same defect class as a documented
flag that does nothing, and this round produced both. The full generated
inventory is [`docs/config/flag-inventory.md`](config/flag-inventory.md) and the
token spellings are in [`docs/flag-tokens.md`](flag-tokens.md); what follows is
only what moved, with the reason, because a default that changes how often the
default collector runs is not a detail.

| Flag | Default | What it does, and why |
|---|---|---|
| `CRATONVM_ZGC_VM_TLAB_SHARE` | **ON** | Counts the VM-thread buffer against the TLAB reservation share, so a chunk shrinks with the number of threads actually claiming one. `=0` restores the pre-2026-09-20 arithmetic, which counted only the backend's own cells and therefore sized every thread as if it were alone once the VM buffer became default-on. The kill switch exists so the claim is falsifiable on a built binary rather than argued; see the TLAB row above for the Tomcat measurement it re-closes. |
| `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM` | **OFF** | Lets the `headroom_low` clause of `needs_gc` ask for a collection without clearing the `gc_rearm` floor — but only while the last cycle actually reclaimed something. `gc_rearm` is about **live bytes** and `headroom_low` is about **allocatable space**, and on a heap that may decline to compact those two come apart without bound: a 2 GiB heap holding 10 MiB live sets the floor near 507 MiB, the bump cursor then runs to capacity over a fragmented free list, and the clause answers "no" until an allocation actually fails — which for the infallible `alloc_object` is `abort()`, not an `OutOfMemoryError`. Off by default because removing the floor on a heap genuinely full of **live** data is a collection per allocation, which is the storm the floor exists to prevent; the tree has shipped that argument before and reverted it. `docs/internal/zgc-round-20260920/gap-e-headroom-trigger-below-rearm.md` names the measurement that would earn the flip. |
| `CRATONVM_ZGC_CENSUS` | **OFF** | Turns on the sweep-time reference-slot walk. See the census row above. |
| `CRATONVM_ZGC_CENSUS_ACCESS` | **OFF** | Adds the per-access arm, and implies `CRATONVM_ZGC_CENSUS` — asking for the dynamic composition and silently getting nothing because a second variable was unset is the same "enabled and inert" trap the gate exists to make visible. |
| `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE` | **OFF** | Feeds the pause-target controller the pause **including** the relocation slide. `refresh_pause_target_budget` sizes `alloc_trigger_bytes`, which is a clause of `needs_gc` — so it decides how often the collector runs — and it was fed a reading taken before the compaction block, which on `ZgcTlabStress 8 200` was measured at about **twice** the logged pause. On the default configuration the loop was modelling roughly a third of its own cost. Set to `1`, the reading equals the `pause_with_relocate_us` field `[GC] zgc-relocate:` already prints. Staged behind a flag for one wave rather than flipped, because it tightens every budget on every workload. `[GC] zgc-pause:`'s `total_us` is unaffected in both arms; it is documented as the pre-slide figure. |
| `CRATONVM_ZGC_METRICS` | **OFF** | Also print `metrics::ZgcMetrics`'s OpenJDK-shaped per-cycle line and its whole-run summary at heap teardown. **Recording is not behind this flag** — `ZgcMetrics` is fed on every collection now; only the two renderings are opt-in, because the format differs from `[GC] zgc-real:` and every harness in this tree keys on that one. Additive: the existing lines are unchanged. |
| `CRATONVM_ZGC_TRIGGER_SHADOW` | **OFF** | Print one `[GC] zgc-trigger-shadow:` line per collection showing what an allocation-rate-driven trigger *would* have decided — `rate_bps`, `free_bytes`, `largest_bytes`, `cycle_us`, `ttl_ms`, `ratio`, `affordable_span`, and `fired=` naming which legacy `needs_gc` clauses actually asked (or `none` for a `System.gc()` or a hard refusal that bypassed the poll). Pure observation; costs one arena lock per collection. **Read `ratio`'s stability between adjacent cycles, not its value** — a projection that swings by an order of magnitude between neighbours refutes the direction. |
| `CRATONVM_ZGC_MARK_REF_CHUNK` | **4096** | Numeric, not boolean. How many of one object's strong out-edges a mark worker traces before returning to its loop head, where it re-tests the safepoint yield flag — so it is what bounds time-to-yield on a huge reference array instead of leaving it proportional to the array's length. Read once per process and latched (`mark::ref_chunk_budget`, over `mark::Z_MARK_REF_CHUNK`, in `gc/src/zgc/mark.rs`). **Live on every ZGC configuration**, both through `ZgcRealHeap`'s own `ZMarkContext::visit_refs_chunked` and through the `ZHeapMarkBridge` wrapper that forwards it (both in `gc/src/zgc.rs`) — the bridge inherited the unsplit default until 2026-09-21, which made the in-crate test population exercise the unchunked path while the counter read zero. `0` and unparseable values fall back to the default; there is deliberately no "disable" value. Engagement counter: `ref_chunks` on `[GC] zgc-mark:` — **zero means "no object on this workload needed splitting", not "broken"**. |
| `CRATONVM_ZGC_TLAB_RESERVED_BYTES` | **OFF** | Sizes a TLAB chunk against the *bytes already claimed* (`budget - reserved_bytes`) rather than against a *count of claimants* (`budget / live_claimants`). `ZGC_TLAB_RESERVATION_SHARE` is a byte budget — a chunk is memory no collection can reclaim while its owner lives — and a count only bounds the total if every claimant refills at the same instant, which is why the first refill of each of N threads after a collection is sized at the ceiling again. Read once per **heap**, not per process (`ZArenaTlabRegistry::for_capacity`, `gc/src/zgc/arena_tlab.rs`), so two heaps in one process can differ and a test can override it through `ZgcRealHeap::set_tlab_reserved_bytes_sizing`. **The counter is maintained either way** — one relaxed add per chunk — so `tlab_reserved_bytes()` against `tlab_reservation_budget_bytes()` (both on `ZArenaTlabRegistry`) is readable on a default build — as is `tlab_reserved_bytes_peak()`, the **high-water** mark, which is the one that decides the flip: the instantaneous figure reads near zero at teardown on a healthy run whether or not the bound was ever breached. That reading is what `docs/internal/zgc-round-20260920/gap-c-tlab-reservation-is-bounded-by-a-thread-count-not-by-bytes.md` asks for before the default moves. |
| `CRATONVM_ZGC_PAGE_EVAC` | **OFF, and it switches nothing today** | Would evacuate through `zgc::relocate::ZRelocate`'s page-based copy protocol instead of the arena slide. Documented here because it is declared, greppable and in the generated inventory, and an operator who finds it there must not conclude there is a second relocator to try: `impl ZRelocateContext for ZgcRealHeap` exists, so the page relocator is no longer unreachable, but the driver arm is **not wired** and the gate (`zgc_page_evac_enabled`, `gc/src/zgc.rs`) has no consumer. The blocker is that `ZgcRealHeap::alloc_in` has no to-space provably outside the relocation set — `Arena::alloc` serves from the free list and the sweep has just put the relocation set's own holes on it — which is a change to `gc/src/arena.rs`. Setting it to `1` is a no-op, not a risk. |

And one rename, which is the other half of the same defect class:
`CRATONVM_ZGC_GEN_HEADER_ZERO` → **`CRATONVM_ZGC_SWEEP_HEADER_ZERO`** and
`CRATONVM_ZGC_GEN_DEAD_RUNS` → **`CRATONVM_ZGC_SWEEP_DEAD_RUNS`**, when the
optimisations stopped being scoped to young cycles. The `GEN_` names are read by
nothing; setting one is silently a no-op, and this page named them until
2026-09-20.

Two flags that predate the round and are worth knowing because they decide how
often the default collector runs:

- `CRATONVM_ZGC_ALLOC_TRIGGER` — collect once this **percent of capacity** has
  been allocated since the last cycle. **Default 0, i.e. off.** It caps the span
  a pause has to walk at `budget + live` regardless of `-Xmx`, which is the fix
  for "pause work scales with the heap flag rather than with the garbage". A
  pause target used to supply an implicit 25% floor under it; **that floor was
  withdrawn on 2026-09-04, the day it shipped** — `org.h2.test.db.TestLargeBlob`
  went from 0 GC cycles and a PASS to 34 cycles and a SIGSEGV in libc, 3 runs of
  4, on one switch. The floor is a reproducer rather than the defect (without it
  that test never collects at all, so nothing exercised the
  `FileChannelImpl.implWrite` → `DirectByteBuffer` path), but a default that
  turns a passing test into a native crash does not ship while the underlying
  bug is open. Setting the percent explicitly still works and is unaffected.
- `CRATONVM_ZGC_PAUSE_TARGET_MS` — **default 200.** The feedback loop is blind
  to the first cycle, so that one still runs to the occupancy clause's 75%.

### Targeted compaction, and the budget it no longer obeys

`CRATONVM_ZGC_TARGETED_COMPACTION` is **opt-in, default OFF**
(`zgc_page_evac_enabled`'s neighbour `targeted_compaction_enabled` in `gc/src/zgc.rs`; `types/src/flag_groups.rs:2697`). It lets an
allocation failure name the window the next collection should empty, and pages
in that window bypass the profitability ranking. It ships off because on every
workload tried it engages **zero** times — the windows that fail are in the
large-object end, which has no logical pages — and shipping an unengaged
default is how a feature comes to look measured when it is not.

Two things an operator who turns it on needs to know:

- **The key is read by presence, not by value.**
  `targeted_compaction_enabled()` is `runtime_var_os(..).is_some()`, so
  `CRATONVM_ZGC_TARGETED_COMPACTION=0` **enables** it. Disable it by unsetting
  the key, or with `CRATONVM_GC=-targeted-compaction`, which is what that token
  expands to. This shape is shared with `CRATONVM_ZGC_JIT_BLANKET_REFUSAL`,
  `CRATONVM_ZGC_ASSUME_REWRITABLE` and `CRATONVM_ZGC_UNREWRITABLE_PEER_REFUSES`.
- **A targeted page is exempt from the evacuation budget, and that is
  default-on inside the feature.** `ZRelocationPolicy::budget_exempts_targets`
  (`gc/src/zgc/forwarding.rs`, `true` by default) is a struct field with no
  environment key of its own, because turning the feature off already makes it
  inert. It exists because `ZRelocationSet::select`'s budget rule is a *prefix*
  rule — it does not skip an unaffordable page, it **ends the selection** — and
  targeted pages sort first, so a single targeted page larger than
  `max_evacuation_bytes` (64 MiB) used to end the selection on the first
  iteration and the cycle evacuated *nothing at all*: not the target, and not
  the profitable pages that would have been selected had no target been named.
  **How far past the budget a run went is printed**, as `targeted_overrun=` on
  the `[GC] zgc-relocate:` line (cumulative live bytes selected above the
  budget; also `ZRelocationSet::targeted_overrun_bytes`). Zero unless the exemption fired. A persistently large
  value says to bound the targeted window, not to re-impose the budget — the
  window the mechanism was measured on is 34 objects in 49 runs.

> **Registration, 2026-09-21 — the 2026-09-20 caveat here is discharged.** All
> ten flags added by this round *are* now declared in
> `types/src/flag_groups.rs:2910`–`:2918` and `:2926`, carry
> `CRATONVM_GC=<token>` spellings (`docs/flag-tokens.md`) and appear in
> [`docs/config/flag-inventory.md`](config/flag-inventory.md) with the correct
> opt-in/default-on rendering. The gap this block used to name
> (`docs/internal/zgc-round-20260920/gap-g-new-zgc-flags-are-not-in-the-flag-inventory.md`)
> is **closed**, including the residue in which seven default-OFF booleans were
> declared with `off_word: Some("0")` and therefore rendered as *default-on*;
> that was fixed in the same wave 3 commit that recorded it, `9a03106ff`.

## Sizing knobs

### Heap totals (`VmConfig`)

- `max_heap_size` — `-Xmx` equivalent. Default 256 MiB. Sizes the entire
  managed heap; the backend partitions it into young/old (generational) or
  regions (G1).
- `initial_heap_size` — `-Xms` equivalent. Default 16 MiB. The bytes committed
  at startup, honoured on all three backends since 2026-09-21: G1 commits the
  prefix of its reservation, ZGC the prefix of its arena, Generational the
  request across its two young semi-spaces. The DEFAULT is why no diagnostic is
  raised from the heap constructor — 16 MiB arrives there on every run and is
  indistinguishable from an explicit flag, so only the launcher can tell the
  two apart.

### Generational (`gc/src/gen_heap.rs`)

| Constant | Value | Rationale |
|---|---|---|
| `DEFAULT_YOUNG_SEMI_SIZE` | 64 MiB | Bumped from 16 MiB after RealWorldBench GC-stress finding (commit `69fa153`). Smaller young = more frequent minor GCs; larger young = longer copy latency but more time for objects to die. |
| `YOUNG_GC_THRESHOLD_PERCENT` | **50** | Minor GC fires when from-space utilisation crosses this percentage. **Not 75** — this row said 75 until 2026-09-20; `gc/src/gen_heap.rs:174` has read 50 since the moving young gen shipped, because to-space must be able to hold every survivor. Settable at runtime, unlike its neighbours in this table: `CRATONVM_GC=young-trigger-percent=<n>`, clamped to `1..=95`. Raising it collects less often and copies more survivors per cycle; which wins is a property of the workload's survival rate. |
| `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT` | 90 | The **second** young trigger, and this table did not mention it. The non-moving sweep reclaims dead spans in place and needs no Cheney headroom, so a cycle that is going to be non-moving is allowed to fill 90 % of the semi-space first. Not settable. A workload that diverts to the sweep every cycle (see `docs/GC.md`'s `divert_non_moving` table — a live JIT frame does this) is therefore running at the 90 % trigger, not the 50 % one, and its young occupancy curve looks nothing like this table implies. |
| `MAX_HEAP_EXPANSION_FACTOR` | 4 | Young from-space can grow up to 4× initial when minor GC reclaims < 25 %. **No shrink path today** — bursty workloads keep the larger footprint. |
| `PROMOTION_AGE` | 3 | Survive this many minor GCs before being promoted to old gen. Lower (1–2) sends short-lived but slightly-tenured objects to old prematurely; higher (4–6) costs more copy work but keeps the old gen denser. Bump when minor GC pauses are fine but the old gen fills too quickly. |

There is no public setter for the four constants today; embedders who need
non-default sizing should construct `GenerationalHeap::with_capacity` (or
`with_sizes`) directly, then build the `VmHeap` around it. The defaults
are deliberate and cover the vast majority of workloads.

### G1 (`gc/src/g1.rs`)

`G1CollectorConfig` is exposed publicly. Every row below is the value in
`G1CollectorConfig::default()`; the `-XX:` column is the operator spelling
where one exists (`VmHeap::new_with_overrides`).

| Field | Default | `-XX:` | Effect |
|---|---|---|---|
| `heap_size` | 256 MiB | `-Xmx` | Total managed heap. Since F-16 this is the size of the **reservation** — address space — not of the memory charged to the process at startup. |
| `initial_heap_size` | `0` = ergonomic | `-Xms` | Bytes committed up front. The ergonomic is a **sixteenth of the heap, floored at four regions**, so a short-lived process pays almost nothing for a large `-Xmx`. The prefix then grows on demand as regions are claimed; `CRATONVM_G1_RESERVE_HEAP=0` restores the pre-F-16 "commit everything at startup". |
| `region_size` | 1 MiB, **but see the ergonomic** | `-XX:G1HeapRegionSize` | Granularity for evacuation and remembered-set tracking. `VmHeap` overrides the default with `g1_ergonomic_region_size` = `max(heap / 2048, 1 MiB)` clamped to `[256 KiB, 32 MiB]`, so the region COUNT stays near 2048 at any heap size. Raising it reduces remembered-set overhead at the cost of more wasted bytes per part-full region — and raises the humongous threshold, which is `region_size / 2`. |
| `max_gc_pause_ms` | 200 | — | The pause goal. It reaches **two** decisions: how many OLD regions a mixed collection set may take (`old_cset_copy_budget_ns` = the goal minus the rolling fix-up cost), and the adaptive young-region target (`update_young_target`). It does **not** move the marking threshold any more — see `ihop_percent`. |
| `ihop_percent` | **70** (the field doc's "45" is stale) | `-XX:InitiatingHeapOccupancyPercent` | Old-generation occupancy at which a concurrent mark cycle starts. A **ceiling** on the adaptive threshold, never a floor: `CRATONVM_G1_ADAPTIVE_IHOP=0` restores the old pause-time-driven model. Raised from HotSpot's 45 because the default heap is small; on a 16 GiB heap you want it back near 45. |
| `promotion_age` | 15 | — | The tenuring **ceiling**. Since F-18 the effective threshold is re-derived after every pause from an age histogram of surviving bytes and may be lower — never higher — when survivor space would overflow its target (an eighth of the young generation). `CRATONVM_G1_ADAPTIVE_TENURING=0` pins it at the configured value. |
| `gc_worker_threads` | `0` = ergonomic | `-XX:ParallelGCThreads` | **No longer a no-op.** The parallel evacuator is on by default; `0` derives the count from the machine (`ergonomic_gc_worker_threads`, HotSpot's taper), a non-zero value is an explicit request still clamped to the hardware, and `CRATONVM_G1_WORKERS=<n>` overrides both. `=1` drains the parallel path serially, which is the lever that separates a concurrency race from a logic divergence. The generational young collector has its own worker policy (`CRATONVM_GC=par-threads=<n>`) and does not read this field at all. |
| `string_dedup_enabled` | false | `-XX:+UseStringDeduplication` | Off by default. The table is now remapped and purged by every collection path, so a caller is safe to add — but a HIT is a hash match, not an equality proof. |
| `mixed_gc_count_target` | 8 | — | Mixed-collection cycles armed after a mark cycle completes. |
| `old_cset_region_threshold_percent` | 10 | — | Cap on old regions per mixed GC, as a percent of the region count (minimum one region, so a tiny heap still makes progress). |
| `mixed_gc_live_threshold_percent` | 85 | — | HotSpot's `G1MixedGCLiveThresholdPercent`. An Old region at or above this percent live is never a mixed candidate: evacuating it is nearly a full copy for almost no reclaim. |
| `heap_waste_percent` | 5 | — | HotSpot's `G1HeapWastePercent`. The mixed phase ends early once the garbage held by the remaining candidates is below this share of the heap — **unless the Free pool is tight**, in which case the floor is waived (a mixed pause is this collector's only full GC). |

Two sizing policies have no `G1CollectorConfig` field and are worth knowing
because they, not the fields above, decide when most pauses happen:

- **The free-fraction trigger.** `needs_gc` fires when the Free region
  fraction drops below 25 %. That percentage is itself adaptive: it is raised
  after a collection that hit evacuation failure, because the free pool at
  trigger time *is* the young evacuation's to-space.
- **The adaptive young target** (`CRATONVM_G1_YOUNG_PAUSE_TARGET=1`, opt-in).
  Bounds Eden+Survivor between 5 % and 60 % of the region count: tighten 20 %
  after a *productive* pause that overruns the goal, give 12.5 % back while
  pauses stay under half of it, and reset to the ceiling after an unproductive
  pause so it can never storm. It starts at the ceiling and a target at the
  ceiling is deliberately not a trigger, so the flag does nothing until an
  overrun has actually been measured. `[GC] g1 young-sizing:` at exit reports
  where it ended up.

**Heap growth and shrink, stated plainly.** By default G1 grows its committed
prefix on demand up to `-Xmx` and never shrinks it except at the end of a
concurrent-mark cleanup, and only then if you ask (`CRATONVM_G1_UNCOMMIT=1`).
On a heap sized comfortably for its live set IHOP is never crossed, no mark
cycle runs, cleanup never runs — so in practice `-Xmx` is a one-way ratchet on
RSS.

**Adaptive heap sizing (`CRATONVM_G1_HEAP_RESIZE=1`, opt-in).** This is the
resizing policy the paragraph above says does not exist. It maintains a *soft
capacity* between `-Xms` and the `-Xmx` region grid and moves it from two
signals measured at every pause:

- **GC overhead** — pause time as a fraction of wall clock, smoothed 7/8. Above
  **5 %** the soft capacity grows by a fifth; below **1 %** it becomes eligible
  to shrink. The gap between the two thresholds is deliberate: a program
  collecting 3 % of the time is doing its job, not wasting memory.
- **Occupancy** — regions in use as a fraction of the soft capacity. At or
  above **70 %** it grows; below **40 %** it becomes eligible to shrink.

A shrink additionally requires **four consecutive** qualifying pauses, and aims
to leave occupancy at **60 %** — strictly inside the 40–70 % dead band, so it
cannot land the heap where the next pause would grow it straight back. Growth
has no such delay, because holding memory a program does not need is a cost and
collecting its heap twice as often as it needs is a worse one.

What the soft capacity *does*: it is the denominator of the free-fraction
trigger (so lowering it collects sooner and holds less; raising it collects
less often and holds more), and it is the floor for the trailing-Free-run
shrink, which now runs at the **end of every evacuation pause** rather than only
at cleanup. `-Xms` is an absolute floor in both roles. `Runtime.totalMemory()`
reports the soft capacity; `Runtime.maxMemory()` still reports `-Xmx`.

Every resize is logged at INFO as `[GC] g1 heap-resize: grow|shrink …`, appears
on every `[GC-STAT]` line as `heap_target_regions=` / `gc_overhead_ppm=` /
`heap_grows=` / `heap_shrinks=`, and is summarised at exit by
`[GC] g1 heap-resize:`. **Watch `heap_grows` and `heap_shrinks` together**: two
large, nearly equal counts mean the policy is oscillating, which is worse than
never shrinking at all.

**Pause-goal cost model (`CRATONVM_G1_PAUSE_COST_MODEL=1`, opt-in).** By default
`max_gc_pause_ms` bounds only the OLD half of a mixed collection set, and it
prices that half against the fix-up walk the PREVIOUS pause happened to pay.
With this on, the mixed-CSet budget also subtracts the young half's measured
copy cost (every Eden and Survivor region is in the collection set
unconditionally, so that copy is already committed), and each candidate old
region is charged for the additional fix-up walk it causes — its to-space
destination plus the remembered-set sources it pulls into the walk that no
already-selected candidate has paid for. Both terms are subtractions, so this
can only make a mixed collection set smaller; the selector still takes one
region unconditionally for forward progress.

**G1 kill switches you are most likely to want.** The complete list is in
[`docs/GC.md`](GC.md); these are the ones that change a sizing or placement
decision rather than a verification:

| Switch | Effect |
|---|---|
| `CRATONVM_G1_YOUNG_PAUSE_TARGET=1` | Opt in to the adaptive young-region target described above. |
| `CRATONVM_G1_UNCOMMIT=1` | Opt in to returning trailing Free regions to the OS at cleanup. Never below `-Xms`. |
| `CRATONVM_G1_EAGER_HUMONGOUS=0` | Stop freeing unreferenced humongous spans at every pause; wait for a mark cycle instead. |
| `CRATONVM_G1_ADAPTIVE_TENURING=0` | Pin the tenuring threshold at `promotion_age`. |
| `CRATONVM_G1_ADAPTIVE_IHOP=0` | Restore the pause-time-driven marking threshold. |
| `CRATONVM_G1_EDEN_STRIPES=<n>` | Pin the number of Eden stripes mutators allocate through (default: hardware parallelism, capped at an eighth of the region count). `=1` is the single global Eden. |
| `CRATONVM_G1_TLAB_CLAMP=0` | Refuse, rather than clamp, a TLAB request larger than half a region. The refusal is a one-way cliff for a thread whose adaptive TLAB size climbed past that bound — see [`docs/GC.md`](GC.md). |
| `CRATONVM_G1_HUMONGOUS_BEST_FIT=0` | First-fit instead of best-fit for the humongous contiguous-run search. |
| `CRATONVM_G1_HEAP_RESIZE=1` | Opt in to the adaptive heap-sizing policy described above. Also arms the pause-driven shrink, so trailing Free regions go back to the OS at the end of an evacuation pause rather than only at cleanup. |
| `CRATONVM_G1_PAUSE_COST_MODEL=1` | Opt in to charging the mixed-CSet budget for the young half of the collection set and for the marginal fix-up walk each old candidate causes. |
| `CRATONVM_G1_HUMONGOUS_RUN_GUARD=1` | Stop a single-region Old/humongous-start claim landing in the interior of the heap's longest run of Free regions — it takes the run's END instead, shortening the run rather than splitting it. Placement only. Costs a full table pass on the claim path where the default stops at its first hit. |

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

- `GenerationalHeap::collect_garbage` spins on the live-token counter before
  entering the STW window.
- G1 honours the same counter in [`gc/src/g1.rs`](../gc/src/g1.rs)
  before `collect_garbage` proceeds.
- `VmHeap::gpu_relocation_forbidden` is the collector-facing predicate: when
  the bounded wait for the window to close expires, the generational collector
  takes its non-moving young sweep and the ZGC slide stands down for the cycle
  (decision reason `nonmoving-gpu-critical-section`).

A token is `RAII`: dropping it decrements the counter; if a worker thread
panics, drop-order still releases the GC. The practical
effect for tuning: a long GPU kernel can defer GC, growing the young set
and forcing a larger copy when GC finally runs — keep GPU work bounded or
checkpoint between launches.

> **`Heap` is not a collector you can select.** This section used to point at
> `Heap::collect_garbage` and `Heap::enter_gpu_critical`
> ([`gc/src/heap.rs`](../gc/src/heap.rs)) as if they were a production path.
> The semi-space `Heap` **struct** has no `GcBackend` variant, no `VmHeap` arm
> and no non-test constructor anywhere in the workspace — it is vestigial, and
> `vm/src/memory/roots.rs` says so in a comment of its own. The `heap` **module**
> is very much live: it owns `ObjectHeader`, `ObjectKind`, `ArrayElementType`,
> `HEADER_SIZE`, the descriptor coercion rules and several corruption probes
> that every backend uses. Read a reference to `gc/src/heap.rs` as a reference
> to the object model, not to a collector. Removing the struct is filed as
> [`gengc-plumbing-vestigial-semispace-heap-20260920`](internal/gaps/gengc-plumbing-vestigial-semispace-heap-20260920.md)
> rather than done unilaterally, because it still backs several of `gc.rs`'s and
> `collector.rs`'s trait-level doctests and examples.

## Concurrent vs STW

**G1 is the exception to this section.** Its concurrent mark cycle is not
optional and not wired through the API below: it is SATB tri-colour marking
with a background worker, driven from `interpreter::g1_concurrent_mark_cycle`
(STW initial mark -> concurrent trace -> STW remark -> cleanup), and it is what
arms mixed collections. G1's *evacuation* is stop-the-world; its *marking* is
not.

Generational is stop-the-world by default. Concurrent marking is
optionally enabled via `GenerationalHeap::enable_concurrent_gc`
([`gc/src/gen_heap.rs:6854`](../gc/src/gen_heap.rs)) and
`VmHeap::enable_concurrent_gc`
([`gc/src/vm_heap.rs:2881`](../gc/src/vm_heap.rs)) — both line numbers were
stale by thousands of lines until 2026-09-20, so prefer the symbol name to the
line; it wires the SATB queue
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

> **Environment-variable spellings on this page were audited and corrected on
> 2026-09-20.** Every `CRATONVM_*` knob is a **token in a grouped variable**
> (`cratonvm_types::flag_groups`): `CRATONVM_GC=zgc-relocate` enables,
> `CRATONVM_GC=-zgc-relocate` disables, a value goes after a second `=`
> (`CRATONVM_GC=par-threads=8`), and several tokens of one group are
> comma-separated in a **single** assignment. The old per-flag names still work
> and warn once at startup (`[cratonvm] N per-flag variable(s) set directly;
> the supported spelling is now: …`), silenced by `CRATONVM_DBG=-deprecations`.
> An unrecognised *token* is fatal; a misspelt *legacy variable* is silently
> ignored, which is how the two rows corrected in the ZGC table above went
> unnoticed. Per-row verdicts:
> [`gengc-round2-plumbing2-20260920`](internal/reviews/gengc-round2-plumbing2-20260920.md).

> **Do not reach for JFR as the GC diagnostic channel yet.** The paragraph
> below claimed, until 2026-09-20, that the collector emits four event types
> "for every minor and major collection". Checked against the tree:
>
> | event type | declared in `jfr/src/builtin.rs` | emitted by production code |
> |---|---|---|
> | `jdk.GarbageCollection` | yes | **once**, `vm/src/runtime/interpreter/gc_and_alloc.rs:1450` |
> | `jdk.YoungGarbageCollection` | yes | same site |
> | `jdk.GCHeapSummary` | yes | same site |
> | `jdk.GCPhasePause` | yes | **never** — `emit_gc_phase_pause_event` has no caller outside the jfr crate's own tests |
> | `jdk.OldGarbageCollection` | yes | **never** — same |
>
> So there is **no per-phase breakdown in JFR at all**, and no old-generation
> event. Worse, the one site that does emit passes constants: `gc_id` is the
> literal `1` on every collection (so a recording cannot tell two collections
> apart), the name is always `"YoungGC"` and the cause always
> `"Allocation Failure"` whatever actually triggered it, the tenuring threshold
> is the literal `15` (G1's — Generational's `PROMOTION_AGE` is 3), and the heap
> summary reports `committed = used` and `max = 2 × used`. A JMC session opened
> on such a recording shows a plausible-looking GC timeline that is mostly
> fabricated. Filed as
> [`gengc-plumbing-jfr-gc-events-are-constants-20260920`](internal/gaps/gengc-plumbing-jfr-gc-events-are-constants-20260920.md);
> the fix site is in `vm/`, not `gc/`. Use `--verbose:gc` and the
> `[GC] …` shutdown census until it lands.

The event types the collector declares
([`jfr/src/builtin.rs`](../jfr/src/builtin.rs)) are:

- `jdk.GarbageCollection` — wall-clock pause, generation, cause.
- `jdk.GCPhasePause` — per-phase breakdown. *(declared, never emitted)*
- `jdk.YoungGarbageCollection` / `jdk.OldGarbageCollection` — bytes
  reclaimed, before/after sizes. *(the Old one is declared, never emitted)*

Start a recording with the standard `JFR.start` JMX command. Dump with
`JFR.dump`. The recording can be opened in JDK Mission Control or any
JFR-aware analyser. (`-Xlog:gc*=info:stdout:time,level,tags` is a **unified
logging** spec, parsed by `vm/src/runtime/unified_logging.rs`; it does not
start a JFR recording, and this page implied it did.)

For an in-process view:

- `--verbose:gc` prints a one-line summary per collection to stderr on all
  three backends (see [`docs/CONFIG.md`](CONFIG.md)). **On Generational this is
  true only since 2026-09-20**: before that the flag logged an affirmative and
  enabled nothing, so the run produced no per-collection output at all — the
  shutdown `[GC] …` census did still print. See
  [`gengc-plumbing-verbose-gc-does-nothing-20260920`](internal/gaps/gengc-plumbing-verbose-gc-does-nothing-20260920.md).
- `GenerationalHeap::stats()` returns a `HeapStatsSnapshot`
  ([`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs)) with allocation counters,
  bytes promoted, dirty-card scan counts, and pause histograms.
- Allocation-rate spikes show up as elevated `bytes_allocated_since_last_gc`
  / dirty-card-scan counters in the snapshot.

In `release` builds, `tracing::debug!` and `tracing::trace!` calls compile out
(workspace `release_max_level_info` lint). **The production diagnostic channel
is `--verbose:gc` plus the `[GC] …` shutdown census, not JFR** — this paragraph
ended "the JFR path is your production diagnostic channel" until 2026-09-20,
which is the opposite of what the boxed audit above establishes. Every `[GC]`
line is an unconditional `eprintln!` and survives a release build.

## Common symptoms and remedies

| Symptom | Likely cause | First thing to try |
|---|---|---|
| Long minor-GC pauses (50 ms+) | Young space too large, or too many surviving objects | Shrink `DEFAULT_YOUNG_SEMI_SIZE` *or* lower `PROMOTION_AGE` so survivors leave the young set sooner. |
| Frequent minor GCs (multiple per second) | Young space too small, or allocation storm | Grow `DEFAULT_YOUNG_SEMI_SIZE`. Confirm with `bytes_allocated_since_last_gc` rate. |
| Allocation stall / `OutOfMemoryError` under steady-state load | Old gen filling without reclaim | Switch to G1 (better old-gen compaction), or bump `max_heap_size`. Check for retention leaks via HPROF dump (`-XX:+HeapDumpOnOutOfMemoryError`). Under G1 specifically, read `[GC] g1 ihop:` at exit first: if `threshold` was never crossed, no concurrent mark cycle ran, so no mixed collection ran and the old generation was never reclaimed at all — lower `-XX:InitiatingHeapOccupancyPercent`. |
| Long pauses every Nth collection | Old-gen full GC (compaction) | Raise `max_heap_size`, or move to G1 with `enable_concurrent_gc` so old-gen work overlaps mutators. |
| Steady RSS growth over hours/days | Slow-burn retention leak, *or* young from-space expanded to 4× initial and never shrank | Check `GenerationalHeap::stats()`: if young from-space is at `MAX_HEAP_EXPANSION_FACTOR × DEFAULT_YOUNG_SEMI_SIZE`, this is expected (no shrink path). Otherwise HPROF dump and look for reference cycles outside collectable roots. |
| Throughput regression after enabling `gpu-offload` | GC deferred by a long-lived `SafepointToken` | Break GPU work into shorter kernels; observe the token-alive counter in `Heap::stats()`. |
| Per-object `RemSet` overhead in G1 | Cross-region write storm | Raise `-XX:G1HeapRegionSize` (the ergonomic already picks 1 MiB below a 2 GiB heap) to reduce the number of distinct regions an old object can point into. Check `[g1-accessor] rset ... memo_hit_pct` under `CRATONVM_G1_DBG_ACCESSOR=1` first: a high hit rate means the barrier's per-thread edge memo is already absorbing the storm and the region size is not the problem. |

## Further reading

- Where ZGC is going, in one ordered programme with the dependency edges
  between the four current proposals and the refutation criterion for each:
  [`docs/feature-designs/zgc-roadmap-20260920.md`](feature-designs/zgc-roadmap-20260920.md).
  It also lists which claims in the two older ZGC plans are now out of date.
- Architecture overview: [`ARCHITECTURE.md`](../ARCHITECTURE.md).
- Per-module algorithm notes: every `gc/src/*.rs` file starts with a `//!`
  doc block (e.g. [`gc/src/g1.rs`](../gc/src/g1.rs),
  [`gc/src/concurrent_mark.rs`](../gc/src/concurrent_mark.rs),
  [`gc/src/satb.rs`](../gc/src/satb.rs)).
- Lock hierarchy (heap is L8): [`vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs).
- Embedding the VM from Rust: [`docs/EMBEDDING.md`](EMBEDDING.md).
- Profiling: [`docs/PROFILING.md`](PROFILING.md).
