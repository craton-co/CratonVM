# Garbage Collector Tuning Guide

Audience: developers tuning CratonVM heap behaviour for a specific workload
— Quarkus boot, JFR-recorded benchmarks, low-latency request handlers,
long-running daemons, or test fixtures with tight budget envelopes.

Source: [`gc/src/`](../gc/src/) — **61 files** (43 top-level, 17 under
`gc/src/zgc/`, 1 under `gc/src/old_gen/`), **~264 k LOC** as of 2026-09-26.
Re-derive rather than trusting it: `find gc/src -name '*.rs' | wc -l` and
`… | xargs cat | wc -l`. The figure has been wrong on this page several
times, which is why the command is written out here rather than the figure
alone.
The collector
dispatches through the `VmHeap` enum
([`gc/src/vm_heap.rs`](../gc/src/vm_heap.rs)); each backend is a separate
module.

> **Status as of 2026-09-26.** Every flag, default and constant on this page
> was re-checked against the code on that date (`types/src/flag_groups.rs`,
> `types/src/flags.rs`, `types/tests/flag-surface.txt`, the collector sources
> and `vm-cli/src/main.rs`). What moved since the previous revision:
> the **8-byte object header** (2026-09-24, see "Heap totals");
> the launcher's **ergonomic default `-Xmx`**; G1's adaptive young target is
> **default ON** (it was listed as opt-in); `-XX:MaxGCPauseMillis`,
> `-XX:G1MixedGCLiveThresholdPercent` and `-XX:G1HeapWastePercent` are
> honoured; the Generational old generation runs a **concurrent-first**
> policy by default, with growth hysteresis, a decommit of its free tail and
> a compaction on a fragmentation refusal all default ON; and the HotSpot
> tenuring flags (`-XX:MaxTenuringThreshold` and friends) reach the
> Generational collector. Open collector defects that change what an operator
> sees are linked where they bite (G1's first parallel pause commits `-Xmx`;
> ZGC decommits below `-Xms`; G1 refuses humongous allocations on a
> fragmented heap).

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
collection that can compact (`GenerationalHeap::major_gc`) is called only from
the moving young path (`collect_garbage_inner`), it stops the old generation
compacting too. On a default run nothing in the heap then relocates. (On the
moving path the old generation compacts in two cases: always under
`CRATONVM_OLDGEN_COMPACT=1` / `CRATONVM_GC=oldgen-compact`, off by default
since 2026-08-03; and, **default ON since 2026-09-24**, for the one collection
after an old-gen allocation was refused for want of a contiguous block,
`CRATONVM_GC_OLD_OOM_COMPACT`, `=0` off. The non-moving path compacts only
under the opt-in `CRATONVM_GC_OLD_PINNED_COMPACT`, so leave that unset while
bisecting.) A crash, a stale-reference warning or a silently-wrong result
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
`[GC] moving_young:` — that line is printed only on Generational and only when
moving-young is enabled (`VmHeap::print_gc_summary`), so it is absent by design
on exactly this run.

> **If you ran this bisect before 2026-09-21, re-run it.** The flag did not work
> on a process with no live compiled frame: the term that diverts the cycle
> required a live JIT frame *as well as* the gate being off, so a short repro,
> a unit-test-shaped reproduction — anything that had not tiered up — moved on
> every collection with the flag set. Measured at `d6d5cd262`: 1512 of 1512
> cycles `kind=moving`, `reason=moving-no-jit-frames-live`, on a run whose own
> decision record said `moving_young_requested=false`. **Any "not a relocation
> defect" conclusion drawn from that lever on a JIT-cold workload is void.**
> (`docs/internal/gc/gengc-probe-no-moving-young-optout-does-not-opt-out-FIXED-20260923.md`.)

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
| What does a sweep do per dead object? | It zeroes the object's first `HEADER_SIZE` (16) bytes only (`CRATONVM_ZGC_SWEEP_HEADER_ZERO=0` restores a full-body memset, which is redundant with the memset `alloc_raw` already does on every handout) and hands over one free-list span per **run** of adjacent dead objects rather than one span per object (`CRATONVM_ZGC_SWEEP_DEAD_RUNS=0` restores the per-object calls, each of which then needs sorting in `coalesce_free_list`). **Both apply to every cycle**, and the saving is on the whole-heap one: a young cycle's sweep is 6.3 ms of a 112 ms pause where a whole-heap cycle's is 170 ms of 265 ms, because it walks 13.0M dead objects against a young cycle's 137k. They were scoped to young cycles when they landed on 2026-08-17 — a mid-gauntlet caution that has since expired — and the restriction pointed the change away from the cost. Engagement on `[GC] zgc-sweep-cost:`: **`zero_bytes_skipped=0` with collections having run means the first is on and inert, and `dead_runs == dead_objects` means the second is**. The flags were `CRATONVM_ZGC_GEN_*` before the scoping was removed; **the old names are dead and set silently to nothing** | `ZgcRealHeap::collect_garbage`'s `ZSweepCfg`, `zgc_sweep_header_zero` / `zgc_sweep_dead_runs` |
| Is it safe to stop zeroing a dead object's body? | Yes, and the reason is that the header is the whole of the property. The sweep zeroed to stop "a later scan seeing a stale header". Since the 8-byte header (2026-09-24) every object starts with one header word (class id and mark word) and is at least `MIN_OBJECT_SIZE` (16) bytes; arrays and legacy instances carry a second shape/aux word, a compact instance has its first field there. Zeroing the first 16 bytes therefore clears the whole header of every object, and the result -- class id 0, a mark word with no `COMPACT` flag, shape 0 -- is the same `class_id=0, num_slots=0` corpse a reader of a vacated span always saw. `ARRAY_DATA_OFFSET == HEADER_SIZE`, so an array's length is still in the shape word, not a body prefix. The body is only reachable **through** that header: field reads size the object from its shape or its class's compact layout, extent walks from `alloc_size(header)`, membership from the registry the sweep just removed the base from. And reuse was never the reason -- `alloc_raw` and `tlab_refill` memset unconditionally | `ObjectHeader`, `ZgcRealHeap::alloc_raw` |
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
  minor-GC pause envelope. On a VM-built heap each young semi-space is
  **`-Xmx / 4`** (`GenerationalHeap::with_capacity` in
  [`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs)), so 64 MiB at the 256 MiB
  library default but 1 GiB under the launcher's ergonomic 4 GiB heap;
  `-Xmn` / `-XX:NewRatio` resize it (see "Young generation size" below).
  `DEFAULT_YOUNG_SEMI_SIZE` (64 MiB) is used only by the argument-less
  `GenerationalHeap::new`, not by the VM.
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
  has been on by default since **2026-08-13**
  (`ZgcRealHeap::relocation_requested_by_default` in `gc/src/zgc.rs`), pinned by
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
- `CRATONVM_ZGC_PAUSE_TARGET_MS` — **default 200**; `0` turns the pause-target
  budget off. `-XX:MaxGCPauseMillis=<n>` sets the same target and wins over
  the variable (`ZgcRealHeap::set_pause_target_ms`, applied after the heap is
  built). The feedback loop is blind to the first cycle, so that one still
  runs to the occupancy clause's 75%. (The generated inventory renders
  `CRATONVM_ZGC_ALLOC_TRIGGER` as `default-on` because its row carries an
  off word; the code default is off, `zgc_alloc_trigger_percent`.)

### Targeted compaction, and the budget it no longer obeys

`CRATONVM_ZGC_TARGETED_COMPACTION` is **opt-in, default OFF**
(`targeted_compaction_enabled` in `gc/src/zgc.rs`; the `targeted-compaction`
row of `types/src/flag_groups.rs`). It lets an
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
> `types/src/flag_groups.rs` (`INVENTORY`, among the `zgc-*` rows marked
> `since: "2026-09-20"` or `"2026-09-21"`), carry
> `CRATONVM_GC=<token>` spellings (`docs/flag-tokens.md`) and appear in
> [`docs/config/flag-inventory.md`](config/flag-inventory.md) with the correct
> opt-in/default-on rendering. The gap this block used to name
> (`docs/internal/zgc-round-20260920/gap-g-new-zgc-flags-are-not-in-the-flag-inventory.md`)
> is **closed**, including the residue in which seven default-OFF booleans were
> declared with `off_word: Some("0")` and therefore rendered as *default-on*;
> that was fixed in the same wave 3 commit that recorded it, `9a03106ff`.

## Sizing knobs

### Heap totals (`VmConfig`)

- `max_heap_size` — `-Xmx` equivalent. Sizes the entire managed heap; the
  backend partitions it into young/old (generational) or regions (G1). The
  **library** default (`VmConfig::default`, every embedding entry point) is
  256 MiB. The **`cratonvm` launcher** without `-Xmx` sizes it like a stock
  JDK instead: a quarter of physical RAM (of the cgroup limit, if smaller),
  capped at 4 GiB, floored at 256 MiB and never above the basis
  (`ergonomic_default_max_heap` / `clamp_ergonomic_heap`,
  `vm/src/runtime/container.rs`). `CRATONVM_DEFAULT_HEAP_MAX_MB` moves the
  cap, `CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` restores the fixed 256 MiB, and
  `--verbose:gc` prints the size chosen. An embedded VM inside a cgroup that
  left the default in place gets a quarter of the cgroup limit, same cap and
  floor (`suggested_default_max_heap`); an uncontained embedder keeps 256 MiB.
- `initial_heap_size` — `-Xms` equivalent. Default 16 MiB. The bytes committed
  at startup, honoured on all three backends since 2026-09-21: G1 commits the
  prefix of its reservation, ZGC the prefix of its arena, Generational the
  request across its two young semi-spaces and, since 2026-09-23, whatever
  they cannot absorb as an old-generation prefix (the old generation reserves
  and commits on demand like the rest of the heap; before that it was
  committed whole at startup). The DEFAULT is why no diagnostic is
  raised from the heap constructor — 16 MiB arrives there on every run and is
  indistinguishable from an explicit flag, so only the launcher can tell the
  two apart.
- **What `Runtime.totalMemory()` reports.** The heap's COMMITTED size
  (`VmHeap::committed_bytes`), on every backend since 2026-09-23 — so a fresh
  `-Xms64m -Xmx512m` process reports about 64m, as on HotSpot, and
  `freeMemory()` is that minus occupancy. Before that date Generational and
  ZGC reported their reservation (`-Xmx`), and a cache sized from
  `totalMemory()` or `freeMemory()` was sized against a heap that did not
  exist. One backend-specific movement to expect, still open on
  2026-09-26 (G1's first parallel young pause committed its whole
  reservation until 2026-09-30 —
  `docs/internal/gc/g1-parallel-survivor-claim-commits-the-whole-reservation-FIXED-20260930.md`):
  **ZGC** falls after any collection that freed memory, below `-Xms` too,
  because it hands free granules back at the start of every cycle
  (`docs/known-issues/gc/zgc-decommit-ignores-xms.md`).
  **Generational** can now fall too, but never below `-Xms`: since
  2026-09-24 a stop-the-world old-gen collection decommits the old
  generation's free tail (`CRATONVM_GC_OLD_SHRINK`, default ON, `=0` off;
  after a concurrent cycle only with the opt-in
  `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT`).
  `Runtime.maxMemory()` is `-Xmx` everywhere.
- **Object size: the 8-byte header** (since 2026-09-24,
  `docs/architecture/compact-object-and-field-layout.md`). Every object starts
  with one 8-byte header word (class id and a 32-bit mark word). A **compact
  instance** has nothing else: its fields are packed at their natural widths
  from offset 8, and the object is rounded to 8 bytes with a 16-byte minimum
  (`MIN_OBJECT_SIZE`), so a node with two references and an int is 24 bytes
  instead of 32, as on HotSpot. **Arrays** keep the 16-byte header
  (`HEADER_SIZE`, `ARRAY_DATA_OFFSET`), so a large-array workload sees no
  change. **Legacy instances** — classes that are compatibility stubs, or
  inherit from one — keep the 16-byte header *and* 16-byte field cells, and
  are several times larger than their HotSpot counterparts. What this means
  for sizing: an instance-heavy live set is smaller than it was before
  2026-09-24, so a `-Xmx` tuned before that date has more headroom now, and
  `HeapUsedDelta`-style heap numbers are not comparable across the change.
  It is not a switch: `-XX:+UseCompactObjectHeaders` is accepted and only
  echoed back in the reported JVM flags (`VmConfig::use_compact_headers`);
  the layout is the same with or without it. Two side effects an operator may
  see: the identity hash of a compact instance has 20 significant bits, and a
  hashed compact instance inflates its monitor on the first `synchronized`.
- **`-XX:MaxMetaspaceSize=<size>`** (launcher, since 2026-09-24; not a
  `VmConfig` field — the launcher installs it on the VM's class manager). A
  class defined from raw bytes that would push the charged class metadata
  past the limit triggers one full collection (class unloading returns dead
  loaders' charges) and, if it still does not fit, fails with
  `OutOfMemoryError: Metaspace`. That collection is not a `System.gc()`
  (gc-common w7-g): the defining thread holds the class-loading lock, so it
  runs no finalizer, Cleaner or GC-notification drain there; the next GC door
  delivers them. The charge is the class-file size of every
  class not defined by the bootstrap loader, a deliberate under-estimate of
  HotSpot's metaspace use: a value that works on HotSpot is never hit
  earlier here. It is NOT the non-heap `used` JMX reports (a walk of
  everything the class store holds, every class included), and the non-heap
  `max` stays `-1`. Unset = unbounded. `-XX:MetaspaceSize` is accepted and
  ignored.
- **`-XX:+UseCompressedOops`** (alias `CRATONVM_COMPRESSED_OOPS=1`; opt-in,
  default off, Generational only — G1 and ZGC run 64-bit references and say
  so). Reference fields and array elements become 4 bytes against a window
  fixed from the heap's reserved spans at init (`HeapBased`, shift 3, base
  8 GiB below the lowest span). The width is **per process**, not per VM: in
  an embedding process the first VM decides it, and since gc-common w8-f a
  later VM that cannot run at that width — its heap outside the window, no
  flag, or an unaudited collector — is refused at creation rather than handed
  the window unchecked, while a later flag after a 64-bit VM falls back to
  64-bit references with a message (`compressed_oops::admit_heap`). Measured
  at 4.7 % of peak RSS here, not HotSpot's 20-30 %
  (`gc/src/compressed_oops.rs` module doc).

### Generational (`gc/src/gen_heap.rs`)

| Constant | Value | Rationale |
|---|---|---|
| young semi-space | **`-Xmx / 4`** each | What a VM-built heap actually uses (`GenerationalHeap::with_capacity`: half the heap for the from/to pair, half for the old generation). `-Xmn`, `-XX:NewSize`, `-XX:MaxNewSize` and `-XX:NewRatio` change the split (next section). Smaller young = more frequent minor GCs; larger young = longer copy latency but more time for objects to die. The constant `DEFAULT_YOUNG_SEMI_SIZE` (64 MiB) is only `GenerationalHeap::new`'s, for tests and embedders that build a heap with no budget. |
| `YOUNG_GC_THRESHOLD_PERCENT` | **50** | Minor GC fires when from-space utilisation crosses this percentage. **Not 75** — this row said 75 until 2026-09-20; the constant (`gc/src/gen_heap.rs`) has read 50 since the moving young gen shipped, because to-space must be able to hold every survivor. Settable at runtime, unlike its neighbours in this table: `CRATONVM_GC=young-trigger-percent=<n>`, clamped to `1..=95`. Raising it collects less often and copies more survivors per cycle; which wins is a property of the workload's survival rate. **What it actually sets** is the INITIAL threshold and the CEILING of the pause-goal loop (`CRATONVM_GC_YOUNG_PAUSE_MS`, default 200), which then moves the live value; and with the goal OFF it does not reach non-moving cycles at all (those fire at the 90 % row below). `GenerationalHeap::young_trigger_census_line()` (`[GC] young_trigger:`) reports the live threshold and every loop decision — see proposal #4 of [the 2026-09-20 round's design page](feature-designs/gc-round-20260920-generational.md) for the sweep protocol. |
| `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT` | 90 | The **second** young trigger, and this table did not mention it. The non-moving sweep reclaims dead spans in place and needs no Cheney headroom, so a cycle that is going to be non-moving is allowed to fill 90 % of the semi-space first. Not settable. A workload that diverts to the sweep every cycle (see `docs/GC.md`'s `divert_non_moving` table — a live JIT frame does this) is therefore running at the 90 % trigger, not the 50 % one, and its young occupancy curve looks nothing like this table implies. |
| `MAX_HEAP_EXPANSION_FACTOR` | 4 | Young from-space can grow up to 4× initial when minor GC reclaims < 25 % — **but only on a heap built by `with_sizes`** (explicit arena sizes, no budget: tests and embedders). The production constructor, `with_capacity` (and `-Xmn`'s `with_capacity_and_young`), passes the initial semi as the growth ceiling because the `-Xmx` budget leaves growth nothing to take, so on a VM-built heap this row is inert. **The young generation has no shrink path**; the old generation's committed size does shrink since 2026-09-24 (`CRATONVM_GC_OLD_SHRINK`, see "Heap totals"). |
| old-gen triggers | concurrent cycle at an adaptive occupancy (45 % until measured, never below 20 %); stop-the-world collection at 75 % | **Concurrent-first, default since 2026-09-24** (`OldGenPolicy::ConcurrentFirst`, `gc/src/concurrent_mark.rs`). A concurrent old-gen mark cycle opens at an initiating occupancy *below* the 75 % stop-the-world floor, predicted from the old-gen growth one cycle was measured to see (`ConcurrentStartPolicy`); `CRATONVM_GC_CONC_START_PERCENT=<n>` (`CRATONVM_GC=conc-start-percent=<n>`, clamped to 20–75) fixes it instead. While a cycle is open the stop-the-world old-gen collection inside a young pause **defers** to it, unless it was requested (`System.gc()`), an old-gen allocation failed, or the generation is 90 % full. **Growth hysteresis is ON under this policy** (flipped 2026-09-24): above 75 %, a repeat stop-the-world collection waits until half of what the previous one left free — at least 1/16 of the generation — has been promoted since; a failed old-gen allocation always collects; `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS=0` (`CRATONVM_GC=-old-trigger-hysteresis`) switches it off. `CRATONVM_GC_NO_CONCURRENT_FIRST=1` (`CRATONVM_GC=-concurrent-first`) restores the pre-2026-09-24 legacy policy: one shared 75 % trigger, no deferral, hysteresis off unless set. `GenerationalHeap::old_gen_trigger_stats()` counts `would_suppress` and `low_yield`, and `[GC] major_cadence:` splits the majors by cause. |
| `PROMOTION_AGE` | 3 | Survive this many minor GCs before being promoted to old gen. Lower (1–2) sends short-lived but slightly-tenured objects to old prematurely; higher (4–6) costs more copy work but keeps the old gen denser. **Settable since 2026-09-24** through the HotSpot tenuring flags, which also switch on HotSpot-style *adaptive* tenuring for this VM's heap: `-XX:MaxTenuringThreshold=<0..15>` (ceiling, HotSpot default 15), `-XX:InitialTenuringThreshold=<0..15>`, `-XX:TargetSurvivorRatio=<0..100>` (default 50) and `-XX:+PrintTenuringDistribution` (prints the age table after every moving young collection). Any one of the three values engages it (`TenuringConfig`, `gc/src/gen_heap_tenuring.rs`); `CRATONVM_GC_ADAPTIVE_TENURING` engages it with HotSpot's defaults. A bad value is a `Warning:` and is ignored; under G1 or ZGC the flags print one `note:` and do nothing. Unset, the fixed age 3 applies, as before. |

The remaining constants have no setter; embedders who need
non-default sizing can construct `GenerationalHeap::with_capacity` (or
`with_sizes`) directly, then build the `VmHeap` around it. The defaults
are deliberate and cover the vast majority of workloads.

#### Young generation size: `-Xmn`, `-XX:NewSize`, `-XX:MaxNewSize`, `-XX:NewRatio`

Since 2026-09-23 the **Generational** collector honours HotSpot's four young
sizing flags (`gc/src/gen_young_sizing.rs`, `VmConfig::young_gen_sizing`).
Before that date all four were accepted and silently dropped, and the young
generation was always half of `-Xmx`.

HotSpot's young generation is eden plus two survivors; this collector's is the
from/to semi-space **pair**, so a young size is the pair and each semi-space
gets half of it. The old generation gets the rest of `-Xmx` — the pair plus the
old generation is always exactly `-Xmx`.

| Flag | Meaning here |
|---|---|
| none of the four | Default, unchanged: young pair = `-Xmx / 2` (each semi `-Xmx / 4`). |
| `-Xmn<size>` (also `-Xmn <size>`) | Young pair = `<size>`, exactly. Wins over the other three (a `Warning:` says so). |
| `-XX:NewRatio=<n>` | Young pair = `-Xmx / (n + 1)`. `NewRatio=1` is the default split. |
| `-XX:NewSize=<size>` | A **floor** on the NewRatio-derived (or default) pair. |
| `-XX:MaxNewSize=<size>` | A **ceiling** on it. `NewSize > MaxNewSize` raises the ceiling to `NewSize`, as HotSpot does. |

Why floor/ceiling rather than HotSpot's "start at `NewSize`, grow toward
`MaxNewSize`": this collector's young pair does not resize once built (under an
`-Xmx` budget the growth path has nothing to give — see `with_capacity`), so a
single size held between the two bounds is the honest reading.

**Clamps.** The old generation keeps at least 1 MiB (half the heap on a heap
under 2 MiB) and each semi-space is at least 1 KiB. Every clamp or override
prints one `Warning:` line at startup; `--verbose:gc` also prints the resolved
split (`[cratonvm] generational young generation: 256 MiB (-Xmn; 2 x 128 MiB
semi-spaces), old generation 768 MiB, heap 1024 MiB`).

**Other collectors.** G1 sizes its young generation from region counts and ZGC
has none; on either, passing any of the four prints one
`[cratonvm] note: … honoured by the Generational collector only` line and the
flags have no effect.

**What changes when you shrink the nursery.** The young trigger
(`YOUNG_GC_THRESHOLD_PERCENT` above) and the humongous threshold (half a semi,
capped at 256 MiB) both scale with the semi-space, so `-Xmn` smaller than the
default means more frequent minor collections AND more arrays allocated
straight into the old generation. The historical measurement against a smaller
initial semi on JIT-heavy workloads (a net regression, see `with_capacity`'s
doc) still applies: this is an operator's knob, not a recommended default.

#### Opt-in allocation experiments (default OFF)

| Variable | Effect |
|---|---|
| `CRATONVM_TLAB_FILLER_SKIP_ZERO=1` (`CRATONVM_GC=tlab-filler-skip-zero`) | Every backend: a Java thread's TLAB retire writes the tail filler's header but not the memset of its data area, which is still zero from the refill. A tail whose first word is not zero is zeroed as before. |
| `CRATONVM_TLAB_GATE_BUMP_FLOOR=1` (`CRATONVM_GC=tlab-gate-bump-floor`) | Generational only in effect: the compiled-code TLAB refill gate accepts a young bump tail smaller than the full request (down to the object being allocated) instead of spilling that allocation to the old generation. |
| `CRATONVM_TLAB_SHARE_SIZER=1` (`CRATONVM_GC=tlab-share-sizer`) | Every backend. HotSpot-shaped TLAB sizing: each thread asks for a fiftieth of its exponentially averaged bytes allocated per young pause (`TLABWasteTargetPercent` 1 %, `TLABAllocationWeight` 35 %), clamped to 8 KiB – 1 MiB, and a compiled allocation that misses with a tail bigger than `desired / 64` keeps the buffer and allocates outside it (the refill-waste limit). Replaces the ladder sizer when on (the ladder's two retired-buffer switches and the Generational pristine-chunk skip were removed in gce e2/o); read back on `[GC] tlab-sizer:`. Its protocol-A win on Generational (W1 610 → 63 minors) made it the Generational default for a moment in gen r5w4, **reverted** before merging: with the tail sink it hands out live memory (`docs/internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`). Do not turn it on until that page is closed. |

All are off until their A/B runs are taken; the protocols are in
`docs/internal/reviews/gengc-round4-w2-alloc2-20260923.md` (the first two),
`docs/internal/reviews/gengc-round4-w3-alloc3-20260923.md` (the next three)
and protocol A of `docs/internal/reviews/gengc-round5-w2-alloc6-20260926.md`
(which measured the share sizer and the tail sink on the Generational heap;
the tail sink is in the next table now, the share sizer waits on its defect page). Every default lives in one place,
`cratonvm_types::flags::alloc_policy_defaults`: flipping an arm is that
constant (plus its row's `off_word` in `types/src/flag_groups.rs`, for the
generated flag docs).

#### Allocation kill switches (default ON)

One row below (`CRATONVM_SOFTREF_HOTSPOT_LRU`) is ON by default on the **Generational heap only**
(`-XX:+UseGenerationalGC`, gen r5w4, 2026-09-26): their default is per backend
(`alloc_policy_defaults::BackendDefault`) because its evidence was taken on
Generational alone (the share sizer used the same mechanism and was reverted). On G1 and ZGC it stays off unless set to `1`; on every
backend an explicit `=0` / `=1` wins. Each constant in `types/src/flags.rs`
carries its evidence.

| Variable | Effect of `=0` |
|---|---|
| `CRATONVM_GEN_TLAB_TAIL_SINK=0` (`CRATONVM_GC=-gen-tlab-tail-sink`) | Generational (default ON since gen r5w2): buries a retiring TLAB's unused tail under an `int[]` filler again. ON, the tail goes back to the young from-space (the bump cursor retracts, or the tail joins the young free list) instead of being counted as occupied by the young trigger. Read back as the `young_tlab_tail_*` counters of `HeapStatsSnapshot`. |
| `CRATONVM_SOFTREF_HOTSPOT_LRU=0` (`CRATONVM_GC=-softref-hotspot-lru`) | **Generational default ON** (gen r5w4; G1 and ZGC default OFF): the SoftReference LRU policy goes back to the wall clock now and the free space measured before the collection. ON, it takes HotSpot's `LRUMaxHeapPolicy` inputs — the soft clock and the free heap as of the end of the previous collection — so a soft reference read since the last collection is never cleared by the policy. |
| `CRATONVM_GEN_XMS_USABLE_FIRST=0` (`CRATONVM_GC=-gen-xms-usable-first`) | Generational (default ON since gen r5w4): splits the `-Xms` startup commit evenly across the two semi-spaces again, so `Runtime.totalMemory()` starts at about `-Xms / 2`. ON, it is split the way HotSpot Serial sizes its initial heap (one survivor of copy reserve in the to-space, the rest usable): a fresh `-Xms64m -Xmx512m` heap reports about 64m. |
| `CRATONVM_GEN_ZERO_ONCE=0` (`CRATONVM_GC=-gen-zero-once`) | Restores the memset of every young hand-out. ON, a TLAB chunk or slow-path object carved from the young bump tail is not zeroed again: the young wipe (or the OS, for a freshly committed page) is its one zeroing point. Free-list reuse is always zeroed. Read back as `gen_zero_skipped_bytes=` / `gen_zeroed_bytes=` on `[GC] gen-alloc:`; `gen_zero_repairs=` non-zero is a finding. |
| `CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED=0` (`CRATONVM_GC=-gen-humongous-zero-unlocked`) | Zeroes a humongous array under the old-generation lock again. ON, a primitive humongous array (64 KiB or more) has its body zeroed after the lock is released; reference arrays always keep the locked memset. Read back as `gen_humongous_unlocked_zeroes=`. |

#### Old-generation switches (gen round 4, 2026-09-23/24)

The `types/src/flag_groups.rs` rows carry the full reasoning; these are the
ones that change what the old generation does with memory.

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GC_OLD_SHRINK` (`old-shrink`) | **ON** (`=0` off) | Decommit the old generation's free tail after a stop-the-world old-gen collection (HotSpot's `MaxHeapFreeRatio` shape), never below its `-Xms` share. |
| `CRATONVM_GC_OLD_OOM_COMPACT` (`old-oom-compact`) | **ON** (`=0` off) | Compact a fragmented old generation for the one collection after an allocation was refused for want of a contiguous block, instead of throwing a false `OutOfMemoryError`. Moving path only; a cycle with conservative roots downgrades to the in-place sweep. |
| `CRATONVM_GC_OLD_WALK_GAP_RECOVERY` (`old-walk-gap-recovery`) | **ON** (`=0` off) | An in-place sweep that meets an unwalkable gap scans it conservatively and carries on, instead of failing closed for the whole generation. |
| `CRATONVM_GC_OVERHEAD_PROGRESS` (`overhead-progress`) | **ON** (`=0` off) | The mutator-progress half of the GC-overhead limit, and the full collection before its error (see "GC overhead limit" below). |
| `CRATONVM_GC_OLD_FRAG_COMPACT` (`old-frag-compact`) | off | Proactive fragmentation-triggered compaction, before any allocation fails. |
| `CRATONVM_GC_OLD_PINNED_COMPACT` (`old-pinned-compact`) | off | Let the non-moving path answer a fragmentation request by compacting around the objects conservative roots pin. |
| `CRATONVM_GC_OLD_GIVE_BACK` (`old-give-back`) | off | Also return large free granules after an in-place sweep, not only the tail. |
| `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` (`old-shrink-after-concurrent`) | off | Run the committed-size shrink after a concurrent cycle's sweep too. |
| `CRATONVM_GEN_CONC_SERVICE_THREAD` (`gen-conc-service-thread`) | off | Run the concurrent old-gen cycle on a service thread instead of on the mutator whose young pause found it due. |

### G1 (`gc/src/g1.rs`)

`G1CollectorConfig` is exposed publicly. Every row below is the value in
`G1CollectorConfig::default()`; the `-XX:` column is the operator spelling
where one exists (`VmHeap::new_with_overrides`).

| Field | Default | `-XX:` | Effect |
|---|---|---|---|
| `heap_size` | 256 MiB | `-Xmx` | Total managed heap. Since F-16 this is the size of the **reservation** — address space — not of the memory charged to the process at startup. |
| `initial_heap_size` | `0` = ergonomic | `-Xms` | Bytes committed up front. The ergonomic is a **sixteenth of the heap, floored at four regions**, so a short-lived process pays almost nothing for a large `-Xmx`. The prefix then grows on demand as regions are claimed; `CRATONVM_G1_RESERVE_HEAP=0` restores the pre-F-16 "commit everything at startup". |
| `region_size` | 1 MiB, **but see the ergonomic** | `-XX:G1HeapRegionSize` | Granularity for evacuation and remembered-set tracking. `VmHeap` overrides the default with `g1_ergonomic_region_size` = `max(heap / 2048, 1 MiB)` clamped to `[256 KiB, 32 MiB]`, so the region COUNT stays near 2048 at any heap size. Raising it reduces remembered-set overhead at the cost of more wasted bytes per part-full region — and raises the humongous threshold, which is `region_size / 2`. |
| `max_gc_pause_ms` | 200 | `-XX:MaxGCPauseMillis` | The pause goal (the same flag also sets ZGC's pause target). It reaches **two** decisions: how many OLD regions a mixed collection set may take (`old_cset_copy_budget_ns` = the goal minus the rolling fix-up cost), and the adaptive young-region target (`update_young_target`). It does **not** move the marking threshold any more — see `ihop_percent`. |
| `ihop_percent` | **70** | `-XX:InitiatingHeapOccupancyPercent` (1–100) | Old-generation occupancy at which a concurrent mark cycle starts. A **ceiling** on the adaptive threshold, never a floor: `CRATONVM_G1_ADAPTIVE_IHOP=0` restores the old pause-time-driven model. Raised from HotSpot's 45 because the default heap is small; on a 16 GiB heap you want it back near 45. |
| `promotion_age` | 15 | — | The tenuring **ceiling**. Since F-18 the effective threshold is re-derived after every pause from an age histogram of surviving bytes and may be lower — never higher — when survivor space would overflow its target (an eighth of the young generation). `CRATONVM_G1_ADAPTIVE_TENURING=0` pins it at the configured value. |
| `gc_worker_threads` | `0` = ergonomic | `-XX:ParallelGCThreads` | **No longer a no-op.** The parallel evacuator is on by default; `0` derives the count from the machine (`ergonomic_gc_worker_threads`, HotSpot's taper), a non-zero value is an explicit request still clamped to the hardware, and `CRATONVM_G1_WORKERS=<n>` overrides both. `=1` drains the parallel path serially, which is the lever that separates a concurrency race from a logic divergence. The generational young collector has its own worker policy (`CRATONVM_GC=par-threads=<n>`) and does not read this field at all. |
| `string_dedup_enabled` | false | `-XX:+UseStringDeduplication` | Off by default. The table is now remapped and purged by every collection path, so a caller is safe to add — but a HIT is a hash match, not an equality proof. |
| `mixed_gc_count_target` | 8 | — | Mixed-collection cycles armed after a mark cycle completes. |
| `old_cset_region_threshold_percent` | 10 | — | Cap on old regions per mixed GC, as a percent of the region count (minimum one region, so a tiny heap still makes progress). |
| `mixed_gc_live_threshold_percent` | 85 | `-XX:G1MixedGCLiveThresholdPercent` (1–100) | HotSpot's `G1MixedGCLiveThresholdPercent`. An Old region at or above this percent live is never a mixed candidate: evacuating it is nearly a full copy for almost no reclaim. |
| `heap_waste_percent` | 5 | `-XX:G1HeapWastePercent` (0–100) | HotSpot's `G1HeapWastePercent`. The mixed phase ends early once the garbage held by the remaining candidates is below this share of the heap — **unless the Free pool is tight**, in which case the floor is waived (a mixed pause is this collector's only full GC). |

Two sizing policies have no `G1CollectorConfig` field and are worth knowing
because they, not the fields above, decide when most pauses happen:

- **The free-fraction trigger.** `needs_gc` fires when the Free region
  fraction drops below 25 %. That percentage is itself adaptive: it is raised
  after a collection that hit evacuation failure, because the free pool at
  trigger time *is* the young evacuation's to-space. Its periodic free-region
  recount is scheduled by a per-mutator thread-local countdown, so the common
  allocation path does not perform a shared atomic RMW.
- **The adaptive young target** (**default ON since 2026-09-02**;
  `CRATONVM_G1_YOUNG_PAUSE_TARGET=0` restores the free-pool-only trigger).
  Bounds Eden+Survivor between 5 % and 60 % of the region count: tighten 20 %
  after a *productive* pause that overruns the goal, give 12.5 % back while
  pauses stay under half of it, and reset to the ceiling after an unproductive
  pause so it can never storm. It starts at the ceiling and a target at the
  ceiling is deliberately not a trigger, so it changes nothing until an
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
reports the **committed prefix** (`VmHeap::committed_bytes` →
`G1Collector::committed_bytes`, i.e. `committed_len`), which follows the soft
capacity only as far as the shrink actually uncommits — this line said "reports
the soft capacity" until 2026-09-23, which is `heap_capacity()`, a different
accessor. `Runtime.maxMemory()` still reports `-Xmx`. (Today the committed
prefix reaches the whole reservation at the first parallel young pause, and the
trailing-Free-run shrink cannot hand it back while a Survivor region sits in the
top region — see "Heap totals" above.)

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
| `CRATONVM_G1_YOUNG_PAUSE_TARGET=0` | Switch off the adaptive young-region target described above (default ON). |
| `CRATONVM_G1_UNCOMMIT=1` | Opt in to returning trailing Free regions to the OS at cleanup. Never below `-Xms`. |
| `CRATONVM_G1_EAGER_HUMONGOUS=0` | Stop freeing unreferenced humongous spans at every pause; wait for a mark cycle instead. |
| `CRATONVM_G1_ADAPTIVE_TENURING=0` | Pin the tenuring threshold at `promotion_age`. |
| `CRATONVM_G1_ADAPTIVE_IHOP=0` | Restore the pause-time-driven marking threshold. |
| `CRATONVM_G1_EDEN_STRIPES=<n>` | Pin the number of Eden stripes mutators allocate through (default: hardware parallelism, capped at an eighth of the region count). `=1` is the single global Eden. |
| `CRATONVM_G1_TLAB_CLAMP=0` | Refuse, rather than clamp, a TLAB request larger than half a region. The refusal is a one-way cliff for a thread whose adaptive TLAB size climbed past that bound — see [`docs/GC.md`](GC.md). |
| `CRATONVM_G1_HUMONGOUS_BEST_FIT=0` | First-fit instead of best-fit for the humongous contiguous-run search. |
| `CRATONVM_G1_HEAP_RESIZE=1` | Opt in to the adaptive heap-sizing policy described above. Also arms the pause-driven shrink, so trailing Free regions go back to the OS at the end of an evacuation pause rather than only at cleanup. |
| `CRATONVM_G1_PAUSE_COST_MODEL=1` | Opt in to charging the mixed-CSet budget for the young half of the collection set and for the marginal fix-up walk each old candidate causes. |
| `CRATONVM_G1_HUMONGOUS_RUN_GUARD=0` | Opt out of the default run-preserving placement and restore the hinted first-Free-region scan for an explicit performance A/B. The default takes the longest run's END instead of its interior, preventing a one-region Old claim from splitting the span a later humongous allocation needs. |

**Open G1 defects that change a tuning decision** (all collector-owned, all
re-checked against the code on 2026-09-26):

- **The parallel evacuator is the default** and `CRATONVM_G1_PARALLEL_EVAC=0`
  is the bisect for a G1-only hang. The one hang that disappeared under it
  had a root cause and is fixed (2026-09-30,
  `docs/internal/gc/g1-parallel-evac-pool-exhaustion-loses-a-held-monitor-FIXED-20260930.md`):
  a peer's unpublished forwarding claim was abandoned after a bounded wait.
  The first parallel young pause no longer commits the whole reservation
  (same date,
  `docs/internal/gc/g1-parallel-survivor-claim-commits-the-whole-reservation-FIXED-20260930.md`).

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
> [`gengc-plumbing-vestigial-semispace-heap-RETIRED-20260923`](internal/gc/gengc-plumbing-vestigial-semispace-heap-RETIRED-20260923.md)
> rather than done unilaterally, because it still backs several of `gc.rs`'s and
> `collector.rs`'s trait-level doctests and examples.

### Explicit GC: `-XX:±DisableExplicitGC`, `-XX:±ExplicitGCInvokesConcurrent`

Per VM (`VmConfig::explicit_gc`), and both are off by default, as in HotSpot.
An explicit GC is `System.gc()`, `Runtime.gc()`, `MemoryMXBean.gc()`, or the
`System.gc()` inside `java.nio.Bits.reserveMemory`. `jcmd GC.run`, the
`-XX:MaxMetaspaceSize` refusal's collection (reported as `Metadata GC
Threshold`, `door=metadata`), VM-internal collections, and every
allocation-driven collection are not explicit GCs, and neither flag affects
them.

| Flag | Effect |
|---|---|
| `-XX:+DisableExplicitGC` | An explicit GC does nothing: no collection, no finalizer, cleaner or notification drain, and no `jdk.SystemGC` event. It wins over the flag below. With it, direct-buffer exhaustion surfaces as `OutOfMemoryError: Direct buffer memory` as it does on HotSpot, instead of being rescued by a forced collection. |
| `-XX:+ExplicitGCInvokesConcurrent` | Accepted. It collects exactly as the default does, on every backend. Only the `invokedConcurrent` field of JFR `jdk.SystemGC` changes. On HotSpot the flag replaces G1's stop-the-world full compaction with a young pause plus a marking cycle the caller waits to finish; CratonVM's G1 `System.gc()` already works that way. HotSpot ignores the flag on Serial/Parallel (the Generational backend's model), and ZGC's `System.gc()` is its whole-heap cycle either way. |

## Concurrent vs STW

**G1 is the exception to this section.** Its concurrent mark cycle is not
optional and not wired through the API below: it is SATB tri-colour marking
with a background worker, driven from `g1_concurrent_mark_cycle`
(`vm/src/runtime/interpreter/gc_and_alloc.rs`; STW initial mark -> concurrent
trace -> STW remark -> cleanup), and it is what arms mixed collections. G1's
*evacuation* is stop-the-world; its *marking* is not.

**Generational's young collections are stop-the-world; its old generation is
concurrent-first.** This section used to say the whole collector was
stop-the-world unless an embedder called `enable_concurrent_gc`. That has not
been true for some time: `SharedVm::new` (`vm/src/vm/vm_init.rs`) calls
`VmHeap::enable_concurrent_gc` on every VM, wiring the SATB queue and the
tri-colour mark state ([`gc/src/concurrent_mark.rs`](../gc/src/concurrent_mark.rs))
into the heap, and since 2026-09-24 the default old-gen policy
(`OldGenPolicy::ConcurrentFirst`) opens a concurrent old-gen mark cycle below
the stop-the-world floor and makes the stop-the-world old-gen collection defer
to it (see the "old-gen triggers" row above). The cycle is an initial-mark
pause, a marking phase sliced into 32 Ki-object steps that release the old-gen
lock and poll for a safepoint between slices (`CRATONVM_GEN_CONC_MARK_SLICE`;
`CRATONVM_GC=-gen-conc-mark-slice` restores one lock hold), a remark pause and
a sweep. It runs on the mutator whose young pause found it due, unless
`CRATONVM_GEN_CONC_SERVICE_THREAD` moves it to a service thread.

The trade-off:

- **Stop-the-world old-gen collection** (`CRATONVM_GC=-concurrent-first`, the
  legacy policy): simpler, predictable. The old-gen part of a pause is
  proportional to the old live set.
- **Concurrent-first (default)**: two short STW pauses (initial-mark + remark)
  plus a marking interval during which mutators pay the SATB pre-store
  barrier (a log write per reference store while a cycle is open). Shorter
  pauses, more total work. The stop-the-world collection still runs when the
  cycle cannot keep up (90 % full), on an allocation failure and on
  `System.gc()`.

## Finalizer and reference delivery

Every collection door QUEUES what it found dead the same way on all three
backends — `finalize()` candidates on the VM's finalizer queue, cleaner actions
and `ReferenceQueue` enqueues on theirs. Who then RUNS that Java code is one
per-VM policy, `CRATONVM_FINALIZER_THREAD` (grouped spelling
`CRATONVM_GC=finalizer-thread`), latched at first use
(`vm/src/runtime/interpreter/gc_and_alloc.rs`, `FinalizerDelivery`):

| setting | `finalize()` | cleaner actions, `ReferenceQueue` wake-ups, GC notifications | `System.gc()` / `Runtime.runFinalization()` |
|---|---|---|---|
| unset (default since 2026-09-23, `WhenLockHeld`) | inline on the thread that drains the queue, EXCEPT a thread holding a user-visible lock (a monitor, or an `AbstractOwnableSynchronizer` such as `ReentrantLock`) — that thread hands the queue to the per-VM daemon "Craton Finalizer", started on first such use (JLS §12.6: the finalizing thread holds no user-visible locks) | inline, as before | a lock-free caller drains inline; a lock-holding caller waits for the daemon (at most 2 s, and not at all once the daemon is seen blocked on a monitor the caller owns) and then returns, run or not — it never runs `finalize()` itself |
| `=1` (or any value but the off words) | always handed to the daemon | always handed to the daemon | waits for the daemon (same bound); a lock-free caller whose wait times out drains inline |
| `=0` / `false` / `off` / `no` | inline everywhere (the pre-2026-09-23 behaviour; for bisection) | inline | inline |

The unset row is per VM mode. Under `--jdk-only` (the launcher's default mode)
the daemon ALSO delivers `ReferenceQueue` wake-ups (started the first time a
collection links a `Reference` into a queue) and GC notifications, so no
`NotificationListener` ever runs on an application thread; once it exists, a
native `ReferenceQueue.remove` sleeps on the queue's `lock` (in slices of at
most 1 s) instead of polling. Cleaner actions stay inline in both modes.
`--compatible` is exactly the row above.

What changes for an application:

* **Default.** A program that never finalizes while holding a lock sees no
  difference from `=0`. One that does — typically a `synchronized` method
  that allocates, triggering a collection whose drain would have run a
  `finalize()` re-entrantly INSIDE that critical section (monitors are
  re-entrant) — now gets the finalizer on the daemon instead. A
  `synchronized (L) { … System.gc(); … }` block may therefore return from
  `System.gc()` before the finalizers it queued have run; HotSpot never runs
  them on the caller either.
* **`=1`.** Finalizers, cleaners and queue wake-ups never run on an
  allocating mutator (HotSpot's shape: a `Finalizer` thread and a
  `ReferenceHandler`). The allocation doors run nothing; `System.gc()` keeps
  its "finalizers have run when I return" ordering by waiting, bounded. Not
  the default yet
  ([`common-w4d-proposal-full-reference-delivery-by-default-REJECTED-20260928`](internal/gc/common-w4d-proposal-full-reference-delivery-by-default-REJECTED-20260928.md)).
* The daemon is idle GC-blocked (never counted by a pause), holds only a weak
  handle to its VM, and is never waited for at exit.
* **`--jdk-only` (since 2026-09-24).** With the setting unset, the
  `ReferenceQueue` wake-ups of every collection are handed to the daemon as
  under `=1` — the JDK's `ReferenceQueue.remove()` waits on its lock, which
  no collector splice notifies — while `finalize()` keeps the default rule
  above. `Reference.waitForReferenceProcessing()` waits (bounded) for the
  daemon under `=1` and answers `false` otherwise. `--compatible` records no
  wake-ups and is unchanged.

`--compatible` output is identical to `=0` except where a finalizer used to
run under the caller's lock.

> **Known regression, 2026-09-26.** On current `dev`, `--compatible` runs
> deliver **no** GC notifications to a `NotificationListener`
> (`GcNotificationThreadProbe` prints `notifications=0` on all three
> backends; `--jdk-only` is unaffected). It came in with the dev merge that
> brought the 8-byte header and is not bisected yet:
> `docs/known-issues/gc/common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md`.

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

> **JFR GC events, as of 2026-09-23.** Until then only ONE of the six VM
> collection doors emitted, with `gcId` the literal `1`, the name `"YoungGC"`,
> the cause always `"Allocation Failure"`, G1's tenuring threshold `15`, and a
> heap summary with `committed = used`, `max = 2 × used`
> ([`gengc-plumbing-jfr-gc-events-are-constants-FIXED-20260923`](internal/gc/gengc-plumbing-jfr-gc-events-are-constants-FIXED-20260923.md)).
> Every door now goes through `vm/src/runtime/interpreter/gc_events.rs`:
>
> | event type | emitted | fields |
> |---|---|---|
> | `jdk.GarbageCollection` | every collection, all three doors | `gcId` = the VM's collection ordinal; name `DefNew`/`SerialOld` (generational), `G1New`, `Z`; cause `Allocation Failure` or `System.gc()` |
> | `jdk.YoungGarbageCollection` | generational young cycles, G1 pauses | tenuring threshold from `VmHeap::tenuring_threshold` (generational: 3; G1: its adaptive threshold — the literal `15` until 2026-09-23, gc-common w3-f) |
> | `jdk.OldGarbageCollection` | a generational cycle that reclaimed old gen | — |
> | `jdk.GCHeapSummary` | `Before GC` and `After GC`, every collection | whole-heap used, real committed, `-Xmx` max |
> | `jdk.GCPhasePause` | generational collections, while a recording runs | the collector's phase marks laid out consecutively from the pause start, plus an `other` row for the residual (gen r4w2/obs). G1 and ZGC: none yet; concurrent cycles emit nothing (`docs/internal/gaps/gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted-20260923.md`) |
>
> Duration is the initiator's pause (roots + collection + reference processing
> + remap), measured from the moment the world is stopped. Concurrent mark
> cycles (ZGC concurrent mark, G1 marking) emit nothing yet.

`-Xlog:gc` (any rule whose tags include `gc` at `info` or finer) now prints one
HotSpot-shaped line per collection through the unified logger, e.g.
`GC(3) Pause Young (Allocation Failure) 24M->3M(96M) 2.345ms`. Until
2026-09-23 the logger was initialised and nothing ever logged to it.
`--verbose:gc` is unchanged: it still prints CratonVM's own
`[GC] generational: …` line, not the HotSpot format. Since 2026-09-23 it also
prints, on every backend, one `[GC] pause: gc=N door=… ttsp_us=… straggler=…
xt_pass_us=… xt_post_quota_us=… collect_us=… xt_passes=… xt_frozen=… …` line per
collection: `ttsp_us` is that pause's time-to-safepoint, `collect_us` the
collection itself; `straggler` the thread the pause waited for last. Remarks,
initial marks, stack-trace, heap-dump and loop-exit pauses print the same line with
`gc=-` and `work_us=`, so a slow pause in the exit aggregate can be
attributed to its kind; `xt_post_quota_us` the cross-thread take-over's work
between the two (large only when a peer was frozen in compiled code or a
blocked peer had JIT frames; a few microseconds of skip-span publishing
otherwise). The three add up to the pause, request to release. `xt_pass_us`
is the take-over passes' share of `ttsp_us` (part of it, not added to it), so
`ttsp_us - xt_pass_us` is the wait for threads that had to reach a poll. A pause that is slow
because one thread took long to reach a safepoint shows a large `ttsp_us` and
a normal `collect_us`; the exit `[GC] ttsp:` line is the run's aggregate
over every stop-the-world pause (non-collection pauses included), and its
`max_straggler=` names the thread the slowest pause waited for last (a
registry `ThreadId`; `none` when no participating thread was last — see
`docs/GC.md`). The pause line's `gc=N` is the `GC(N)` of the `-Xlog:gc` line
and the JFR `gcId`.

**GC overhead limit** (`UseGCOverheadLimit`'s counterpart,
`CRATONVM_GC_OVERHEAD_LIMIT=0` to disable). Once `GC_OVERHEAD_LIMIT_CYCLES`
(8) collections in a row were each unproductive *while the old generation
could not absorb 2 % of the heap either*, a failed allocation clears the soft
references, retries once and then throws `OutOfMemoryError` without the
last-ditch ladder (G1 and ZGC). On Generational that exit runs one
old-generation collection first, as Serial runs a full collection before
every error, so a program that dropped its data after an earlier
`OutOfMemoryError` gets the memory back; since 2026-09-24 the error follows
that collection directly when it leaves the streak latched. A collection is
unproductive when it freed less than 2 % of the heap capacity, or — on
Generational only, since 2026-09-24 — when the mutators allocated less than
2 % of the heap since the previous forced collection and the live set did not
fall by 2 % (a heap thrashing on TLAB filler frees a little every cycle while
the program makes no progress; `gc_cycle_is_unproductive`).
`CRATONVM_GC_OVERHEAD_PROGRESS=0` removes the progress test and restores the
retry after the old-generation collection. "In a row" counts only the collections run for
a FAILED allocation (the forced door): a productive forced collection resets
the streak, but the threshold-triggered young collections between two forced
ones neither reset nor advance it. That is a decision, not an oversight
(gc-common round 2026-09-23): in the generational death spiral the
interleaved young collections are exactly the ones that look productive while
the old generation stays wedged. It also differs from HotSpot's criterion
(98 % of time in GC): it measures reclamation only, not time.

Start a recording with the standard `JFR.start` JMX command. Dump with
`JFR.dump`. The recording can be opened in JDK Mission Control or any
JFR-aware analyser. (`-Xlog:gc*=info:stdout:time,level,tags` is a **unified
logging** spec, parsed by `vm/src/runtime/unified_logging.rs`; it does not
start a JFR recording.)

For an in-process view:

- `--verbose:gc` prints a one-line summary per collection to stderr on all
  three backends (see [`docs/CONFIG.md`](CONFIG.md)). **On Generational this is
  true only since 2026-09-20**: before that the flag logged an affirmative and
  enabled nothing, so the run produced no per-collection output at all — the
  shutdown `[GC] …` census did still print. See
  [`gengc-plumbing-verbose-gc-does-nothing-FIXED-20260923`](internal/gc/gengc-plumbing-verbose-gc-does-nothing-FIXED-20260923.md).
- `GenerationalHeap::stats().snapshot()` returns a `HeapStatsSnapshot`
  ([`gc/src/gen_heap.rs`](../gc/src/gen_heap.rs)): minor and major
  collection counts, bytes and objects copied and promoted, old-gen bytes
  freed, young and old allocation counts, and the zeroing and TLAB-tail
  counters of the allocation switches above. It has no pause histogram and no
  card-scan counts; pauses are on the `[GC] pause:` lines and the exit
  census.
- Allocation-rate spikes show up as a faster-growing `young_allocations` /
  `old_allocations` between two snapshots, and as a shorter interval between
  `[GC] pause:` lines.

In `release` builds, `tracing::debug!` and `tracing::trace!` calls compile out
(workspace `release_max_level_info` lint). **The production diagnostic channel
is `--verbose:gc` plus the `[GC] …` shutdown census, not JFR** — this paragraph
ended "the JFR path is your production diagnostic channel" until 2026-09-20,
which is the opposite of what the boxed audit above establishes. Every `[GC]`
line is an unconditional `eprintln!` and survives a release build.

## Common symptoms and remedies

| Symptom | Likely cause | First thing to try |
|---|---|---|
| Long minor-GC pauses (50 ms+) | Young space too large, or too many surviving objects | Shrink the young generation (`-Xmn`, `-XX:NewRatio`) *or* lower `-XX:MaxTenuringThreshold` so survivors leave the young set sooner (Generational). |
| Frequent minor GCs (multiple per second) | Young space too small, or allocation storm | Grow the young generation (`-Xmn`, `-XX:NewRatio`). Confirm with the `young_allocations` rate. |
| Allocation stall / `OutOfMemoryError` under steady-state load | Old gen filling without reclaim | Switch to G1 (better old-gen compaction; not for a workload of large arrays near `-Xmx`, see "Open G1 defects"), or bump `max_heap_size`. Check for retention leaks via HPROF dump (`-XX:+HeapDumpOnOutOfMemoryError`). Under G1 specifically, read `[GC] g1 ihop:` at exit first: if `threshold` was never crossed, no concurrent mark cycle ran, so no mixed collection ran and the old generation was never reclaimed at all — lower `-XX:InitiatingHeapOccupancyPercent`. |
| Long pauses every Nth collection | A stop-the-world old-gen collection inside a young pause (Generational), because the concurrent cycle could not finish before the generation reached 90 %, an old-gen allocation failed, or the legacy policy is selected | Raise `-Xmx`; check `[GC] major_cadence:` for the cause; lower the concurrent start (`CRATONVM_GC=conc-start-percent=<n>`) so the cycle opens earlier. On G1, read `[GC] g1 ihop:` as in the row above. |
| Steady RSS growth over hours/days | Slow-burn retention leak, or a backend that does not give memory back | Generational decommits the old generation's free tail after each stop-the-world old-gen collection (default ON); G1 only with `CRATONVM_G1_UNCOMMIT=1` or `CRATONVM_G1_HEAP_RESIZE=1`; ZGC every cycle. A VM-built Generational young generation never grows past its initial size, so it is not the cause. Otherwise HPROF dump and look for reference cycles outside collectable roots. |
| Throughput regression after enabling `gpu-offload` | GC deferred by a long-lived `SafepointToken` | Break GPU work into shorter kernels; observe the live-token count with `VmHeap::gpu_critical_count()`. |
| Per-object `RemSet` overhead in G1 | Cross-region write storm | Raise `-XX:G1HeapRegionSize` (the ergonomic already picks 1 MiB below a 2 GiB heap) to reduce the number of distinct regions an old object can point into. Check `[g1-accessor] rset ... memo_hit_pct` under `CRATONVM_DBG=g1accessor` (legacy `CRATONVM_DBG_G1ACCESSOR=1`; this row said `CRATONVM_G1_DBG_ACCESSOR`, a name nothing reads, until 2026-09-23) first: a high hit rate means the barrier's per-thread edge memo is already absorbing the storm and the region size is not the problem. |

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
