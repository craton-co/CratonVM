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
> **Parallel marking is on by default since zr z3/mt (2026-10-12).** Unset
> `CRATONVM_ZGC_PARMARK` means `auto`: HotSpot ZGC's pause-worker ergonomic
> (60 % of the CPUs rounded up, capped at 2 % of the heap in 2 MiB pages, so a
> small heap still marks serially), or `-XX:ParallelGCThreads=<n>` when given
> (`1` = serial). Measured on the host (z2b rows, `Zr2CcParmarkShapes` and
> `Zr1MkParmarkCost`): about **2.5x lower mark pause and 2x lower wall for
> +15-20 % CPU**. `CRATONVM_ZGC_PARMARK=0` (or `CRATONVM_GC=-zgc-parmark`)
> restores the serial loop. (It was off from 2026-08-14 on a +31 % / +153 %
> pause measurement of the engine of that time, before claim-on-push, the
> persistent pool, the inline driver and the idle spill.)
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
| How do I get a build with no ZGC? | `--no-default-features` (name `mimalloc` back if you still want it on non-Windows targets). Generational becomes the default there and `-XX:+UseZGC` warns and falls back | `vm-cli/Cargo.toml` |
| How do I switch collector at runtime? | `-XX:+UseGenerationalGC` (or `-XX:-UseZGC`); `-XX:+UseG1GC` for G1. Available in every build | `parse_gc_algorithm`, `vm/src/config.rs` |
| Does ZGC move objects? | **Yes, by default — and each cycle decides again.** After the sweep, a stop-the-world slide compacts the small-object end of the arena and returns a **non-empty** `PointerMap`; `CRATONVM_ZGC_RELOCATE=0` (`off`/`false`/`no`) restores the non-moving behaviour byte for byte, so an A/B is a re-run and not a rebuild. Whole-heap and stop-the-world remain the defaults, and it is non-generational unless `CRATONVM_ZGC_GENERATIONAL=1`. **"Default-on" is not "it happened":** `relocate_stw_admitted` re-decides per collection and may decline — on the per-cycle JIT coverage proof (`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` restores the older blanket refusal: compact only when no compiled frame is live) or on the cost gate, which declines a low slide whose selected garbage is under 1/32 of the low region's live bytes (`CRATONVM_ZGC_RELOCATE_COST_GATE=0`; default-on since round 9 wave 9). Check `moved=` on `[GC] zgc-relocate:` before concluding a cycle compacted; `live=0 moved=0` is a refusal. **This row said "No. Non-moving" for five weeks after the default moved** (default-on 2026-08-13, row corrected 2026-09-20), which is the exact reading that makes a stale-reference crash look impossible | `ZgcRealHeap::relocation_requested_by_default` and `::relocate_stw_admitted`, `gc/src/zgc.rs` |
| Does ZGC have TLABs? | **Yes, both (default-on since 2026-09-18).** The VM-thread buffer (`CRATONVM_ZGC_JIT_TLAB`, `=0` turns it off) makes `VmHeap::refill_tlab` hand the VM thread's own buffer a chunk, so the interpreter's `new` and the JIT's inline bump both hit (bintrees ~6x faster than the helper path; see `ZGC_VM_TLAB_DEFAULT_ON`); the backend keeps its own buffer under that for the helper and native paths (`CRATONVM_ZGC_TLAB=0`, which switches both off — they carve from one source). Engagement: `vm_tlab_refills` on the `[GC] zgc-features:` line. **Chunk sizing counts the VM-thread buffer since 2026-09-20** (`CRATONVM_ZGC_VM_TLAB_SHARE=0` restores the old arithmetic): the one sizing entry point divides a sixteenth of the arena by the buffers claiming one, and that divisor used to count only the backend's own cells — so once the VM buffer became default-on and carried nearly all allocation without registering a cell, **every thread was sized as if it were alone**. That is the Tomcat `TestNonBlockingAPI` defect verbatim: ~4 000 threads × 512 KiB is the whole heap, 1.99 GB of it on the free list in pieces, and a 2 MB `char[]` with nowhere to go. Below ~256 buffers the 512 KiB clamp decides and the divisor is inert, so on a small thread count the switch changes nothing measurable | `gc/src/zgc/vm_tlab.rs`, `ZArenaTlabRegistry::chunk_bytes_now`, `ZgcRealHeap::alloc_raw_tlab` |
| Is it concurrent, generational or compacting? | **Concurrent: OPT-IN (`CRATONVM_ZGC_CONC_START=60`).** The strong closure is traced by a worker pool while every mutator runs; the pause replays the SATB ingress, re-scans the roots and sweeps. Measured per-cycle pause **-38% to -58%**, wall clock **+37% to +55%**, and roughly **twice as many cycles** (floating garbage) -- so it is off unless asked for. **Compacting: YES, on by default** (`CRATONVM_ZGC_RELOCATE=0` is the kill switch). **Generational: OPT-IN (`CRATONVM_ZGC_GENERATIONAL=1`).** Every collection between two whole-heap ones is a young cycle: the old generation is pre-marked and never traced, and the remembered set supplies the old-to-young roots. The split is by **object** age, not page age | [the concurrent+generational plan](internal/zgc-round-20260920/zgc-concurrent-and-generational-plan-20260813.md) |
| How do I tell whether a collection marked concurrently? | `--verbose:gc`'s **`mark=`** field on each `[GC] zgc-real:` line — `concurrent`, `stw-parallel` or `stw-serial`. At shutdown, `[GC] zgc-concurrent:` gives `cycles_started` / `cycles_completed` / `black_allocations` / `satb_replayed` / `concurrent_phase_ms`. **`cycles_started=0` means the run says nothing about concurrent marking**, which is a different fact from concurrent marking not helping | `ZgcRealHeap::collect_garbage`, `VmHeap::print_gc_summary` |
| When does a concurrent cycle open? | When allocation crosses `CRATONVM_ZGC_CONC_START`% of the collection threshold (default `0`, i.e. never). `=60` is the value the 2026-08-16 measurement was taken at and the one to opt in with. **`CRATONVM_ZGC_CONC_START=auto`** is a third setting, not a boolean: it seeds the window at 60 and then lets `refresh_adaptive_conc_start` move it from each cycle's own measurements, which is the answer to "a fixed percentage is why concurrent marking loses on *total* pause while winning 38-58% per cycle". **A `System.gc()`-driven workload never opens one** — a forced collection is meant to collect now, not to start marking — so a benchmark built out of `System.gc()` calls measures the stop-the-world path however this is configured | `ZgcRealHeap::should_start_concurrent_mark`, `conc_start_percent_setting` |
| Do young cycles actually happen? | `CRATONVM_ZGC_GEN_NURSERY_PERCENT` (default 10, `0` disables it) adds a nursery-size clause to `needs_gc`, so a young cycle can fire on its own rather than only when a whole-heap collection would have. `[GC] zgc-nursery-trigger:`'s **`fired=`** is the engagement counter — a **separate line** from `[GC] zgc-nursery:`, which carries `sweep_skipped` / `floor` / `old_live_bytes` and no trigger count at all: **zero on a generational run means every collection still came from the whole-heap predicate**. The clause bypasses `gc_rearm` deliberately (that floor is a quarter of remaining headroom and would make it unreachable); it cannot storm because the watermark resets on every collection | `ZgcRealHeap::needs_gc` |
| Is the young cycle's SWEEP actually bounded? | `--verbose:gc`'s `gen=young/N **swept=A/B**` field: `A` is the registered objects the sweep walked, `B` the whole registry. `A == B` means the nursery floor never moved and the sweep is O(registry) — the state the first Phase G measurement was in. At shutdown, `[GC] zgc-nursery:` gives `sweep_skipped` / `floor` / `old_live_bytes`; **`sweep_skipped=0` with `young_cycles>0` is the inert state**. The floor is the arena cursor the last WHOLE-HEAP collection ended on, so a young cycle sweeps only what the bump cursor served since then — an object the free list placed below the floor waits for a major (over-retention, never unsoundness) | `ZgcRealHeap::gen_young_floor` |
| How do I tell whether a collection was a young one? | `--verbose:gc`'s **`gen=`** field on each `[GC] zgc-real:` line: `young/N` (a minor, where `N` is the objects it retained **without tracing** — the work it did not do), `major/+N` (whole-heap, `N` objects promoted) or `off`. At shutdown, `[GC] zgc-generational:` gives `enabled` / `young_cycles` / `minors_since_major` / `old_retained` / `remembered_roots` / `promotions` / `recards_after_relocation`, printed unconditionally so a run that never engaged says so rather than printing nothing. **`young_cycles>0` with `old_retained=0` means the run did full-heap work under a generational name** — which is the vacuous green a "generational is on" claim would otherwise rest on | `ZgcRealHeap::collect_garbage`, `VmHeap::print_gc_summary` |
| When is a collection forced to be whole-heap? | **Six cases**, and each is a refusal rather than a policy: the mark set came from a **concurrent** cycle (it *is* the whole-heap closure and cannot be scoped after the fact); the collection was driven by **hard allocation failure** (`hard_alloc_failure` — a young cycle retains the whole old generation unexamined, so it is the wrong tool for "the heap is full"; **`headroom_low` is deliberately *not* one of these**, and the code says so in as many words); a previous young cycle **reclaimed nothing** (`gen_force_major_next` — an escalation latch, and the reason a generational run can show a major it did not ask for); the `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` ceiling (default 8) has been reached; **nothing has been promoted yet** (`has_old_objects`), in which case a young cycle would be a full one anyway; or the **young page set is incomplete** (`young_pages_are_complete` — an allocation that landed off the page grid, which fails closed, because an unrecorded page is retained until a major and therefore forever if majors are rare). Check `young_cycles` before concluding anything. **This row named four cases and listed `headroom_low` among them until 2026-09-20**, which sent an operator chasing `young_cycles=0` on a small heap to a knob that has nothing to do with it | `ZgcRealHeap::collect_garbage`, the `force_major` / `young_cycle` pair in PHASE G |
| What does a sweep do per dead object? | It zeroes the object's first `HEADER_SIZE` (16) bytes only (`CRATONVM_ZGC_SWEEP_HEADER_ZERO=0` restores a full-body memset, which is redundant with the memset `alloc_raw` already does on every handout) and hands over one free-list span per **run** of adjacent dead objects rather than one span per object (`CRATONVM_ZGC_SWEEP_DEAD_RUNS=0` restores the per-object calls, each of which then needs sorting in `coalesce_free_list`). **Both apply to every cycle**, and the saving is on the whole-heap one: a young cycle's sweep is 6.3 ms of a 112 ms pause where a whole-heap cycle's is 170 ms of 265 ms, because it walks 13.0M dead objects against a young cycle's 137k. They were scoped to young cycles when they landed on 2026-08-17 — a mid-gauntlet caution that has since expired — and the restriction pointed the change away from the cost. Engagement on `[GC] zgc-sweep-cost:`: **`zero_bytes_skipped=0` with collections having run means the first is on and inert, and `dead_runs == dead_objects` means the second is**. The flags were `CRATONVM_ZGC_GEN_*` before the scoping was removed; **the old names are dead and set silently to nothing** | `ZgcRealHeap::collect_garbage`'s `ZSweepCfg`, `zgc_sweep_header_zero` / `zgc_sweep_dead_runs` |
| Is it safe to stop zeroing a dead object's body? | Yes, and the reason is that the header is the whole of the property. The sweep zeroed to stop "a later scan seeing a stale header". Since the 8-byte header (2026-09-24) every object starts with one header word (class id and mark word) and is at least `MIN_OBJECT_SIZE` (16) bytes; arrays and legacy instances carry a second shape/aux word, a compact instance has its first field there. Zeroing the first 16 bytes therefore clears the whole header of every object, and the result -- class id 0, a mark word with no `COMPACT` flag, shape 0 -- is the same `class_id=0, num_slots=0` corpse a reader of a vacated span always saw. `ARRAY_DATA_OFFSET == HEADER_SIZE`, so an array's length is still in the shape word, not a body prefix. The body is only reachable **through** that header: field reads size the object from its shape or its class's compact layout, extent walks from `alloc_size(header)`, membership from the registry the sweep just removed the base from. And reuse was never the reason -- `alloc_raw` and `tlab_refill` memset unconditionally | `ObjectHeader`, `ZgcRealHeap::alloc_raw` |
| Should I turn generational on? | **No, not for CPU (zr round, 2026-10-12).** On `Zr1GnGenShape 64 400 200000 5 8 64` at `-Xmx1g` generational cost **12.6 s** CPU against the default's **6.3 s** (10.8 s with the young slide refused); HotSpot's generational ZGC ran 0 minors and 3 majors there. By the allocation volume (an estimate pending the z2 host rows) the mode runs ~36 cycles where the default runs ~5-6, and its minor-count ceiling still schedules as many whole-heap cycles as the default -- see `docs/known-issues/gc/zgc-zr1gn-proposal-generational-direction-20261012.md`. Defaults INSIDE the mode since z2/g2: old-start bitmap ON (`CRATONVM_ZGC_GEN_OLD_BITMAP=0` restores the header walk; since z3/rv3 a young cycle the bitmap cannot serve runs whole-heap instead of falling back, `CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR=0` restores the fallback), young slide OFF (`CRATONVM_ZGC_GEN_YOUNG_RELOCATE=1`), nursery census OFF (`CRATONVM_ZGC_NURSERY_CENSUS=1`), JIT inline start-bit store open (`CRATONVM_ZGC_GEN_INLINE_ANNOUNCE=0`); opt-in: `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT=1`, `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT=<p>`, `CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`. `[GC] zgc-gen-cost:` (with `--verbose:gc`) splits the collector time by cycle kind. The earlier measurement follows. On the shape it is for — 800k retained objects, 48M allocated, an old-to-young store per round — it is **neutral at the default promotion age (+2.5% to +4% total pause) and clearly worse at age 1 (+46% to +55%, reclaim 75.7% → 63.7%)**, with wall clock flat. The split works (4.8M objects skipped per young cycle, every old-to-young edge intact) and does not pay, because `sweep` is 182 ms of a 309 ms pause and walks every registered object whatever the split says. A real young space, reclaimed by resetting a cursor, is what would change that. If you try it anyway: `CRATONVM_ZGC_GEN_PROMOTION_AGE` (default 3, clamped to `1..=15` because the header field is 4 bits) decides how many collections an object must survive; a lower value promotes sooner, so young cycles skip more and old garbage accumulates faster. Measure with `gen=` and `old_retained`, and note that **a lower promotion age is not a stronger version of the same knob** — age 1 promoted 11.7M objects, filling old with garbage no young cycle examines | the plan's §3 |
| Should I turn concurrent marking on? | If your workload has **many mutator threads, a large live set, and a pause budget you are missing**. It trades throughput for pause and the trade is not small; measure your own workload with `--verbose:gc` and compare `mark=concurrent` cycles against `CRATONVM_ZGC_CONC_START=0`. `-XX:ConcGCThreads=<n>` (since zr z3/mt; wins over the env knob) or `CRATONVM_ZGC_CONC_WORKERS` (an explicit value is clamped to `1..=32`) is the second knob; the default is HotSpot ZGC's ergonomic, 25 % of the CPUs rounded up (12.5 % with `-XX:-UseDynamicNumberOfGCThreads`), capped at 2 % of the heap in 2 MiB pages (`CRATONVM_ZGC_GC_THREAD_FLAGS=0` restores the old `cores/4`, floor 1, cap 4) — 1 worker measured best on a single-threaded probe, 2 on an 8-thread one. Deliberately a different knob from the stop-the-world mark pool: that one wants every core because nothing else is running, this one is stealing cycles from the mutators beside it. `-XX:ParallelGCThreads=<n>` (zr z3/mt) sizes ZGC's stop-the-world pools that are on (the parallel mark when `CRATONVM_ZGC_PARMARK` selects it, the compaction passes, the parallel sweep when `CRATONVM_ZGC_PARSWEEP` turns it on); `1` makes them all serial | the plan's §2b |
| Why did a collection run? | `[GC] zgc-trigger:` carries four reason tallies plus `hard_alloc_refusals` — `stress` (`CRATONVM_DBG_GC_STRESS`), `live_bytes_threshold` (75% of `-Xmx`), `headroom_low` (the arena cannot serve a `zgc_headroom_margin` request) and `alloc_budget` (`CRATONVM_ZGC_ALLOC_TRIGGER`, percent of capacity allocated since the last cycle, **default 0 = off** — the implicit floor a pause target used to supply was withdrawn on 2026-09-04, see the flag notes below). They are not exclusive: a cycle can be charged to the threshold *and* to headroom, which is the whole question when two arms differ in collection **count**. **As of 2026-09-20 they count collections; before that they counted agreeing polls of `needs_gc`** — one per native boundary — so any archived comparison of these numbers against a current run is meaningless, not merely rescaled | `ZgcRealHeap::needs_gc`, `ZgcCounters::trigger_seen` |
| Can I measure the reference-slot mix? | `CRATONVM_ZGC_CENSUS=1` turns on the sweep-time walk that answers "what fraction of live reference slots are legacy 16-byte cells?", and `CRATONVM_ZGC_CENSUS_ACCESS=1` adds the per-access arm (and implies the first). The summary prints when the heap is dropped, and only if the census was enabled. **It was unreachable before 2026-09-20** — `ZSlotCensus::enable()` had no caller outside its own tests, so `is_enabled()` was `false` for the life of every process and a 2 900-line instrument had never run. Read `legacy_share` off the **`last_walk`** block, not the cumulative `walk` one: the cumulative figure is lifetime-weighted and reads high on legacy. A `VERDICT=no_data` row is the instrument saying so, and is a different fact from "no legacy slots" | `ZSlotCensus::enable_from_env`, `gc/src/zgc/census.rs` |
| What does it cost me? | Headroom, and less of it than this row used to claim. It said "**not** compacting means free memory can be plentiful and still too broken up to serve one large array" — written when the slide was off. The slide is on, so the steady state is defragmented; what remains is that **a cycle may decline to compact** (the cost gate, the JIT coverage proof, `CRATONVM_ZGC_RELOCATE=0`), and that a declined cycle leaves exactly the fragmentation the sentence describes, with `headroom_low` as the symptom and an aborting `alloc_object` as the endgame. The large-object end is compacted separately (`CRATONVM_ZGC_HIGH_COMPACTION=0` is its kill switch) and reserves `capacity / 8` | the sizing notes below; `[GC] zgc-relocate:`'s `moved=` |

| Backend | Module | Status | Best for |
|---|---|---|---|
| **ZGC** (default) | [`gc/src/zgc.rs`](../gc/src/zgc.rs) | Default, and still **not a real ZGC** | Most workloads, on the suite evidence above. Compacting by default, concurrent marking and generational mode opt-in, one arena. Still not OpenJDK ZGC: it marks under a **pre-write** barrier (snapshot-at-the-beginning), not under ZGC's load barrier, so it is conservative about objects that die mid-cycle, and relocation is stop-the-world. Fewest hangs and zero crashes across the Tomcat suite. See [the maturity assessment](internal/zgc-round-20260920/zgc-maturity-assessment-and-plan-20260813.md) for what is and is not built, and the plan to close it. |
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
  see [the maturity assessment](internal/zgc-round-20260920/zgc-maturity-assessment-and-plan-20260813.md)
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
| `CRATONVM_ZGC_TRIGGER_SHADOW` | **OFF** | Print one `[GC] zgc-trigger-shadow:` line per collection showing what an allocation-rate-driven trigger *would* have decided — `rate_bps`, `free_bytes`, `largest_bytes`, `cycle_us`, `ttl_ms`, `ratio`, `affordable_span`, and `fired=` naming which legacy `needs_gc` clauses actually asked (or `none` for a `System.gc()` or a hard refusal that bypassed the poll). Pure observation; costs one arena lock per collection. **Read `ratio`'s stability between adjacent cycles, not its value** — a projection that swings by an order of magnitude between neighbours refutes the direction. *zr z1/tr (2026-10-12):* the line also carries the HotSpot-shaped director's view in either arm -- `used_at_start`, `hs_first_yes=<rule>@<bytes>` (where its rules first said yes in the interval), `hs_rate_avg_bps` / `hs_rate_sd_bps`, `hs_cycle_avg_us` / `hs_cycle_sd_us`, `hs_time_until_gc_ms` -- and `fired=` now names `warmup`, `nursery` and the `hs_*` rules (a warm-up cycle used to print `fired=none`). |
| `CRATONVM_ZGC_RATE_TRIGGER` | **OFF** | *zr z1/tr (2026-10-12).* HotSpot ZGC's director rules decide when to collect (`gc/src/zgc/director.rs`): **allocation rate** (worst-case rate = 2 x the 1 s window mean + 3.29 sd; collect when free space under the soft max no longer covers it for one cycle -- decaying mean + 3.29 sd of measured cycles -- plus 100 ms), **proactive** (warm, grown 10% of the soft max or 5 min idle, and past the 1%-throughput interval) and **high usage** (free <= 5% of the soft max). The 75%-of-`-Xmx` live-bytes clause stands down; headroom, the pause-target budget, stress, nursery, warm-up and the hard allocation refusal are unchanged. Evaluated as a byte threshold, so a poll costs two relaxed loads and a compare. Collections it admits are tallied as `director` and named `Allocation Rate` / `Proactive` / `High Usage` on `-Xlog:gc` and notifications. Flip criteria: `docs/internal/gc/zgc-zr1tr-proposal-rate-trigger-flip-20261012.md`. |
| `CRATONVM_ZGC_HOTSPOT_GC_CAUSE` | **on** (`=0` kill) | *zr z1/tr (2026-10-12).* An allocation-driven ZGC collection is reported under HotSpot 25 ZGC's `GCCause` on `-Xlog:gc` (`Major Collection (<cause>)`), `GarbageCollectionNotificationInfo.getGcCause()` and JFR `jdk.GarbageCollection.cause`: `Warmup`, `Allocation Rate` (every other `needs_gc` clause), `Proactive`, `High Usage`, and `Allocation Stall` for an allocation that failed outright. HotSpot's ZGC never says `Allocation Failure`; this VM's ZGC did on every allocation-driven collection. `=0` restores `Allocation Failure`. `System.gc()` / `Metadata GC Threshold` are unchanged. |
| `CRATONVM_ZGC_CONC_EVERY_TRIGGER` | **OFF** (`=1` opt-in; a zr z8 default flip was reverted after the host batch: F1 CPU 1.11x, pause 0.88x vs STW -- the slide, not the mark, dominates the pause) | *zr z3/cs (2026-10-12).* HotSpot ZGC's model: every non-urgent trigger (warm-up, allocation budget, live-bytes threshold, headroom, the `CRATONVM_ZGC_RATE_TRIGGER` rules) first opens a CONCURRENT mark when the heap has the headroom to finish one (free space above 5% of the capacity -- at least the headroom margin -- plus 1.5x the expected allocation during the mark), and the collection is taken when the mark reaches its fixed point. Stop-the-world only on allocation failure, at the urgent reserve, without headroom, or when a requested start is refused (the take-over cover) or not served within half the reserve. Configures concurrent marking by itself; inert in generational mode. Counted on `[GC] zgc-conc-trigger:` (`ct_opened`, `ct_finished`, `ct_abandoned_stw`, `ct_urgent`, `ct_lapsed`, `ct_start_refusals`, `ct_no_headroom`). Flip table: `docs/internal/zgc-round-zr-20261012/z3-cs-report.md`. |
| `CRATONVM_ZGC_CONC_RATE_FROM_ALLOCATION` | **on** (`=0` kill) | *zr z8/cm (2026-10-13).* The adaptive concurrent-start window samples the allocation since the last collection before the sweep resets the counter (it used to read the live-set growth, ~0 on a steady live set, so the window stuck at the bootstrap or the ceiling). Only acts in window mode (`CRATONVM_ZGC_CONC_START=auto`). |
| `CRATONVM_ZGC_CONC_WINDOW_EXTRAPOLATE` | **on** (`=0` kill) | *zr z8/cm (2026-10-13).* A concurrent mark cut short by its collection feeds `elapsed x total scanned / scanned concurrently` (capped at 4x) back as the mark-duration estimate instead of its short elapsed time, so windows stop shrinking cycle over cycle. Window mode only. |
| `CRATONVM_ZGC_SATB_FILTER_MARKED` | **2** (filter at buffer flush; `=1` per-store; `=0` kill) | *zr z7/cm, z8/cm (2026-10-13).* SATB records whose old value is already marked are dropped (HotSpot's `SATBMarkQueueSet::filter` rule). Mode 2 filters a 256-entry thread buffer when it flushes, so the per-store barrier is unchanged and an all-marked batch skips the ingress and the hand-off; mode 1 filtered each store (an extra mark-bitmap read per store). Only acts while a concurrent cycle is open. |
| `CRATONVM_ZGC_RATE_STW_RESERVE` | **OFF** | *zr z3/cs.* With the director deciding or shadowing and no concurrent marking configured, the allocation-rate rule reserves only one sample interval of worst-case allocation, not the cycle's duration as well (no mutator allocates during a stop-the-world cycle). Proposal `zgc-zr2t2-proposal-stw-aware-rate-reserve-20261012`. |
| `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS` | **on** (`=0` kill) | *zr z3/cs.* `-XX:MaxGCPauseMillis` does not size ZGC's pause-target budget (HotSpot's ZGC does not read it); `CRATONVM_ZGC_PAUSE_TARGET_MS` still does. |
| `CRATONVM_ZGC_STALL_CAUSE_FROM_TRIGGER` | **on** (`=0` kill) | *zr z2/t2 (2026-10-12).* A ZGC collection taken through the allocation-FAILURE door is reported `Allocation Stall` only when no trigger rule asked for it; otherwise it carries the rule `needs_gc` charged (`Warmup`, `Allocation Rate`, ...). The compiled refill trigger (`tlab_alloc_shaped_inner`, census site `tlab-alloc-shaped`) takes that door on an occupancy verdict, so every collection a JIT-compiled allocation loop caused printed `Major Collection (Allocation Stall)` although no allocation failed. `=0` restores the z1 reading. |
| `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE` | **on** (`=0` kill) | *zr z2/t2.* `ZGC Young Generation` reports the bytes allocated since the last collection and `ZGC Old Generation` the post-collection live figure. Old `committed == used`, young committed = heap committed minus old committed (never below young used), so the pools sum to `getHeapMemoryUsage()` (HotSpot 25 ZGC's pool shape, host `Zr3T2HeapVsPools`). `=0`: everything in the old pool with the heap's commit, young always 0. `MemoryMXBean.getHeapMemoryUsage` and `Runtime` are unchanged. |
| `CRATONVM_ZGC_HOTSPOT_BEAN_ORDER` | **on** (`=0` kill) | *zr z2/t2.* ZGC heap pools list their managers as HotSpot 25 does (`ZGC Minor Cycles, ZGC Major Cycles, ZGC Minor Pauses, ZGC Major Pauses`), and a collection's `end of GC pause` notification is queued before its `end of GC cycle` notification. `=0`: bean order on both. |
| `CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET` | **on** (`=0` kill) | *zr z9/hp (2026-10-13).* A ZGC collection writes no `jdk.GCPhasePauseLevel1` rows, on either JFR backend: HotSpot 25 ZGC writes none (its pauses have no sub-phases; `Zr4JfJfrEvents sysgc` on HotSpot: `present=false`). This collector's laps run inside its one pause, so they are not written as concurrent rows either. `=0` writes them as level-1 rows again (the z4/jf shape). |
| `CRATONVM_ZGC_EXIT_HEAP_SUMMARY` | **on** (`=0` kill) | *zr z9/hp.* `-Xlog:gc+exit` (and `gc*`) prints HotSpot ZGC's exit lines on ZGC: `Stopping ZGC`, `Heap`, ` ZHeap           used NM, capacity NM, max capacity NM` (allocated, committed, `-Xmx`, truncated to MiB) and the `Metaspace` line. HotSpot's two page-cache lines (`Cache`, `size classes`) and `class space` describe structures this heap does not have and are not printed. |
| `CRATONVM_VERSION_XLOG_BOOT` | **on** (`=0` kill) | *zr z9/hp.* `-version` / `--version` together with an `-Xlog` option boots the VM before printing the banner (HotSpot creates the VM for `-version`), so `-Xlog:gc -version` prints `[gc] Using The Z Garbage Collector`, `-Xlog:gc+init -version` the `gc,init` block and `-Xlog:gc+exit -version` the exit summary. Without `-Xlog`, or with no class library, the banner keeps its no-VM fast path. |
| `CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE` | **on** (`=0` kill) | *zs s2/ob (2026-10-14).* A `ZGC Major Pauses` / `ZGC Minor Pauses` `GcInfo` (`getLastGcInfo()`, the `end of GC pause` notification) carries all-zero pool usages before and after, as HotSpot 25's pause tracer records none (`Zs2ObBeanSurface`: `gcinfo_usage_before=zero`). The cycle beans keep the real figures. `=0`: the pause records carry the collection's real usage, as before. |
| `CRATONVM_ZGC_POOL_PEAK_AT_GC` | **on** (`=0` kill) | *zs s2/ob.* `ZGC Young Generation` / `ZGC Old Generation` `getPeakUsage()` also takes the pools' usage at both edges of every collection, as HotSpot's ZGC cycle tracer records it (a query-time sample alone missed the pre-collection peak). `resetPeakUsage()` resets it. `=0`: query-time peaks only. |
| `CRATONVM_ZGC_HOTSPOT_POOL_INIT` | **on** (`=0` kill) | *zs s2/ob.* `ZGC Young Generation`'s `MemoryUsage.getInit()` is `MinHeapSize` (8M by default, `-Xms` when given) and `ZGC Old Generation`'s 0, as HotSpot 25's `ZServiceability` builds them. `=0`: young 0, old the startup commit. |
| `CRATONVM_ZGC_HEAP_ALIGN_HOTSPOT` | **on** (`=0` kill) | *zs s2/ob.* The launcher aligns ZGC's `-Xmx`, initial and minimum heap up to 2 MiB, HotSpot's ZGC granule: `-Xmx9m` is a 10M heap (`Runtime.maxMemory()` 10485760, `Max Capacity: 10M`), `-Xms5000k` a 6M minimum. `-XX:SoftMaxHeapSize` is not aligned (nor is it on HotSpot). Launcher only; the embedding entry points keep the typed sizes. `=0`: the typed sizes. |
| `CRATONVM_ZGC_REFUSE_ZERO_GC_THREADS` | **on** (`=0` kill) | *zs s2/ob.* An explicit `-XX:+UseZGC` with `-XX:ParallelGCThreads=0` (checked first) or `-XX:ConcGCThreads=0` exits 1 with HotSpot's two lines on stdout, `Error occurred during initialization of VM` / `The flag -XX:+UseZGC can not be combined with -XX:<name>=0`, also in place of a `-version` banner. ZGC as the default (no `-XX:+UseZGC`) is not refused: a zero `ConcGCThreads` is dropped silently (zs s3/ob2), a zero `ParallelGCThreads` keeps the warning (HotSpot's default G1 refuses it with a G1-named text). `=0`: warn and continue. |
| `CRATONVM_ZGC_JFR_HOTSPOT_CONFIGURATION` | **on** (`=0` kill) | *zs s2/ob.* On ZGC the real JFR backend's `jdk.GCHeapConfiguration` states `MinHeapSize` / `InitialHeapSize` / `MaxHeapSize` and narrow-oop mode `32-bit` (it wrote `Runtime.totalMemory()` as both the minimum and the initial size), and `jdk.GCConfiguration` the explicit `-XX:ParallelGCThreads` / `-XX:ConcGCThreads`, `UseDynamicNumberOfGCThreads`, `DisableExplicitGC` and `ExplicitGCInvokesConcurrent`. Ergonomic thread counts keep the generic figures. `=0`: the generic figures. |
| `CRATONVM_ZGC_JFR_HOTSPOT_PAUSES` | **on** (`=0` kill) | *zs s7/jf.* A ZGC collection's `jdk.GCPhasePause` rows are HotSpot 25's pause names over the pause's own phase split: `Pause Mark Start (Major)` (`Pause Mark Start` for a minor; absent when the mark start was its own pause), `Pause Mark End`, `Pause Relocate Start`; `GarbageCollection.longestPause` is the longest. Both JFR writers. `=0`: one `GC Pause` row. |
| `CRATONVM_ZGC_JFR_STALL_REQUEST` | **on** (`=0` kill) | *zs s7/jf.* `jdk.ZAllocationStall` `type` / `size` = the page HotSpot 25 ZGC would allocate for the request the arena refused before the stall (Small 2 MiB; Medium `MaxHeapSize/32` in [2M,32M]; Large rounded to 2 MiB). `=0`: no type, size 0. |
| `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS` | **on** (`=0` kill) | *zs s7/jf, s8/jf2.* `jdk.GCReferenceStatistics` (four rows per major collection, none per minor, as HotSpot 25) and, for a generational-mode `ZGC Minor`, `jdk.ZYoungGarbageCollection` (threshold = promotion age - 1); a major's young event also needs `CRATONVM_ZGC_JFR_MAJOR_YOUNG`. `=0`: neither. |
| `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER` | **on** (`=0` kill) | *zs s7/jf; corrected zt t1/jf; zt t3/jf: only read under `CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR=0`.* On ZGC `jdk.SystemGC` is written by `Runtime.gc()` on the calling thread with its stack, spanning the call (including the door's finalizer / cleaner / notification drains); the door then walks no stack for a row it will not write. `=0`: the `System.gc()` door's per-request row (gci i19/i23), also on the calling thread and with the request's stack, spanning the collection only -- the 2026-10-18 dev merge retired the collection's `VM Thread` row this switch used to fall back to. |
| `CRATONVM_ZGC_JFR_DRIVER_THREADS` | **on** (`=0` kill) | *zs s8/jf2.* A ZGC collection's own JFR events (real backend: `GarbageCollection`, `GCPhasePause`, `GCPhaseConcurrent`, `ZOld`/`ZYoungGarbageCollection`, `ZRelocationSet*`, `GCReferenceStatistics`...) are committed on `ZDriverMajor` (whole-heap collection, its mark start and concurrent mark) or `ZDriverMinor` (young), as HotSpot 25 names them; safepoint rows stay on `VM Thread`. `=0`: all on `VM Thread`. |
| `CRATONVM_ZGC_JFR_STALL_STACK` | **on** (`=0` kill) | *zs s8/jf2.* The real backend's `jdk.ZAllocationStall` is written from the stalling thread's context: its own Java thread entry and its Java stack, as HotSpot's. `=0`: no stack, thread found by name. |
| `CRATONVM_JFR_ENUM_KEYS_FROM_ONE` | **on** (`=0` kill) | *zs s8/jf2.* The real JFR backend numbers its enum-like constant pools (`GCName`, `GCCause`, `GCWhen`, `ZPageTypeType`, `ReferenceType`, ...) from 1, so a "no value" reference (id 0) reads back as `null`; numbered from 0 it read as the pool's first entry. All collectors (the numbering is internal; only a null reference reads differently). `=0`: from 0. |
| `CRATONVM_ZGC_JFR_SYSTEM_GC_NATIVE_FRAME` | **on** (`=0` kill) | *zs s8/jf2.* The caller-side `jdk.SystemGC` stack starts with the native `java.lang.Runtime.gc()` frame, as HotSpot 25's (its `JVM_GC` commits under that frame whatever the Java caller); the native's own frame is not in this VM's capture. Both modes. `=0`: the Java caller (`System.gc`) on top. |
| `CRATONVM_ZGC_JVMTI_FORCE_GC_DOOR` | **on** (`=0` kill) | *zs s8/jf2.* On ZGC a JVMTI `ForceGarbageCollection` is HotSpot 25's `_jvmti_force_gc` collection: cause `JvmtiEnv ForceGarbageCollection` (JFR, `-Xlog:gc`, GC notifications), no `jdk.SystemGC` on either JFR writer (HotSpot commits it in `JVM_GC` only), and not refused by `-XX:+DisableExplicitGC`. `=0`: the `System.gc()` door. |
| `CRATONVM_ZGC_JFR_METASPACE_CHUNK_FREE_LISTS` | **on** (`=0` kill) | *zs s10/jf3, ported zt t1/jf.* The real JFR backend writes HotSpot 25's `jdk.MetaspaceChunkFreeListSummary` per ZGC collection: four rows (Before / After GC x `Metadata` / `Class`), every count and size 0 -- HotSpot's elastic metaspace has kept no chunk free lists since JDK 16 and writes zeros; this VM has none either. Generational and G1 write none (out of the ZGC round's scope). `=0`: none. |
| `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN` | **on** (`=0` kill) | *zs s10/jf3, ported zt t1/jf.* A ZGC concurrent cycle (`CRATONVM_ZGC_CONC_START`) is written as HotSpot 25 writes one: `jdk.GarbageCollection` and `ZOld` / `ZYoungGarbageCollection` span from its mark-start pause to the closing collection's end; the mark-start pause is the cycle's first pause row, `Pause Mark Start (Major)` (`Pause Mark Start` for a minor), counted in `sumOfPauses` / `longestPause`. *zt t2/jf:* the `Before GC` `jdk.GCHeapSummary` (both writers) is the heap as the mark-start pause found it, at the cycle's start, as HotSpot traces it; the metaspace summaries keep the closing collection's edges. A mark start whose cycle never closes is still written, on its own (zt t3/jf: as `Pause Mark Start (Major)` with its `ZMarkStartYoungAndOld` safepoint under `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS`). Both JFR writers. `=0`: the s7 shape (the mark-start row on its own, `Pause Mark Start`). |
| `CRATONVM_ZGC_JFR_MAJOR_YOUNG` | **on** (`=0` kill) | *zt t1/jf (s7a host JF1).* A ZGC major (whole-heap) collection writes `jdk.ZYoungGarbageCollection` under its `gcId`, on `ZDriverMajor`, from the cycle's start to its `Pause Mark End` row (zt t3/jf: HotSpot's young collection ends before the old one; without named rows, the whole span): HotSpot 25 ZGC is generational only and every major runs a young collection first. `tenuringThreshold` 0 (promote all) in the single-generation mode and on `System.gc()`; the promotion age - 1 in the generational mode otherwise. One row per major where HotSpot's `System.gc()` writes two (pre-clean + roots young collections). Real backend only. Also needs `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS`. `=0`: none for a major. |
| `CRATONVM_ZGC_JFR_MAJOR_YOUNG_AT_MARK_END` | **on** (`=0` kill) | *zt t4/x3 (switch for zt t3/jf's zt2x1 item 4).* The `jdk.ZYoungGarbageCollection` a ZGC major writes under `CRATONVM_ZGC_JFR_MAJOR_YOUNG` ends where the major's `Pause Mark End` row starts (HotSpot's major runs its young collection first, so that event ends before the old one), clamped into the cycle; a major without named pause rows keeps the cycle's span. `=0`: the young event spans the whole cycle, as before t3. Read per collection. |
| `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS` | **on** (`=0` kill) | *zt t1/jf.* The real JFR backend writes a ZGC collection's stop-the-world time as HotSpot 25 ZGC's VM operations: one `SafepointBegin` / `ExecuteVMOperation` / `SafepointEnd` per named pause row (`ZMarkStartYoungAndOld`, `ZMarkEndOld`, `ZRelocateStartOld` for a major; `ZMarkStartYoung`, `ZMarkEndYoung`, `ZRelocateStartYoung` for a minor), `caller` the collector thread (`ZDriverMajor` / `ZDriverMinor`; HotSpot's driver requests them). It wrote one safepoint per collection, operation `Collect` (an operation HotSpot's ZGC never runs), with the Java requester as `caller`. Without named pause rows (`CRATONVM_ZGC_JFR_HOTSPOT_PAUSES=0`, or a split that does not fit) the one collection-wide row is named `ZMarkStartYoungAndOld` / `ZMarkStartYoung` (zt t2/jf). `=0`: the one `Collect` row. |
| `CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR` | **on** (`=0` kill) | *zt t3/jf.* On ZGC `jdk.SystemGC` is the `System.gc()` door's per-request row: the calling thread (its pool record taken off the `Thread` object by the `Runtime.gc()` native first, so a virtual thread reads virtual), the request's stack under the `Runtime.gc()` frame, spanning the collection only, as HotSpot 25's `JVM_GC` (local 25.0.3 `Zt1JfSystemGcSpan`: `ends_with_collection=true`; the caller-side row also spanned the finalizer / cleaner / notification drains: `false`). Wins over `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER`. `=0`: the caller-side writer. |
| `CRATONVM_ZGC_JFR_SYSTEM_GC_ENDS_WITH_COLLECTION` | **on** (`=0` kill) | *zt t5/jf.* A ZGC `jdk.SystemGC` row (the door's, or the caller side's under `CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR=0`) ends where the last ZGC collection inside the request ended, as HotSpot 25's `JVM_GC` commits right after `collect` (local 25.0.3 `Zt1JfSystemGcSpan`: 0.1-0.8 ms after the collection; this VM's row ended 16-24 ms later, after the collection's own event emission and the door's work). Other collectors: unchanged. `=0`: the writer's own end. |
| `CRATONVM_ZGC_JFR_STANDALONE_MARK_START_JOINS` | **on** (`=0` kill) | *zt t5/jf.* The mark-start pause of a ZGC concurrent cycle that its closing collection did not close concurrently (a `System.gc()` abandoned it and marked afresh) carries the id that collection takes; it joins the collection's pause rows and `jdk.GarbageCollection` span instead of sitting before it under the same `gcId` (local row `jf2_t2_sysgc_conc`: `pause ... inside_gc=false`, HotSpot `true`). Needs `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN`. `=0`: on its own. |
| `CRATONVM_ZGC_XLOG_PAUSE_PHASES` | **on** (`=0` kill) | *zt t5/jf (R9).* A ZGC collection prints, between its `gc` start and end lines, HotSpot 25's `gc,phases` pause lines -- `GC(n) Y: Pause Mark Start (Major) 0.011ms`, `GC(n) O: Pause Mark End ...`, `GC(n) O: Pause Relocate Start ...` (`y:` and the plain names for a generational-mode minor; a concurrent cycle's mark start first when a recording joined it) -- and its `gc,metaspace` line `GC(n) O: Metaspace: NM used, NM committed, NM reserved`, the shapes of a local HotSpot 25.0.3 `-Xlog:gc*` capture. The pauses are the same three the `... Pauses` bean and JFR book. HotSpot's other per-collection `gc*` lines are recorded as intended differences in `docs/GC.md`. *zt t6/ob4 (R9 tail):* and, after the metaspace line, the three `gc,heap` capacity lines `GC(n) O: Min Capacity: 8M(3%)`, `GC(n) O: Max Capacity: 256M(100%)`, `GC(n) O: Soft Max Capacity: 256M(100%)` (`y:` for a minor; the `gc,init` block's min and max, the soft max the collection ran under; percent of the max). `=0`: none of these. |
| `CRATONVM_ZGC_JFR_CODE_HEAP_ROWS` | **on** (`=0` kill) | *zt t2/jf.* On ZGC the real JFR backend's `jdk.CodeCacheStatistics` is one row per HotSpot code heap (`CodeHeap 'non-nmethods'`, `'profiled nmethods'`, `'non-profiled nmethods'`, as `jdk.types.CodeBlobType` keys), the JIT's code in the non-profiled one with `unallocatedCapacity` = code-cache cap - used, and `jdk.CodeCacheConfiguration` states the cap as `reservedSize` / `nonProfiledSize`: the figures the `CodeHeap` pool beans report. The generic row wrote `codeBlobType` as an inline string into a constant-pool field (it read back `null`). Other collectors keep it (outside the ZGC round). `=0`: the generic row. |
| `CRATONVM_JFR_NATIVE_THREAD_NULL_JAVA_NAME` | **on** (`=0` kill) | *zs s8/jf2.* The real JFR backend writes a VM-internal thread (no Java thread id and no Java name: `VM Thread`, `ZDriverMajor` / `ZDriverMinor`, `ZUncommitter`) with a null `javaName`, as HotSpot 25 writes its non-Java threads (`RecordedThread.getJavaName()` is `null`); it wrote `""`. All collectors. `=0`: `""`. |
| `CRATONVM_ZGC_REFUSE_MALFORMED_GC_THREADS` | **on** (`=0` kill) | *zs s3/ob2.* On ZGC (explicit or default) a malformed `-XX:ParallelGCThreads=` / `-XX:ConcGCThreads=` value (`abc`, `-1`, `+3`, `1.5`, empty, `0x`, above `u32`) ends the launch as HotSpot 25 does: `Improperly specified VM option '<name>=<value>'`, `Error: Could not create the Java Virtual Machine.`, `Error: A fatal exception has occurred. Program will exit.` on stderr, exit 1, the first bad option in command-line order. Both flags take HotSpot's value syntax (`0x` hex, `k`/`m`/`g`/`t` multiplier). `=0`: warn and continue, decimal only. |
| `CRATONVM_ZGC_IMPLICIT_ZERO_GC_THREADS_SILENT` | **on** (`=0` kill) | *zs s3/ob2.* A default (implicit) ZGC drops `-XX:ConcGCThreads=0` without a stderr line, as HotSpot's default G1 accepts it silently. `=0`: the `Warning: ignoring ...` line. |
| `CRATONVM_ZGC_JMX_METASPACE_POOL` | **on** (`=0` kill) | *zs s3/ob2.* On ZGC the non-heap `Metaspace` `MemoryPoolMXBean` (measured class metadata; `init` 0, `committed` = `used`, `max` `-XX:MaxMetaspaceSize` or -1; usage threshold supported, collection threshold not, `getCollectionUsage()` null) is listed before the heap pools, its plain `Metaspace Manager` before the collectors (left out of `getGarbageCollectorMXBeans()`), and every `GcInfo` carries a `Metaspace` key (cycle beans: the figure at each edge; pause beans: zeros). The edges sample only once a program has been handed the pool beans, and walk the class store only when the loaded-class census moved (never inside the pause). *zt t1/ob:* the listed pools are now `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS`'s (HotSpot's five, next row); this switch keeps the `GcInfo` edge samples, and lists its own `Metaspace`-only pool (`committed` = `used`) only under that row's kill. `=0`: no edge samples (a cycle `GcInfo`'s `Metaspace` entry is the pool's usage at delivery). |
| `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS` | **on** (`=0` kill) | *zt t1/ob (the zs / dev merge decision).* ZGC lists HotSpot 25's five non-heap `MemoryPoolMXBean`s (`CodeHeap 'non-nmethods'`, `CodeHeap 'profiled nmethods'`, `CodeHeap 'non-profiled nmethods'`, `Metaspace`, `Compressed Class Space`) and the plain `CodeCacheManager` / `Metaspace Manager` with gci i15/nh / i33/ms's figures, as every other backend does, so each pool bean and `getNonHeapMemoryUsage()` (their sum) agree. A pause `GcInfo` carries a zero entry for all five; a cycle `GcInfo`'s `Compressed Class Space` entry (and its `Metaspace` `committed`) comes from the same edge walk as its `Metaspace` entry. `=0`: zs s3/ob2's `Metaspace`-only list and `Metaspace Manager`, and its `committed` = `used` on the pool bean and the `GcInfo` entries (zt t2/x1: the t1 cut kept the edge walk's committed under the kill). |
| `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT` | **on** (`=0` kill) | *zt t3/ob.* With the row above on, ZGC's three `CodeHeap '...'` pools each report HotSpot 25's `init` of 2555904 (`InitialCodeCacheSize`, 2496K, committed per heap at start), so `MemoryMXBean.getNonHeapMemoryUsage().getInit()` is HotSpot's 7667712 instead of 2 MiB; the non-heap threshold samples carry the same `init`. G1 / Generational keep gci i15/nh's figures. `=0`: 2 MiB on the non-profiled heap, 0 on the other two. |
| `CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES` | **on** (`=0` kill) | *zt t6/ob4.* With `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS` on, a ZGC cycle `GcInfo` (`getLastGcInfo()`, the cycle notifications) carries the three `CodeHeap '...'` entries sampled at the collection's two edges (the JIT's mapped code, one relaxed load per edge), as HotSpot 25 samples every pool at `gc_begin` / `gc_end`; they were rendered from the usage at delivery, so before == after and code installed after the collection was counted. Pause `GcInfo`s keep their zero entries. G1 / Generational unchanged. `=0`: rendered at delivery. |
| `CRATONVM_ZGC_METASPACE_FIRST_CYCLES` | **on** (`=0` kill) | *zs s6/ob5.* A ZGC cycle `GcInfo` recorded before the `Metaspace` pool was first handed out keeps the class-store size at its edges (one length read, no walk) and gains the measured `Metaspace` key -- the class metadata of exactly the classes defined by then, one walk of that prefix of the class store -- when a `GcInfoBuilder` first names the pool (`MemoryImpl.getMemoryPools0`, or the first notification drain). `=0`: such a record keeps no key and reads the JMM's undefined usage (`init = max = -1`); HotSpot always has the figure. |
| `CRATONVM_ZGC_METASPACE_THRESHOLD` | **on** (`=0` kill) | *zs s6/ob5.* A `Metaspace` usage threshold is checked when it is set and at the end of every collection (HotSpot 25: the same two points, not on class loading), so its sensor fires `MEMORY_THRESHOLD_EXCEEDED` and counts. The set-time crossing is delivered at the next drain point (see the next row; HotSpot's Service Thread delivers it at once). *zt t1/ob:* since the dev merge gci i16/nt arms and checks every non-heap threshold on every backend (at the set, on the define cadence, and at a collection's end when its walk is due); this switch adds the every-collection-end check against the edge sample. `=0`: only gci i16/nt's checks (the sensor still fires). |
| `CRATONVM_ZGC_THRESHOLD_SET_DRAIN` | **on** (`=0` kill) | *zs s6/ob5.* On ZGC a usage-threshold crossing that `setUsageThreshold` finds (any pool) raises the drain hint and (zs s9/ob6) asks the delivery thread at once, so the notification is delivered without waiting for a collection or for the setting thread to allocate (zt t1/ob: also for a non-heap pool such as `Metaspace`, whose set takes gci i16/nt's arm since the dev merge). `=0`: it waits for a drain door (the next collection, or the 16 KiB occupancy poll of an allocating thread). |
| `CRATONVM_ZGC_METASPACE_BACKFILL_HOLD_MEMO` | **on** (`=0` kill) | *zt t6/rv3.* With `CRATONVM_ZGC_METASPACE_BUSY_LOCK_BACKFILL` on: once a drain's busy-lock retries (16 yields + `try_read`s) failed, later drain points keep the hold after their one non-blocking try until the class-definition epoch or the collection count moves, instead of paying the retries again while one thread holds the class lock for a long define. Delivery order and content unchanged. `=0`: the retries at every drain point. |
| `CRATONVM_ZGC_METASPACE_BUSY_LOCK_BACKFILL` | **on** (`=0` kill) | *zs s9/ob6.* The `Metaspace` back-fill (`CRATONVM_ZGC_METASPACE_FIRST_CYCLES`) survives a busy class lock. A collection edge reads the class store's next `ClassId` from a published atomic, with no lock. A GC-notification drain whose non-blocking back-fill finds the lock busy (after a few yields) holds the keyless cycle records back, delivery order kept, until a later round or drain point, instead of delivering them with the JMM's undefined usage (`init = max = -1`), which HotSpot never reports. `=0`: the s6/ob5 `try_read` census and keyless delivery. |
| `CRATONVM_ZGC_IMPLICIT_ZERO_PARALLEL_GC_THREADS_REFUSE` | **on** (`=0` kill) | *zs s9/ob6.* With no collector flag (the default ZGC), `-XX:ParallelGCThreads=0` ends the launch as HotSpot 25's default G1 does: `Error occurred during initialization of VM` / `The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0` on stdout, exit 1, also before a `-version` banner. `=0`: the `Warning: ignoring ...` line and the run. |
| `CRATONVM_ZGC_METASPACE_PEAK_AT_GC` | **on** (`=0` kill) | *zs s6/ob5.* The `Metaspace` pool's collection-edge samples are its peaks (`getPeakUsage()` keeps the figure a collection saw after the classes are unloaded), and `resetPeakUsage()` resets that peak. Also off with `CRATONVM_ZGC_POOL_PEAK_AT_GC=0`. `=0`: a query-time peak. |
| `CRATONVM_ZGC_INIT_BACKING_LINE` | **on** (`=0` kill) | *zr z2/t2.* `-Xlog:gc+init` prints `Available space on backing filesystem: N/A` between `Pre-touch` and `Uncommit` (this heap has no backing filesystem). |
| `CRATONVM_ZGC_XLOG_ALLOCATION_STALL` | **OFF** | *zr z2/t2.* With `-Xlog:gc`, print `Allocation Stall (<thread>) <ms>ms` (HotSpot ZGC's critical-phase line, from reading) for every mutator allocation stall. The stall itself is recorded on `ZgcMetrics` whatever this says (`CRATONVM_ZGC_METRICS=1` summary). |
| `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT` | **`2`** (`=1` after-failure only, `=0` kill) | *zr z2/cp (2026-10-12).* The low slide's page selector widens its live-occupancy cutoff under pressure. `1`: after a hard allocation failure, pages up to 95 % live are evacuated (HotSpot's old-page figure, recalled) and the cost gate is skipped -- without it no page over 25 % live is ever compacted and a contiguous request (an `ArrayList`'s `Object[]`) fails on a heap 60 % free in small holes (`ProxyBoxingOomeProbe` fit 39 % of HotSpot's live set). `2` (the default since 2026-10-13): also 0.75 whenever the low bump headroom is under 25 % of capacity (host: `Zr2CpCapacity mosaic -Xmx256m` CPU halved, 9.1-10.6 s against 18.2-22.4 s). `0`: the pre-round selector. Counters on `[GC] zgc-capacity:` (`after_failure_slides`, `tight_slides`, `widened_live_bytes`). |
| `CRATONVM_ZGC_INFALLIBLE_OOME_UNWIND` | **on** (`=0` kill) | *zr z2/cp.* ZGC's infallible `alloc_object` / `alloc_array` offer the VM's catchable-`OutOfMemoryError` unwind (where `native_oom::unwind_ok` allows it) before aborting. `infallible_unwind_offers=` on `[GC] zgc-capacity:`. |
| `CRATONVM_ZGC_OVERHEAD_BACKSTOP` | **OFF** (`=1` opt-in) | *zr z2/cp; default ON 2026-10-12, opt-in since 2026-10-13 because HotSpot ZGC has no overhead limit (`GcOverheadProbe` went DIFF with it on).* The GC-overhead crawl backstop on ZGC: eight consecutive forced collections on a heap under 2 % free, each of whose windows the live set grew by at least 1/16 of (`zgc_window_is_crawl`), and the next refusal throws after the soft-reference rung and the bounded grace. Ends the near-OOM one-collection-per-refusal crawl (`Zr2CpCapacity grow`: 3 302 collections with `=0`). A program working in a steady near-full heap (live flat) never counts. Not a HotSpot rule (HotSpot's overhead limit is Parallel-only). |
| `CRATONVM_ZGC_REFUSAL_SKIP_FUTILE` | **on** (`=0` kill) | *zr z5/cr (2026-10-13).* A ZGC allocation refusal on a heap that allocated under 4 KiB since its last collection gets no forced collection of its own; its ladder's clear-soft rung pays one owed collection instead (soft references or not). Also skips the native-boundary relief raised by the hard-failure latch alone on such a heap. Saves the second collection of an unchanged heap (post-OOME allocations, ProxyBoxing's door 2). `[GC] zgc-refusal:` counts it. |
| `CRATONVM_ZGC_NATIVE_ROOM_SKIPS_SOFT` | **on** (`=0` kill) | *zr z5/cr.* The native factory door (no attempt between its rungs) skips the clear-soft rung when its forced collection was the only one since the arena refused the request and a single span now serves it: HotSpot's young, try, full, try. |
| `CRATONVM_ZGC_NATIVE_ROOM_SKIP_REPAY` | **on** (`=0` kill) | *zr z8/nr.* That skip is tracked per thread (one per-VM slot, taken only when free); a caller whose retry still fails (another thread took the span, or the native's own first allocation did) pays the skipped clear-soft rung (`last_ditch_reclaim` + second major) and retries once more before `OutOfMemoryError`. Counters `soft_skip_repaid=` / `soft_skip_unpaid=` on `[GC] zgc-refusal:`. |
| `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` | **on** (`=0` kill) | *zr z8/nr.* ZGC only: `Unsafe.allocateInstance` (every constructor method handle, `Constructor.newInstance`) and `Thread.getThreads` / `dumpThreads` take the fallible door and the reclaim ladder before `OutOfMemoryError` instead of one attempt; `=0` restores the one-attempt doors. |
| `CRATONVM_ZGC_NATIVE_OOM_REPLAY` | **on** (`=0` kill) | *zr z11/nd.* ZGC only: when a native's one-attempt allocation door refuses (the `NativeAllocOom` unwind) and the native's body is listed as restartable (`native-builtins/src/oom_restartable.rs`: `Class.getDeclaredFields0` / `getDeclaredMethods0` / `getDeclaredConstructors0` / `getMethods` / `getInterfaces0` / `getDeclaredClasses0` / `getName`, `Thread.getStackTrace0` / `dumpThreads` / `getThreads`, the `--compatible` `Integer.toString` / `Long.toString` / `Double.toString` / `Integer.toHexString`, `Object.getClass`, and since zs s1/nd the `--compatible` `AbstractStringBuilder.getValue()`; the list is frozen by `native-builtins/tests/oom_restartable_ratchet.rs`), the native funnel walks the native ladder's rungs and re-runs the native on its re-read (pinned) arguments after each, before `OutOfMemoryError`. Counters `native_replays=` / `native_replay_served=` on `[GC] zgc-refusal:` (since zs s1/nd `served` counts only re-runs that returned a value, not a Java error); `=0` restores the one-attempt answer. |
| `CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS` | **on** (`=0` kill) | *zs s1/dg.* ZGC only: a LIVE array past 1 << 28 elements (legal up to `i32::MAX`, refused by `alloc_size`'s element cap) is sized from independent evidence (registered base, extent inside the arena, no other base inside it; `ZgcRealHeap::survivor_size`) by the slide's census, partition, both slides, screens and reference rewrite, by the reference-slot walk, and by the mark's live-set accounting. Before, the rewrite skipped it ("rewrite target is not walkable ... SKIPPED -- any reference they hold to a moved object is now dangling"), a reference array's elements were omitted from the slot walk (dangling after a slide), the slide stopped at it (compaction starved) and its bytes were not counted live. `=0` restores all of that (positive control: the guard lines come back on `Zs1DgUnwalkableSurvivor prim`). |
| `CRATONVM_ZGC_NATIVE_OOM_REPLAY` | **on** (`=0` kill) | *zr z11/nd.* ZGC only: when a native's one-attempt allocation door refuses (the `NativeAllocOom` unwind) and the native's body is listed as restartable (`native-builtins/src/oom_restartable.rs`: `Class.getDeclaredFields0` / `getDeclaredMethods0` / `getDeclaredConstructors0` / `getMethods` / `getInterfaces0` / `getDeclaredClasses0` / `getName`, `Thread.getStackTrace0` / `dumpThreads` / `getThreads`, the `--compatible` `Integer.toString` / `Long.toString` / `Double.toString` / `Integer.toHexString`), the native funnel walks the native ladder's rungs and re-runs the native on its re-read (pinned) arguments after each, before `OutOfMemoryError`. Counters `native_replays=` / `native_replay_served=` on `[GC] zgc-refusal:`; `=0` restores the one-attempt answer. |
| `CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE` | **on** (`=0` kill) | *zs s2/rf, ported to dev 2026-10-17.* ZGC only: the per-object sweep (`sweep_one`: young cycles under `CRATONVM_ZGC_GENERATIONAL=1`, and the fallback arm when the complement sweep refuses, e.g. `CRATONVM_ZGC_BITMAP_SWEEP=0` or `CRATONVM_ZGC_SWEEP_DEAD_RUNS=0`) frees a DEAD array that `alloc_size` refuses, in either region, once `oversize_array_extent`'s checks pass against the cycle's immutable snapshot (race-free under `CRATONVM_ZGC_PARSWEEP`) and the true low bump cursor. Before, it retained such an array forever (premature `OutOfMemoryError`). Counter `sweep_one_oversize_freed=`; probe `Zs2RfOverLong dead`. |
| `CRATONVM_ZGC_SWEEP_SPILL_HIGH_SPANS` | **on** (`=0` kill) | *zs s2/rf.* A spilled dead base in the large-object end always pushes its free span; the "already reclaimed by the complement" test compared it against `Arena::used` (low cursor plus large-object bytes) and dropped the span while removing the registry bit, so the bytes were lost for good. |
| `CRATONVM_ZGC_OVERSIZE_SCREEN_MEMO` | **on** (`=0` kill) | *zs s2/rf, zs s3/rv2.* The live over-long array screen (`oversize_live_array_extent`, a `size / 8`-bit registry scan) memoises its answer per `(base, length)` for one stop-the-world collection; a memoised positive re-checks the registry bit on every hit. One scan per array per pause instead of ~7. |
| `CRATONVM_ZGC_ELEMENT_CARD_REFUSED_SCAN` | **on** (`=0` kill) | *2026-10-17.* Only with `CRATONVM_ZGC_GENERATIONAL=1 CRATONVM_ZGC_GEN_ELEMENT_CARDS=1`: a young cycle's element card on a reference array past 1 << 28 elements that the live screen refuses (its switch off, or a claimed extent that swallows a neighbour or runs past the arena) is scanned conservatively (the granule's own slots; only registered young targets are rooted) and kept. Before, the card was dropped without rooting its young targets. |
| `CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR` | **on** (`=0` kill) | *zt t3/gn, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1`: a full request (`System.gc()`, `jcmd GC.run`) makes the collection whole-heap (`Major Collection (System.gc())`), as HotSpot 25's generational ZGC does. Before, a heap with an old generation ran a YOUNG cycle for it: dead old objects survived it, their weak references stayed set and `-Xlog:gc` printed `Minor Collection`. Engagement: `majors_by_request=` on `[GC] zgc-gen-cost:`. |
| `CRATONVM_ZGC_GEN_MAJOR_HEADROOM_PERCENT` | **0 = off** (`=<q>` opt-in, 1..100) | *zt t3/gn, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1`. Majors by old growth against the free space the last major left: a whole-heap cycle is due once the bytes promoted since the last major reach `q` % of that free space (at least 1 % of capacity), with an 8x `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` backstop; takes precedence over `CRATONVM_ZGC_GEN_MAJOR_PROMOTED_PERCENT` and the fixed ceiling. Allocation failure, a young cycle that freed nothing and a full request still force majors. Stage 1 of `docs/internal/proposals/gc/zgc-zt3gn-proposal-generational-as-the-default-20261019.md`: the near-full lever (young cycles where the default runs a ~39 ms whole-heap trace). Engagement: `majors_by_headroom=` on `[GC] zgc-gen-cost:`. |
| `CRATONVM_ZGC_GEN_SOFT_CLEAR_MAJOR` | **on** (`=0` kill) | *zt t5/gn, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1`: a collection that clears every soft reference (the last-ditch rung before `OutOfMemoryError`, a per-VM clear-all request, the near-full soft clear `CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`) is made whole-heap through the collecting thread's full request (needs `CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR`, default on). A young cycle pre-marks the old generation, so it consumed the request and kept every old soft referent. Engagement: `majors_by_request=` on `[GC] zgc-gen-cost:`. |
| `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS` | **1 = serial** (`=<n>` opt-in, clamped to the cores and 64; `0`/`1` serial) | *zt t6/tc, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1`. Shards the young-aged sweep (`sweep_young_aged`, and the aged pass (1) of `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT=1`) over `n` threads at bitmap-word boundaries of the covered words; the generational survivor writes (age, old-start bit, promotion card) are per-object atomics the mutator barrier already runs concurrently. GN-17 of `docs/known-issues/gc/zgc-zt3gn-generational-mode-review-residuals-20261019.md`. Engagement: `young_sweep_sharded=` / `young_sweep_shards=` on `[GC] zgc-gen-cost:`; the decision is `minor_sweep_us` and process CPU against `=1`. |
| `CRATONVM_ZGC_GEN_HIGH_YOUNG_LIVE` | **on** (`=0` kill) | *zt t5/gn, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1`: a whole-heap cycle leaves its YOUNG survivors in the large-object region (allocations of 64 KiB or more) out of the old live figure, because the young cycles after it visit and count them (they lie above every young floor). They were counted twice, then a third time once promoted, until the next major: a heap of large arrays read up to twice its live set, so the live-bytes trigger fired early and the near-full soft clear saw a full heap. |
| `CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE` | **on** (`=0` kill) | *zt t6/tc, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1` and young cycles on the floor-bounded sweep (`CRATONVM_ZGC_GEN_OLD_BITMAP=0` or `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP=0`): a whole-heap cycle leaves its OLD survivors in the large-object region out of the old live figure as well, because the floor arm's young cycles sweep every base above the floor and count them themselves. They were counted twice until the next major (GN-18 of `docs/known-issues/gc/zgc-zt3gn-generational-mode-review-residuals-20261019.md`). No effect on the default generational arms (bitmap and aged sweep on). |
| `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES` | **on** (`=0` kill) | *zt t6/tc, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1`: the whole-heap cycle a `System.gc()` door runs (with `CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default on, read on the collecting thread) ages every survivor straight to the promotion age (card + old-start bit, as an ordinary promotion), as HotSpot's full collection tenures live young data; the survivors used to stay young and were traced by up to `promotion_age - 1` more young cycles (GN-5). Engagement: `system_gc_tenures=` on `[GC] zgc-gen-cost:`. |
| `CRATONVM_ZGC_GEN_JIT_PROMOTION_FLOOR` | **on** (`=0` kill) | *zt t6/tc, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1`: the JIT reference-store plan publishes `promotion_age << 4` as its young floor, so a compiled reference store into any young receiver (age below the promotion age) skips the barrier helper, exactly the helper's own age test; it published age 0, so receivers of age `1..promotion_age-1` paid a call per store once the heap had an old generation (GN-7). Lowered before a lower promotion age, raised after a higher one. Deciding row: E-put of the generational proposal (`Zr1GnGenShape S1`, JIT on, CPU and `SP_REF_STORE_*` helper counts). |
| `CRATONVM_ZGC_GEN_YOUNG_POOL_LIVE` | **on** (`=0` kill) | *zt t6/tc, 2026-10-19.* Only with `CRATONVM_ZGC_GENERATIONAL=1` and an old generation: the `ZGC Young Generation` pool counts the young survivors of the last collection (the live figure minus the last major's old live set and the promotions since) plus what was allocated since; the old pool the rest. It read 0 right after every minor while HotSpot's young pool holds the survivor pages (GN-11). Needs `CRATONVM_ZGC_YOUNG_POOL_SINCE_CYCLE` (default on). |
| `CRATONVM_ZGC_SOFT_MAX_MANAGEABLE` | **off** (`=1` opt-in) | *zt t6/tc, 2026-10-19.* ZGC only: `HotSpotDiagnosticMXBean.getVMOption("SoftMaxHeapSize")` answers a writeable `Long` row (value = the soft maximum, `-Xmx` by default; origin ERGONOMIC / COMMAND_LINE / MANAGEMENT) and `setVMOption("SoftMaxHeapSize", n)` applies `n` to the heap at run time (the next trigger poll reads it), as HotSpot's manageable flag does. Off: the option does not exist, as before. Values above `-Xmx` are refused with an `IllegalArgumentException` whose message is not HotSpot's. Probe `Zt6TcSoftMaxManageable` decides the flip (zt1tp item 2). |
| `CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR` | **on** (`=0` kill) | *zt t6/tc, 2026-10-19.* ZGC: a collection run for a `System.gc()` / `jcmd GC.run` request (read on the collecting thread) clears the warm-up bit a poll may have raised, so it is neither a warm-up step (`warmup_cycles_done`) nor director warmth; HotSpot counts only `Warmup`-cause cycles. The next poll raises the bit again if the step is still due. Allocation-failure collections still count (zt1tp item 7). Deciding rows: Q6 of `t3-tp-report.md`, `Zs1TpWarmupSteps sysgc`. |
| `CRATONVM_ZGC_SPIKE_TOLERANCE_HOTSPOT_PARSE` | **on** (`=0` kill) | *zt t7/x4, 2026-10-19.* ZGC only (explicit or default): the launcher reads `-XX:ZAllocationSpikeTolerance=<f>` as HotSpot 25.0.3 does -- any finite value is kept, `0` and negative ones included (the flag has no range; t6 dropped them with a warning and kept 2.0), and a malformed value (` 3`, `abc`, `inf`, `1e400`, `1e-400`) ends the launch with `Improperly specified VM option 'ZAllocationSpikeTolerance=<raw>'` and the two `Error:` lines on stderr, exit 1. A negative value now reaches clap as one `--name=value` token (it was a separate `-1` token, which failed the parse). `=0`: t6's rule (a positive number, else `Warning: ignoring ...` and the default). Off ZGC t6's rule stands. Known gap: `strtod`'s hexadecimal form (`0x1p3`, which HotSpot takes) is refused. The director reads the value only under `CRATONVM_ZGC_RATE_TRIGGER=1`. |
| `CRATONVM_ZGC_DIRECTOR_REARM_FLOOR` | **ON** (`=0` kill) | *zr zs s1/tp (2026-10-14).* Acts only under `CRATONVM_ZGC_RATE_TRIGGER=1`. A director rule's yes waits for the same `gc_rearm` anti-storm floor as the occupancy clauses. Without it, the high-usage rule (live set above 95% of the soft max) and the rate rule (free space below its reserve) fire again right after every cycle, with no new allocation. Engagement: `director_deferred=` on `[GC] zgc-trigger:`, beside the director's own tally `director=`. |
| `CRATONVM_ZGC_DIRECTOR_STW_MODEL` | **OFF** (`=1` opt-in) | *zr zs s1/tp.* The director's rules translated to a stop-the-world, byte-polled cycle. The rate rule reserves only the poll latency (one sample step + 4 MiB), with no cycle-duration term and no `max_rate x 0.1 s` term. The proactive rule assumes a full throughput drop while a cycle runs. Acts under `CRATONVM_ZGC_RATE_TRIGGER=1` / `CRATONVM_ZGC_TRIGGER_SHADOW=1`, and only without concurrent marking. Proposal `zgc-zs1tp-proposal-director-stw-model-20261014`. |
| `CRATONVM_ZGC_RATE_EXCLUDES_PAUSE` | **ON** (`=0` kill) | *zt t1/tp.* The ZGC metrics' allocation rate (`rate_bps` / `ttl_ms` / `ratio` on `[GC] zgc-trigger-shadow:`, the `[GC-SUMMARY] zgc` rate, the TSV) is measured over mutator time: each cycle's stop-the-world pause is subtracted from the inter-cycle interval (every lap of the pause, the mark-end lap of a concurrently marked cycle included -- zt t2/x1; the concurrent phase itself is in no lap and stays in the interval). No collection trigger reads this rate. |
| `CRATONVM_ZGC_DIRECTOR_HOTSPOT_WARMTH` | **ON** (`=0` kill) | *zt t1/tp.* Acts only where the director samples (`CRATONVM_ZGC_RATE_TRIGGER=1` / `CRATONVM_ZGC_TRIGGER_SHADOW=1`). Warmth is counted in warm-up cycles, as HotSpot's `ZStatCycle` does: the allocation-rate rule waits for the first warm-up cycle (`is_time_trustable`), the proactive rule for the third (`is_warm`). A `System.gc()` or an allocation stall is not a warm-up cycle. Under `CRATONVM_ZGC_WARMUP=0` every cycle counts. |
| `CRATONVM_ZGC_SOFT_MAX_PACES` | **OFF** (`=1` opt-in) | *zt t1/tp.* With `-XX:SoftMaxHeapSize` below `-Xmx`, the live-bytes trigger also fires at 75% of the soft maximum, not only at 75% of `-Xmx`. HotSpot's ZGC reads the soft maximum in every director rule; this VM's default arm reads it only in the warm-up clause. The re-arm floor is unchanged. Proposal `zgc-zt1tp-proposal-soft-max-paces-the-default-trigger-20261019`. |
| `CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS` | **ON** (`=0` kill) | *zr zs s1/tp.* The warm-up clause counts its 10/20/30% steps in warm-up cycles rather than collections, so a `System.gc()` is not a step, as on HotSpot. It also waits for at most half a step of new allocation instead of the `gc_rearm` floor, which had pushed the second warm-up from 20% to ~26%. |
| `CRATONVM_ZGC_CONC_START_RECHECK` | **ON** (`=0` kill) | *zr zs s1/tp.* A concurrent mark start re-tests that it is still due before requesting its pause, so the loser of the open race does not stop the world for a cycle that is already open. Counted as `ct_lost_races=` / `ct_start_rechecks=` on `[GC] zgc-conc-trigger:`. |
| `CRATONVM_ZGC_LAUNCHER_OOME_THROWABLE` | **on** (`=0` kill) | *zs s1/nd.* ZGC only: an `OutOfMemoryError` while the launcher builds an instance `main`'s receiver is thrown as `main`'s uncaught `java.lang.OutOfMemoryError` (vm-cli prints `Exception in thread "main" ...`, rc 1, as HotSpot) instead of `[cratonvm] main-vm run() returned Err: ...`. |
| `CRATONVM_ZGC_FFM_MINT_COLLECT` | **on** (`=0` kill) | *zs s2/nd2.* ZGC only (with `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` on): the FFM segment minted for a downcall's `ADDRESS` return after the C call (memo, JIT direct-stub and general paths) and for an upcall's address argument takes the fallible door and the reclaim ladder, then raises `OutOfMemoryError`; the one-attempt door it replaces aborted the process when a JIT helper made the downcall. *zt t1/nd:* since the dev merge the memo path's `ADDRESS` return collects on every collector (dev 1d682c68d2); on ZGC this key (and `CRATONVM_ZGC_NATIVE_DOORS_COLLECT`) is its kill again (`panama::memo_address_return_collects`). |
| `CRATONVM_ZGC_PROXY_INSTANCE_COLLECT` | **on** (`=0` kill) | *zt t1/nd.* ZGC only (with `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` on): the `Proxy.newProxyInstance` native (`reflect_annotations::native_proxy_new_instance`) mints the proxy instance through the fallible door and the reclaim ladder, then raises `OutOfMemoryError` (HotSpot allocates it in Java, `cons.newInstance`, which collects first); the one-attempt door raised it without a collection. Generational and G1 keep the one-attempt door. |
| `CRATONVM_ZGC_ADDCLASS_PRESIZE` | **on** (`=0` kill) | *zt t6/nd4.* ZGC only: the `--compatible` `ClassLoader.defineClass` backend (`classloader::define_class_via_full`) makes room in the loader's `classes` list (`ArrayList.add(null)` under the list's monitor, then `size` put back) BEFORE the mirror reservation and the define, when the list is full, so the `ClassLoader.addClass` it runs after the define allocates nothing. A refused growth is the define's `OutOfMemoryError` with nothing defined, as on HotSpot (`define_instance_class` calls `addClass` before the dictionary update); before, the class stayed defined and a retry met `LinkageError: duplicate class definition`. Generational and G1 unchanged. *zt t7/cl:* `define_class_via_full` is the synthetic-JDK / `JavaLangAccess` backend, not what a real-JDK `ClassLoader.defineClass` reaches in either mode; the presize now also runs at the real-JDK doors that make the upcall (`CRATONVM_DEFINE_ADD_CLASS`), and only while that switch is on. |
| `CRATONVM_DEFINE_ADD_CLASS` | **on** (`=0` kill) | *zt t7/cl.* Every collector (moves both modes to HotSpot; G1 and Generational take it alike): the real-JDK define doors (`lang_system`'s `ClassLoader.defineClass0/1/2`, which serve `ClassLoader.defineClass*`, `Lookup.defineClass` and JNI `DefineClass` in both `--jdk-only` and `--compatible`; `Unsafe.defineClass0`; `--compatible`'s own `Lookup.defineClass` door) call `ClassLoader.addClass(Class)` for every non-hidden class a loader object defines, after the define, as HotSpot's `SystemDictionary::define_instance_class` does. Before, the loader's private `classes` list stayed empty in both modes. The upcall's failure (its list growth's `OutOfMemoryError`) is the define's. On ZGC `CRATONVM_ZGC_ADDCLASS_PRESIZE` now runs at these doors too, so the growth happens before the define. A loader object whose `classes` is null (a VM-made loader) is skipped. |
| `CRATONVM_ZGC_CLASS_NAME_UNPOOLED` | **on** (`=0` kill) | *zt t7/cl.* ZGC only: the mirror `name` String of a class that a loader object defined (one with a defining-loader row: the classes that can unload) is not added to the interned-string pool, a strong root on ZGC, so it goes with the class when the loader unloads. Not Java-visible: the slot holds this VM's internal-name overlay (`pkg/Foo`), which HotSpot does not have, and `Class.getName()` interns its own answer, so `getName() == "Foo"` is unchanged. Read once per process. Generational and G1 unchanged. |
| `CRATONVM_ZGC_SPRING_BUILDER_COLLECT` | **on** (`=0` kill) | *zt t6/nd4.* ZGC only (with `CRATONVM_ZGC_NATIVE_DOORS_COLLECT` on): the Spring bootstrap's lookup-override and replace-override builders (`spring_startup_bootstrap::try_build_method_injection` / `try_build_replace_override`) retry their generated subclass's instance mint through the reclaim ladder when the heap refused the first `new_object`; the one-attempt door fell through to the ordinary instantiation path without a collection. Real Spring allocates the instance in Java, which collects first. Generational and G1 unchanged. |
| `CRATONVM_LOADER_TABLES_HASHED` | off (`=1` opt-in) | *zt t2/nd.* Every collector: the per-VM loader side tables (`classloader::LoaderIdentityTables` `loader_namespaces` / `loader_meta`) answer a loader's row and a namespace id's loader from a position memo (`MemoRows`) kept in step with the rows, instead of a linear scan per lookup (3-5 full scans per fresh loader on the proxy-define rows). Same answers by construction; opt-in until the A/B in `docs/internal/proposals/gc/zgc-zt1nd-proposal-address-keyed-loader-side-tables-20261019.md` passes. Read once per process. |
| `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH` | off (`=1` opt-in) | *zt t3/nd.* Every collector (a JIT dispatch memo key): `flush_class_identity_dispatch_memos` (vm/src/jit/helpers.rs) keys the JIT class-identity dispatch memos on the class-name generation, hierarchy-edge and in-place origin epochs plus the redefinition flag instead of the class-definition epoch, which moves on every define; a define that changes none of those drops only the memos' cached refusals (the bytecode-callee memos' `None` entries and `WRAPPED_STATIC_CALLEE`; `VIRTUAL_TARGET_CACHE`'s `selection_error` rows are kept, zt t6/rv3). For loops that define a class per iteration (the proxy-define rows of `zgc-zs9nd8-*`). Read through a `MemoSlot` (a test's thread override reaches it). Deciding rows in `docs/internal/zgc-round-zt-20261018/t3-nd-report.md`. |
| `CRATONVM_ZGC_LAUNCHER_ARGS_FIRST` | **on** (`=0` kill) | *zs s4/nd4.* ZGC only: the launcher builds `main`'s `String[]` before the main class is initialized and keeps it pinned across `<clinit>` and the instance receiver, HotSpot's `java.c` order (`CreateApplicationArgs` first). A `<clinit>` that fills the heap no longer fails before `main` (`Zs4Nd4StaticArgs`). |
| `CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED` | **on** (`=0` kill) | *zs s4/nd4.* ZGC only: a hidden class's mirror `name` String is not added to the interned-string pool, which is a strong root on ZGC. A non-strong hidden class that unloads takes its name with it, as on HotSpot (weak `StringTable`); `Lookup.defineHiddenClass` in a loop leaked ~84 bytes per define (`Zs2Nd2FfmMints define`: `door_oome` ~6400 of 20000). |
| `CRATONVM_ZGC_DEFINE_MIRROR_COLLECT` | **on** (`=0` kill) | *zs s2/nd2.* ZGC only: a class definer (`ClassLoader.defineClass0/1/2`, the `--compatible` `defineClass` / `Lookup.defineClass` / `defineHiddenClass` / `Unsafe.defineClass` bodies) mints the new class's mirror through the fallible door and the reclaim ladder before the one-attempt door raises `OutOfMemoryError`. |
| `CRATONVM_ZGC_OOME_TRACED_POOL_ALL_KINDS` | off (`=1` opt-in) | *zt t6/rv3.* ZGC only, with `CRATONVM_ZGC_OOME_TRACED_POOL` on: a VM-raised `OutOfMemoryError` of another kind that has a preallocated default (`Metaspace`, `Requested array size exceeds VM limit`; `CRATONVM_GC_PREALLOCATED_OOME_KINDS`) is also served from the traced pool, its `detailMessage` pointing at the default's, as HotSpot's `Universe::gen_out_of_memory_error` does. Off: those take a traced slot and are constructed fresh. Deciding row in `t6-rv3-report.md`. |
| `CRATONVM_ZGC_OOME_TRACED_POOL` | **on** (`=0` kill) | *zs s9/la.* ZGC only (with `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE` on): the first four VM-raised `Java heap space` errors are preallocated at boot and handed out with a fresh stack trace, as HotSpot's `_preallocated_out_of_memory_error_array`, instead of being constructed on the heap that just refused; the constructed ones became garbage the free list reused. Java-visible shape unchanged (`Zs9LaOomeTrace` SAME on both arms). |
| `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS` | off (opt-in; `1`/`on` = floor 2048, a number >= 16 = that floor, any other value = 2048) | *zs s10/sp.* ZGC only: after the final refusal of a small request of S bytes (the one an `OutOfMemoryError` is built for), the low free list serves no request from a block smaller than `max(S, floor)` until a collection changes the picture by `max(64 KiB, capacity/64)` (the bumped bytes go down by that much, or what the arena can serve grows by that much). Blocks stay listed (nothing leaks); fresh bump space and blocks of at least the threshold still serve, and a block that serves keeps serving from its rest. Every small-object exit honours it (low free list, bump tail, high-end last resort, the smallest-block refill, `room_for_last_refusal`); only `Java heap space` errors arm it. HotSpot's retired-TLAB analogue: after a failed refill no small hole serves the next request. `16` is the literal rule (blocks smaller than the refused request). *zt t1/sp review:* engagement lines `[zgc-post-refusal] sub-request-spans armed|disarmed` under `CRATONVM_DBG_ZGC_POST_REFUSAL`; deciding rows in `docs/internal/zgc-round-zt-20261018/t1-sp-report.md`. |
| `CRATONVM_ZGC_REFUSED_FIT_RELISTS` | **on** (`=0` kill) | *zt t2/al.* ZGC only: when the commit of a span a free-list fit has just taken is refused by the OS (a granule the uncommitter gave back), the span goes back on its free list (low or high end) instead of sitting in no list until a slide rebuilds the free list (`Arena::hand_out_fit`; `take_smallest_block_at_least` already did this). The allocation still fails, as before. Engagement: `Arena::refused_fits_relisted`. Page: `docs/internal/gc/zgc-zt1sp-a-refused-commit-on-a-free-list-fit-drops-the-block-FIXED-20261019.md`. |
| `CRATONVM_ZGC_VERDICT_RECORDS_REFUSAL` | **on** (`=0` kill; inert unless `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR` is on) | *zt t2/al.* ZGC only: an exhausted-verdict refusal in `try_alloc_object` / `try_alloc_array` records its request size and the collection count in the refusal record `alloc_raw`'s failure arm writes, so `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`' final-refusal rule arms on fills the verdict ends, and `room_for_last_refusal` / the `jdk.ZAllocationStall` size read the verdict's request. Proposal: `docs/internal/proposals/gc/zgc-zt1sp-proposal-verdict-refusals-record-the-request-20261019.md`. |
| `CRATONVM_ZGC_CHUNK_ZERO_DESCENDING` | off (opt-in; `1`/`on` = 16 KiB pieces, a number = that many KiB, clamped to 4..256) | *zt t2/al.* ZGC only: `carve_tlab_chunk` zeroes the chunk in pieces from the top down instead of one ascending `memset`, so the chunk's low end, where the bump starts, is the last written and the likeliest to be in L1/L2 when the first headers are stored; each piece also stays below the size where a `memset` turns non-temporal. The same bytes are zeroed (pristine and slide-zeroed ranges are still skipped), so nothing a reader can see changes; there is no soft `Tlab::end` (what retired the removed `ZGC_LAZY_TLAB_ZERO` switch). Engagement: `[GC] zgc-tlab-zero-descending: piece_bytes= chunks=` under `CRATONVM_GC_STATS`. Deciding rows: `docs/internal/zgc-round-zt-20261018/t2-al-report.md`. |
| `CRATONVM_ZGC_UPCALL_ARGS_COLLECT` | **on** (`=0` kill) | *zs s3/nd3.* ZGC only (with `CRATONVM_ZGC_FFM_MINT_COLLECT` on): an FFM upcall's by-value group argument (its per-upcall arena, session and segment; the native block is freed after the arena's close instead of through a session record) and the generic invoker's `Object[]` and primitive boxes take the fallible door and the reclaim ladder, then the upcall's uncaught `OutOfMemoryError`; the one-attempt doors aborted the process. The framework definers' mirrors (`cglib` enhance, `Proxy.getProxyClass`, the service loader) now use `CRATONVM_ZGC_DEFINE_MIRROR_COLLECT` too. |
| `CRATONVM_ZGC_UPCALL_CONTAIN_OOM_UNWIND` | **on** (`=0` kill) | *zs s5/nd5.* ZGC only: the heap-exhaustion unwind of a refused one-attempt allocation inside an FFM upcall's dispatch is stopped below libffi's closure entry and becomes the upcall's uncaught `OutOfMemoryError` (`Unrecoverable uncaught exception encountered`, exit 1, as HotSpot), instead of crossing the trampoline and aborting (`fatal runtime error: failed to initiate panic, error 3`). |
| `CRATONVM_ZGC_LAYER_MODULE_LOADER_OWNED` | **on** (`=0` kill) | *zs s5/nd5.* ZGC only: a user class loader's `java.lang.Module` recorded by `Module.defineModule0` is held weakly and pinned to its loader (root row `layer-modules`), so it dies with the loader; the permanent JNI global root kept every such loader and its classes alive. |
| `CRATONVM_ZGC_LAYER_MODULE_PRUNE_ROWS_FIRST` | **on** (`=0` kill) | *zs s7/nd6.* ZGC only: the prune of dead user-loader modules drops their package rows before it frees their weak handles, and a lookup resolves its handle under the map's read guard, so a freed (reused) handle is never answered as a `Module`. `=0` restores the s5 prune order only: the read-guard resolve stays (unconditional since s7; it only narrows the window). |
| `CRATONVM_ZGC_DEFINE_MIRROR_NAME_FROM_BYTES` | **on** (`=0` kill) | *zs s9/nd8.* ZGC only: a nameless define (`defineClass(null, bytes, ..)`, `Unsafe.defineClass(null, ..)`) parks the class file's own `this_class` name in the reserved mirror, so nothing is allocated for the mirror after the define. |
| `CRATONVM_ZGC_DEFINE_MIRROR_HIDDEN_NAME_PARKED` | **on** (`=0` kill) | *zs s9/nd8.* ZGC only: a hidden define parks its mangled name (unpooled) in the reserved mirror and the adoption reuses that String. Needs `CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION`. |
| `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED` | **on** (`=0` kill) | *zs s7/nd6.* ZGC only: `ClassLoader.defineClass1/2` reserve the new class's `java.lang.Class` object (and intern its name) through the reclaim ladder before the define and adopt it after; a spent ladder is `OutOfMemoryError: Java heap space` with nothing defined (HotSpot's outcome), so a retry defines the class instead of `LinkageError: duplicate class definition`. |
| `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER` | **on** (`=0` kill) | *zs s8/nd7.* ZGC only (with `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED` on): the other definers reserve the mirror before the define too -- `ClassLoader.defineClass0` (`Lookup.defineClass` / `defineHiddenClass`), the `--compatible` `ClassLoader.defineClass` / `defineClassN` backend / `Unsafe.defineClass` / `defineAnonymousClass` / `URLClassLoader` local define / `Lookup.defineClass` / `defineHiddenClass[WithClassData]` bodies, and the service loader's embedded-jar define; the protection domain is re-read across the reservation. A spent ladder is `OutOfMemoryError: Java heap space` with nothing defined. |
| `CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION` | **on** (`=0` kill) | *zs s8/nd7.* ZGC only: a reserved mirror parks its class name unpooled (or the String already pooled) and interns it when the mirror is adopted, after the define succeeded; a define that fails leaves no string-pool entry for its name. `=0`: the name is pooled at the reservation (zs s7/nd6). |
| `CRATONVM_DBG_ZGC_DEFINE_MIRROR` | off (diagnostic) | *zt t1/nd.* ZGC: at every refused define-mirror reservation, print the cumulative `[GC] zgc-define-mirror: reserved=<n> refused=<n> adopted=<n>` census on stderr (`vm_object::zgc_define_mirror_line`); `refused>=1` is the positive control of the `Zt1NdDefineRetry` / `Zt1NdLookupDefineRetry` rows, and `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0` prints nothing. Changes no behaviour. |
| `CRATONVM_ZGC_EMBED_BOOT_LATCH` | **on** (`=0` kill) | *zs s7/nd6.* A per-VM boot-finished latch (`cratonvm_embed::finish_boot`, `preallocate_out_of_memory_error`) ends the window in which a refused start-up object is reported as a VM initialisation failure, for an embedder that never raises the init level to 4. |
| `CRATONVM_PROXY_MODULE_PER_LOADER` | **on** (`=0` kill) | *zs s5/nd5.* Every collector, both modes: `Class.getModule()` of a user loader's proxy class answers the dynamic module its `ProxyBuilder` defined (HotSpot's object identity); a module minted for one is filed per loader instead of in the permanent by-name module table, which kept every proxy loader alive (~6.5 KB per `Proxy.getProxyClass` with a fresh loader). |
| `CRATONVM_REFLECTION_LISTING_LOADS_COMPONENT` | **on** (`=0` kill) | *zs s3/nd3.* Every collector, both modes: `getDeclaredFields0` / `getDeclaredMethods0` / `getDeclaredConstructors0` load `Field` / `Method` / `Constructor` before allocating their result, so the first listing of a VM that had not loaded the element class answers a typed array, not an array over class id 0. |
| `CRATONVM_ZGC_BOOT_OOME_INIT_ERROR` | **on** (`=0` kill) | *zs s2/nd2.* ZGC only: a start-up object the heap refuses before `main` (the synthetic `System.out`/`System.err` pair, `Class.EMPTY_CLASS_ARRAY`) ends the boot as HotSpot does (`Error occurred during initialization of VM` / `java.lang.OutOfMemoryError: Java heap space` on stdout, exit 1) instead of `abort()` (134). |
| `CRATONVM_LAUNCHER_PREMAIN_EXCEPTION_DESCRIBE` | **on**, every collector (`0` / `false` / `off` / `no` kill) | *zs s2/nd2.* A throwable raised before `main` is entered (main class `<clinit>`, instance-main receiver allocation or constructor) is reported as `java.c`'s `ExceptionDescribe` does: `Exception in thread "<name>" ` from the VM, then `printStackTrace()` with whatever it throws discarded. On the full heap of `Zs1NdLauncherOome` HotSpot 25 prints only that prefix (no newline). It was the ZGC heap only (and `1` for every collector) until the zs / dev merge joined it with dev's launcher reporting (`vm-cli/src/main.rs` `premain_exception_describe`); text fixed by zt t1/ob. Kill: the uncaught dispatch. |
| `CRATONVM_WINDOWS_PROCESS_CPU_TIME` | **on** (`=0` kill) | *zt t1/ob.* On Windows `OperatingSystemMXBean.getProcessCpuTime()` answers `GetProcessTimes`' kernel + user time in nanoseconds, as the JDK's Windows `OperatingSystemImpl` does; it answered -1 ("unsupported"). Linux is unchanged (`/proc/self/stat` ticks, the JDK's `times()` granularity). `=0`: -1 on Windows. |
| `CRATONVM_WINDOWS_OS_MEMORY_METRICS` | **on** (`=0` kill) | *zt t3/ob.* On Windows `OperatingSystemMXBean.getTotalMemorySize()` / `getFreeMemorySize()` / `getTotalSwapSpaceSize()` / `getFreeSwapSpaceSize()` answer `GlobalMemoryStatusEx`'s `ullTotalPhys` / `ullAvailPhys` / `ullTotalPageFile` / `ullAvailPageFile`, and `getCommittedVirtualMemorySize()` the process's `PagefileUsage`, the JDK's Windows `OperatingSystemImpl` figures; they answered -1. Linux is unchanged. `=0`: -1 on Windows. |
| `CRATONVM_LAUNCHER_OTHER_GC_OFF_NOOP` | **on** (`=0` kill) | *zt t3/ob.* `-XX:-UseG1GC` / `-XX:-UseZGC` after a `-XX:+Use<Name>GC` that selected another collector change nothing, as on HotSpot 25 (`-XX:+UseZGC -XX:-UseG1GC` runs ZGC, and its ZGC thread-flag refusals apply); with no collector selected, or naming the selected one, they still select Generational. `=0`: either flag selects Generational wherever it stands. |
| `-XX:AllocatePrefetchStyle`, `-XX:AllocatePrefetchLines`, `-XX:AllocateInstancePrefetchLines`, `-XX:AllocatePrefetchDistance`, `-XX:AllocatePrefetchStepSize` | accepted, **no effect** | *zt t3/ob (t2/ca P1).* HotSpot C2's TLAB-allocation prefetch knobs. The launcher accepts them silently (as HotSpot accepts a valid value) and nothing reads them; the JIT's own allocation prefetch is the opt-in `CRATONVM_JIT_ZGC_ALLOC_PREFETCH` family. A malformed or out-of-range value is not refused (HotSpot refuses it). See `docs/internal/proposals/gc/zgc-zt2ca-proposal-hotspot-prefetch-flags-and-retiring-flat-opt-ins-20261019.md`. |
| `CRATONVM_ZGC_NEAR_FULL_PROGRESS` | **75** (percent; `=0` kill) | *zr z6/tp (2026-10-13).* Near a full ZGC heap (post-collection live set at or above the 75% threshold) an occupancy-triggered collection needs `max(1 MiB, <percent>% of the free space)` of new allocation since the last one, instead of `max(free/4, 64 KiB)`, so collections no longer come every ~5% of free-space shrinkage. Allocation failures still collect at once (refusal ladder), so the OOME point does not move. `[GC] zgc-pacing:` shows the rule and window of each near-full collection. |
| `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR` | **off** (`<KiB>` or `on` = 2048 KiB) | *zs s6/no (2026-10-15), opt-in.* The exhausted verdict: after an allocation refusal's collection (the ladder doors and the clear-soft rung) that left under the floor free, the refusing thread's next fallible attempts are refused until the next collection or until its `OutOfMemoryError` is built, so the ladder runs its clear-soft rung and throws. HotSpot's ZGC stall fails after a collection that freed no 2 MiB page; without the verdict this VM collects once per sliver of free space down to the last object (~30 whole-heap collections on the ProxyBoxing / `Zr2CpCapacity grow` shape). Moves the OOME point earlier by at most the floor, toward HotSpot's. zs s8/rv5: the compiled allocation doors (`jit-helpers`: `new`, `newarray`, `anewarray`, the box door) have no clear-soft rung, so there the verdict is armed only while no soft reference is active (armed over one, the compiled allocation threw with soft references uncleared). zs s9/no2: one verdict per fill. A verdict whose ladder threw is spent, and none arms again until a collection leaves the floor free, so a program that keeps allocating after its OOME runs the default ladder down to object granularity (the s6a L4 `Zs1NdLauncherOome -Xmx32m` regression). `exhausted_armed=` / `exhausted_refused=` / `exhausted_spent=` on `[GC] zgc-refusal:`. |
| `CRATONVM_ZGC_NEAR_FULL_FLOOR_FITS` | **off** (`=1` opt-in) | *zs s6/no (2026-10-15).* A near-full floor (`CRATONVM_ZGC_NEAR_FULL_PROGRESS`) that would exceed the free space (its 1 MiB minimum below ~1.33 MiB free) falls back to `max(legacy, <percent>% of free)`, so an occupancy collection runs before the arena is exhausted and a native door that never collects does not meet a heap full of garbage (the zr7rv6 page's mitigation 1). Costs collections in the last MiB. `floor_fitted=` on `[GC] zgc-refusal:`. |
| `CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE` | **on** = 60 (`=0` kill; `<permille>` sets the floor) | *zs s9/no2 (2026-10-16); default ON since zt (2026-10-19): on the local HotSpot 25.0.3 box `Zs6NoSoftNearFull 95` turns SAME (3/3, both modes), 90 / 80 and the regression set stay SAME, interleaved A/B 0.88-1.01x CPU.* An allocation-driven ZGC collection (not `System.gc()`) whose previous cycle left under the given permille of the heap free asks every active soft reference to be cleared. This stands in for HotSpot ZGC's stalled majors near full, which clear all soft references: on 25.0.3, `Zs6NoSoftNearFull` keeps the cache up to 91 % fill and clears it from 94 %. This VM does not stall there, so `CRATONVM_ZGC_STALL_CLEARS_SOFT` never fires. `near_full_soft_clears=` on `[GC] zgc-refusal:`. |
| `CRATONVM_ZGC_STALL_CLEARS_SOFT` | **off** (`=1` opt-in) | *zs s6/no (2026-10-15).* A ZGC allocation-refusal collection (this VM's allocation stall) asks every active soft reference to be cleared, as HotSpot 25's ZGC does for a collection caused by an allocation stall (`tools/probes/Zs6NoSoftNearFull.java 95`: HotSpot clears the whole cache without an OOME). Off, soft references are cleared only by the LRU rule and the ladder's last-ditch rung before an `OutOfMemoryError`. With it on the rung usually finds no soft reference left and runs no second collection. `stall_soft_clears=` on `[GC] zgc-refusal:`. |
| `CRATONVM_ZGC_JIT_BOX_COLLECTS_LATCHED` | **on** (`=0` kill) | *zs s6/no (2026-10-15).* ZGC only: the compiled box door (`jit_box_cold_alloc`, `Integer` / `Long.valueOf` after a missed inline TLAB bump) runs its forced collection and retry even while the GC-overhead streak is latched, as the interpreter's ladder does. `=0`: a latched streak skips the collection, and the box falls through to a native allocation that throws `OutOfMemoryError` without collecting (with the heap possibly full of garbage the program dropped). Read only after the box's fallible door refused under a latched streak. |
| `CRATONVM_ZGC_RELOCATE_SIZE_TABLE` | **on** (`=0` kill) | *zr z7/pc (2026-10-13).* The ZGC slide reads each low-region survivor's header once, in the page census (`logical_pages`), and keeps its size plus a "no reference slots" bit in a side table aligned with the live set (4 bytes per survivor, freed with the slide). The partition takes sizes from it, and the post-slide reference rewrite and `live_ceiling` screen serve a primitive array without reading its header; an object with reference slots still re-reads its post-slide header (the walkability screen). Engagement: `table_skipped=` on the `CRATONVM_DBG_ZGC_REWRITE=1` line. |
| `CRATONVM_ZGC_MARK_ROOT_FILTER_CONC` | **on** since zr z7 (`=0` kill) | *zr z7/cm (2026-10-13).* The extra-root filter stays armed during the concurrent mark (it was disarmed, so each concurrently marked object took three global `RwLock` reads and a shared counter bump); the mark-end late-owner repair (2026-09-20 round) covers rows removed mid-phase. Concurrent cycles only. |
| `CRATONVM_ZGC_CONC_REF_SKIP` | **on** since dev `43e50d605c` (2026-10-18; `=0` kill) | *zr z1/rf, row added zt t1/mk.* A ZGC concurrent mark cycle hides the referent slot of every ACTIVE Weak / Phantom `Reference` of the VM's reference processor (a mark-start snapshot, refreshed at the mark end), as G1's initial mark does, so those referents can clear at the cycle's end. Sound only with the SATB keep-alive on referent reads (`Reference.get`: interpreter, JIT `jit_getfield`, `native_ref_get`); the `Unsafe.getReference` and JNI `GetObjectField` reads at the referent offset have none yet (`docs/internal/gc/zgc-zt1mk-referent-reads-without-a-keep-alive-under-concurrent-reference-skip-FIXED-20261019.md`). Concurrent cycles only (`CRATONVM_ZGC_CONC_START` / every-trigger); inert on the default stop-the-world ZGC. |
| `CRATONVM_ZGC_CONC_REF_SKIP_SOFT` | **on** (`=0` kill) | *zr z2/cc, row added zt t1/mk.* With `CRATONVM_ZGC_CONC_REF_SKIP`, the ACTIVE Soft rows are hidden too, for the concurrent PHASE only; the mark end re-roots the referents the LRU policy kept (`kept_soft_referent_roots`). `=0` hides weak / phantom only. Engagement: `soft_rows=` / `soft_roots=` on `[GC] zgc-markend:`. |
| `CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK` | **on** (`=0` kill) | *zt t1/mk (2026-10-19).* A collection run for a FULL request -- `System.gc()`, JVMTI `ForceGarbageCollection`, a VM-internal full collection (the doors that set `gc_quiescence::request_major_gc` on the collecting thread) -- that finds a concurrent cycle open ABANDONS it and marks from scratch, instead of closing it with its snapshot-at-the-beginning mark set (which keeps every object that died during the phase, and the referent of every `Reference` registered during it). HotSpot ZGC serves `System.gc()` with a cycle that starts after the call. The collection is then exactly the stop-the-world fallback a refused certification already takes. Engagement: `[GC] zgc-conc-abandon: cycle= cause=full-request abandons=` under `--verbose:gc`, `ZgcRealHeap::full_request_conc_abandons()`. Concurrent cycles only; one TLS read per collection otherwise. *zt t2/x1:* every ZGC collection under the switch consumes the request (the `System.gc()` door withdraws it only when it did not collect, and nothing else on ZGC consumed it), so a won `System.gc()` no longer leaves a flag that abandoned every later cycle that thread collected. |
| `CRATONVM_ZGC_FULL_REQUEST_CLOSES_OPEN_CYCLE` | **on** (`=0` kill) | *zt t3/mk (2026-10-19).* A full request (`System.gc()`, JVMTI, the VM-internal full doors; `force_full_collection`) that finds a concurrent cycle open first lets that cycle finish as its OWN collection (one pause on the allocation door, closing it with its mark set as an allocation-triggered collection does), then runs the requested collection, which has no cycle left to close and marks from scratch. HotSpot 25 queues the request behind the running major cycle (`ZDriverPort::send_sync`), so `GarbageCollectorMXBean.getCollectionCount()`, `-Xlog:gc` and `jdk.GarbageCollection` move by 2 there; with only `CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK` they moved by 1 (x1 finding X1-9). It also closes the lost-election hole (the fresh-mark trigger is the requester's thread-local, so a peer's winning pause closed the cycle with its SATB set). `CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK` stays as the fallback for a cycle that opens between the two pauses. `=0`: the fresh-mark abandon alone. Concurrent cycles only. |
| `CRATONVM_ZGC_ROOT_FILTER_REPAIR_OWN_ARENA` | **on** (`=0` kill) | *zr z8/rv7 (2026-10-13).* With the extra-root filter armed across a concurrent ZGC mark (`CRATONVM_ZGC_MARK_ROOT_FILTER_CONC`), the mark-end repair declines to certify the mark set (a stop-the-world re-mark, `parallel_mark_fallbacks`) when a pin-table row present at mark start is gone. The tables are process-wide; with this on, only a row whose owner lies in this heap's arena counts, so another VM in the same process collecting or tearing down during the phase no longer forces the re-mark. `=0` counts every row in the process. Inert in a single-VM process and without a concurrent cycle. |
| `CRATONVM_ZGC_ARENA_NO_THP` | **OFF** (`=1` opt-in) | *zr z7/rs (2026-10-13).* Advise the ZGC arena reservation `MADV_NOHUGEPAGE` at heap construction (Linux; a no-op elsewhere; skipped when `CRATONVM_HEAP_THP` asks for huge pages). Matters only on a host whose `/sys/kernel/mm/transparent_hugepage/enabled` is `always`, where the first touch of a granule faults a whole 2 MiB page in; HotSpot's ZGC heap is a `memfd` mapping that mode does not cover. `[GC] zgc-footprint-spans:` (`CRATONVM_DBG=gc-stats`) prints the outcome as `arena_no_thp=` (0 unset, 1 applied, 2 refused, 3 yielded) beside the arena and bitmap address ranges that `tools/rss-by-mapping.sh` attributes. Page: `docs/known-issues/footprint/boot-rss-by-owner-20261013.md`. |
| `CRATONVM_ZGC_SIDE_TABLE_NO_THP` | **on** (`=0` kill) | *zr z10/mt (2026-10-13).* The ZGC object-start, mark and old-start bitmaps (1/64 of `-Xmx` each, written sparsely: a bit per object start or mark) of at least 1 MiB (a 64 MiB heap) go on private anonymous mappings advised `MADV_NOHUGEPAGE` (`gc::reservation::map_side_table`) instead of `alloc_zeroed` blocks in the process allocator. In the `cratonvm` launcher that allocator is mimalloc, whose default eager arena is `madvise(MADV_HUGEPAGE)`d, so on a THP=`madvise` host every 2 MiB of a bitmap that one bit reached was resident whole. The mapping is zero-filled 4 KiB at a time; the bitmap's words, geometry and every reader are unchanged, and a refused mapping falls back to `alloc_zeroed`. Linux only, ZGC only (GenGC and G1 build their bitmaps as before). Engagement: `side_maps=<n>` on `[GC] zgc-footprint-spans:` (`CRATONVM_GC_STATS`), and `tools/rss-by-mapping.sh` then finds each bitmap as its own `gc:bitmaps` mapping. Report: `docs/internal/zgc-round-zr-20261012/z10-mt-report.md`. |
| `CRATONVM_ZGC_SWEEP_SIZE_HINTS` | **on** (`=0` kill; needs `CRATONVM_ZGC_RELOCATE_SIZE_TABLE`) | *zr z7/pc (2026-10-13).* When a slide is expected, the ZGC bitmap sweep records each low-region survivor's size cell (base + 4-byte cell, ~12 bytes per survivor, freed with the slide) and the slide's page census takes it instead of reading the header a second time. A hint is used only for the exact base it names, so an arm that records nothing costs a header read, never a wrong size. Engagement: `census_hinted=` on the `CRATONVM_DBG_ZGC_REWRITE=1` line. |
| `CRATONVM_ZGC_SELECTIVE_RELOCATE` | **on as the cycle gate `=2`** since zs s2 (`=0` kill; `=1` selects the z9 per-page arm; knobs `CRATONVM_ZGC_SELECTIVE_RELOCATE_HOLE_KIB`, default 64, and `CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT`, default 25) | *zr z9/sl (2026-10-13).* The low slide already evacuates only sparse pages (live under 25 %, widened by `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT`), but on this collector a sparse page's garbage is already on the free list after the sweep, so moving its survivors buys only contiguity -- and every slide that moves anything rewrites the reference slots of the WHOLE live set. With the switch on, a page with survivors stays in place unless at least `FRAG_PCT` % of its extent is free in holes shorter than `HOLE_KIB` (holes the TLAB carve and large requests cannot use); the cost gate prices the slide by that fragmented garbage, and a selection left with no survivor declines the slide outright. Never after an allocation failure: that slide runs the full selector (the near-OOM fallback). Engagement on `[GC] zgc-selective:` (`sel_slides`, `sel_pages_kept`, `sel_emptied`, `sel_failure_bypass`, `sel_bytes_moved`); `moved_bytes=` on `[GC] zgc-relocate:` is each slide's copy volume on every arm (`bytes_copied=` on `[GC] zgc-real:` is the survivor total, not a copy). |
| `CRATONVM_ZGC_SELECTIVE_RELOCATE=2` (or `cycle`) | **default** (unset = `=2` since zs s2) | *zs s1/sl (2026-10-14); default ON zs s2: host A/B on `cvm-s1b`, 7 interleaved rounds: `Zr3MtMarkShapes hubs` -9 % CPU, `Zr7PcCollectionCpu prim` -2 %, `MtChurnProbe` +0.7 %, `Zr2CpCapacity grow` +1.3 %, `Zr3JaRefArrayAlloc` equal; every stdout equal; `=0` kills.* The cycle gate: one keep/evacuate decision per CYCLE instead of per page. The slide runs the default selector unfiltered when the candidate pages hold fragmented garbage of at least 1/32 of the low live bytes (the cost gate's ratio), or when the low bump headroom is under a quarter of capacity (the allocator is already living on holes); otherwise the slide is declined outright (no copy, no rewrite). Rationale: the rewrite is paid once per slide, so once a slide runs, leaving sparse pages in place only gives up their contiguity; and pages kept in place were what made the z9 per-page arm (`=1`, unchanged) +12 % CPU on `MtChurnProbe`. Engagement: `sel_cycle_admit`, `sel_cycle_decline`, `sel_cycle_tight` on `[GC] zgc-selective:`. |
| `CRATONVM_ZGC_SELECTIVE_FUSED_FRAG` | **on** (`=0` kill) | *zs s1/sl (2026-10-14).* Only with `CRATONVM_ZGC_SELECTIVE_RELOCATE` on. The fragmented-garbage figure both selective modes read is booked inside the page census's own pass over the live set (from the size it already reads) rather than by a second whole-live-set walk. `sel_fused` on `[GC] zgc-selective:` counts the slides that used it. |
| `CRATONVM_ZGC_SLIDE_SIZE_FROM_TABLE` | **on** (`=0` kill) | *zs s1/sl (2026-10-14).* The low slide's copy loop sizes each moved survivor from the size cell the page census recorded (`CRATONVM_ZGC_RELOCATE_SIZE_TABLE`) instead of reading its header again: one of the four header reads of a moved object. Same sizes; `=0` restores the header read. |
| `CRATONVM_ZGC_SLIDE_UNSIZABLE_OBSTACLE` | **on** (`=0` kill) | *zs s1/sl (2026-10-14).* An unsizable survivor on a page the slide does not evacuate is an obstacle up to the bump cursor, so the placement probe chooses no destination at or above it (its body has no known end, and the census books it as 0 live bytes). Before, the post-slide ceiling screen failed closed only after the copy. Only reachable with a header the collector cannot size. |
| `CRATONVM_ZGC_SLIDE_PIN_BY_BASE` | **on** (`=0` kill) | *zs s2/sl2 (2026-10-14).* The low slide's pin screen also drops the page of the OBJECT a pin lies in (an interior pin on page k+1 into an object based on page k used to let the object move), and the high slide honours a pin one past an object's end, the rule `resolve_interior_for_pin` already applies. Engagement: `pins_by_base=` on `[GC] zgc-relocate:`. |
| `CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL` | **on** (`=0` kill) | *zs s2/sl2 (2026-10-14).* The relocation-set telemetry (JFR `jdk.ZRelocationSet`, `targeted_pages=`, the pressure tier's selected live bytes) books the set the slide relocates, after the pin screen, the selective cycle gate and the cost gate; a declined cycle books `relocate=0`. `=0` books the selector's answer as before. |
| `CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS` | **on** (`=0` kill) | *zs s2/sl2 (2026-10-14).* The cost gate and the selective empty-set decline exempt a targeted cycle (`CRATONVM_ZGC_TARGETED_COMPACTION`) only while a targeted page survived the pin screen. `targets_pin_dropped=` counts the cycles it changed. |
| `CRATONVM_ZGC_VM_PAUSE_CPU` | **on** (`=0` kill) | *zt t6/ob4.* The VM's side of each ZGC collection pause on the collecting thread -- the safepoint request and rendezvous before `collect_garbage`, and the common epilogue and event emission after it -- is booked as its own share, `vm_pause_ms=` at the end of the shutdown `[GC] zgc-cpu:` line and inside `collection_ms=` (the heap's own `pause_thread_ms=` part of the span is subtracted, so the shares stay disjoint). Not in the per-cycle `[GC] zgc-pause-cpu:` line, which is printed before the epilogue. Needs `CRATONVM_ZGC_MARK_CPU_ACCOUNTING`. G1 / Generational: nothing. `=0`: not booked (`vm_pause_ms=0`). |
| `CRATONVM_ZGC_THREAD_CPU_CYCLES` | off (opt-in `=1`) | *zt t6/ob4.* On Windows x86-64 the GC-thread CPU clock behind `[GC] zgc-cpu:` / `[GC] zgc-pause-cpu:` (`mark::thread_cpu_nanos`) reads `QueryThreadCycleTime` scaled by the TSC rate, measured once per process against `QueryPerformanceCounter` (~2 ms at the first span edge), instead of `GetThreadTimes`, which advances in ~15.6 ms quanta and reads a shorter worker span as 0 or one quantum. Opt-in because Microsoft documents the cycle count as not convertible to time (exact on an invariant TSC). Linux and other targets: no effect. |
| `CRATONVM_ZGC_SLIDE_EVACUATE` | off (opt-in `=1`) | *zs s2/sl2 (2026-10-14).* The low slide EVACUATES the selected pages' survivors into the bump tail (the arena's to-space window) and frees each selected page whole, instead of sliding them down in place; screen, rewrite, registry and `PointerMap` are the slide's. Only on a cycle with tail headroom for the whole set and no failure / pressure tier; else the slide runs. `evac=` on `[GC] zgc-relocate:` (evacuated / kept on the slide / to-space stops / bytes / objects freed one by one). |
| `CRATONVM_ZGC_REWRITE_SIZE_FROM_CELL` | off (opt-in `=1`) | *zs s2/sl2 (2026-10-14).* The slide's rewrite sizes a survivor with reference slots from its pre-slide size cell instead of re-reading its header (one `alloc_size` per referencing survivor). Gives up the walkability screen's independent size read; `cell_sized=` on `[zgc-rewrite]`. |
| `CRATONVM_ZGC_EVAC_QUARANTINE` | off (opt-in `=1`) | *zs s3/cr (2026-10-14).* With `CRATONVM_ZGC_SLIDE_EVACUATE=1`, the spans an evacuating slide vacated are zeroed and WITHHELD from the free list until the next collection, which releases them after its mark and before its sweep. A span something already claimed is dropped, not listed twice. This is stage 2 of the concurrent-relocation plan. `quarantine=` on `[GC] zgc-relocate:` (spans held / bytes held / spans released / bytes released / spans dropped). |
| `CRATONVM_ZGC_SELECTIVE_GATE_ON_SELECTION` | off (opt-in `=1`) | *zs s3/cr (2026-10-14).* The `CRATONVM_ZGC_SELECTIVE_RELOCATE=2` cycle gate prices the fragmented garbage of the pages the slide actually selected (the budget prefix after the pin screen) instead of every page under the occupancy cutoff. It can only decline more. `gate_sel=` on `[GC] zgc-relocate:` (cycles priced / cycles the old input admitted and this one declined). |
| `CRATONVM_ZGC_RELOCATE_POOL` | off (opt-in `=1`) | *zs s3/cr (2026-10-14).* The relocation passes (page census, partition, rewrite and screen) run on a persistent per-heap worker pool (`zgc-reloc-N` threads) instead of spawning scoped threads per pass. The answers are the same. `par=` on `[GC] zgc-relocate:` (parallel passes / threads spawned / passes pooled). *zt t1/rl:* the pool is per heap and no longer feeds the process-wide `[GC] evac_pool:` census; its own census is `pool=` on `[GC] zgc-relocate:` (dispatches / inline runs / widest dispatch / width). |
| `CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE` | off (opt-in `=1`) | *zt t3/rl (2026-10-19).* Each give-back of arena granules (the collection-start one and the `ZUncommitter`'s) also gives back the pages of the object-start registry and of the mark bitmap that cover only the released bytes: a 2 MiB granule is 32 KiB of each table. A page goes back only after every word of it read zero, under the arena lock (the released span holds no object, so no marker writes there even with a concurrent cycle open), and only as whole kernel pages (zt t4/x3: on a 16 / 64 KiB-page kernel a 4 KiB run is left alone). Linux only (the tables live on mappings of their own there). `un_bitmap_released=` on `[GC] zgc-uncommitter:` (bytes). |
| `CRATONVM_ZGC_UNCOMMITTER_WINDOWS_PAUSE_ONLY` | off (opt-in `=1`) | *zt t1/rl (2026-10-19).* On Windows the `ZUncommitter` thread does not start: its reset is `VirtualFree(MEM_DECOMMIT)` + `VirtualAlloc(MEM_COMMIT)`, a window in which a granule faults while mutators run. Idle granules are still stamped by the sweep and released by the collection-start give-back, with the world stopped (the behaviour before zr z4/un). An idle Windows process then keeps its memory until its next collection. Ignored on Linux and macOS. `un_running=0` on `[GC] zgc-uncommitter:`. |
| `CRATONVM_ZGC_POINTER_MAP_SORTED` | off (opt-in `=1`) | *zt t1/rl (2026-10-19).* The low slide hands its `from -> to` pairs over as the cycle's `PointerMap` in a SORTED form (one vector, 16 bytes a move) instead of hashing every move into 16 shards (~19-38 bytes a move plus one insert each, inside the pause). A lookup becomes a binary search; a consumer that mutates the map converts it back. Same answers. Deciding rows: `un_slide_map_peak` on `[GC] zgc-uncommitter:` against maxrss, and user+sys CPU, on `-Xmx256m MtChurnProbe 4 60 48` and `Zr3FpRss steady`, 6 interleaved pairs. |
| `CRATONVM_ZGC_RELOCATIONS_CEILING_SCALED` | off (opt-in `=1`) | *zt t1/rl (2026-10-19).* The ceiling at which the forwarding table (`counters.relocations`) is cleared follows the heap: one entry per 256 bytes of capacity, at least 256 Ki entries, never above the absolute 4 M (1 M at `-Xmx256m`; 4 M from 1 GiB up). It can only lower the bound. A clear degrades the barrier for older moves to the stamped-word path. Deciding row: `fp_relocations_peak` and maxrss on `-Xmx256m Zr3FpRss steady`, with no new `CRATONVM_DBG_VACATED_FRAMES` stale reads. |
| `CRATONVM_DBG_ZGC_QUARANTINE_DEAD_CHECK` | off (debug `=1`) | *zt t1/rl (2026-10-19).* With `CRATONVM_ZGC_EVAC_QUARANTINE=1`, each release of the quarantine first walks every registered object's reference slots and counts the words that still name an address inside a held span: a slot the slide's rewrite missed, which stage 3 (remap in the next mark) would have to heal. A census, not a gate. `qdead=` on `[GC] zgc-relocate:` (releases checked / objects walked / stale words). O(registered slots) per release. |
| `CRATONVM_ZGC_RELOCATE_REFUSES_RAW_JNI_LOCALS` | off (opt-in `=1`) | *zt t1/rl (2026-10-19).* ZGC refuses a slide while a thread of its VM holds RAW JNI local references (`CRATONVM_JNI_INDIRECT_LOCALS=0`, or a thread's raw escape), whose C copies a slide cannot rewrite; the JNI dispatch keeps that count for it. Unconditional like the retained-TLAB refusal (no compiled frame needed), and not licensed by `CRATONVM_ZGC_ASSUME_REWRITABLE`. `[GC] zgc-relocation-skip-reason: raw-jni-locals-open=`. Default arm: locals are indirect, so no holder exists outside a raw escape. |
| `CRATONVM_DBG_ZGC_EVAC_BLOCK_SPANS` | off (debug `=1`) | *zs s3/cr (2026-10-14).* Every span the evacuating slide would free is treated as screen-blocked, so each moved survivor's old extent is freed on its own. It is the positive control of that net (`evac=` fifth field, `evac_forced_blocks=`). Reclaims less, never more. |
| `CRATONVM_ZGC_COST_GATE_EXEMPT_TIGHT` | off (opt-in `=1`) | *zs s1/sl (2026-10-14).* The relocation cost gate does not decline a cycle whose selector `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT` widened for a tight bump headroom. The tier measures the bump headroom, the gate's own tightness leg `arena.remaining()` (holes included), so on a mosaic the gate can decline the selection the tier widened. Opt-in: it only adds slides. |
| `CRATONVM_ZGC_SELECTIVE_BYPASS_ON_LATCH` | **on** (`=0` kill) | *zr z11/rv9 (2026-10-13).* Only with `CRATONVM_ZGC_SELECTIVE_RELOCATE` on. The selective arm's near-OOM fallback (the full selector after an allocation failure) reads the cycle's allocation-failure latch itself. z9 read the pressure tier, which is `None` under `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT=0`, so with that kill set the failure collection still kept the long-hole pages and the large request failed again. `=0` restores the z9 read. |
| `CRATONVM_ZGC_CONC_RATE_EXCLUDE_PAUSE` | **on** (`=0` kill) | *zr z11/rv9 (2026-10-13).* Only with `CRATONVM_ZGC_CONC_START=auto` and `CRATONVM_ZGC_CONC_RATE_FROM_ALLOCATION` on. The adaptive window's allocation-rate sample ends its time span at the collection's entry, where its byte count is read, instead of after the pause (a bias of `pause / interval` toward a low rate, which opens the window later). `=0` restores the post-pause span. |
| `CRATONVM_ZGC_CONC_POOL_WEAK_HEAP` | **on** (`=0` kill) | *zr z11/rv9 (2026-10-13).* Only when a concurrent mark runs (`CRATONVM_ZGC_CONC_START`). The concurrent mark pool is spawned over an inert context before the mark-start pause arms anything, and binds the heap WEAKLY for the cycle. Before, the pool held the heap's own `Arc` while the cycle was open, so a heap dropped mid-cycle (an embedder dropping its VM) was never freed and its workers stayed parked; and a failed spawn panicked inside the pause, where it now declines the cycle (stays stop-the-world). `=0` restores the z3 pool. |
| `CRATONVM_ZGC_CONC_START_ZERO_KILLS` | **on** (`=0` kill) | *zr z11/rv9 (2026-10-13).* Under `CRATONVM_ZGC_CONC_EVERY_TRIGGER=1` an unset `CRATONVM_ZGC_CONC_START` is seeded to 60 ("concurrent marking configured"); an EXPLICIT `CRATONVM_ZGC_CONC_START=0` now stays the kill switch the round's docs name, where z3 seeded it to 60 as well. `=0` restores the z3 seed. |
| `CRATONVM_ZGC_CONC_WINDOW_STALE_FIRE_AT_EMPTIES` | off (opt-in `=1` restores) | *zr z11/rv9 (2026-10-13).* Only with `CRATONVM_ZGC_RATE_TRIGGER=1` and a numeric or `auto` `CRATONVM_ZGC_CONC_START`. A NON-zero director point at or below the cycle's watermark (a live set above `-XX:SoftMaxHeapSize`'s high-usage point) no longer empties the concurrent-start window; it drops out of the nearest-point minimum. `0` (a rule already says yes) still empties it. `=1` restores the empty window. |
| `CRATONVM_ZGC_MARK_SPAWN_BACKOFF` | **on** (`=0` kill) | *zr z11/rv9 (2026-10-13).* After a parallel-mark cycle on which neither the persistent nor the per-cycle pool could be spawned, the next 16 parallel-mark cycles mark serially instead of spawning and joining up to N-1 threads inside each pause (and logging a warning per cycle). `=0` retries every cycle as before. |
| `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` | **on** since zr z8/zb (`=0` kill) | *zr z1/al, flipped by zr z8/zb (2026-10-13).* The ZGC bitmap sweep no longer zeroes the 16-byte header of each dead low-region object (one read-for-ownership miss per dead object; HotSpot writes no dead object either). The free spans are computed from survivors only, every reader of arena bytes is registry or free-list gated, and every hand-out zeroes what it hands out, so no answer changes. With the store gone the dead side of the pass is a popcount per bitmap word, and the region step (`CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`) skips 32 KiB blocks with no survivor. z5a: -20 % on `Zr3JaRefArrayAlloc 1 10 64 prim 400`. Set `=0` on a run hunting a stale reference: the zeroed header is what the "Stale pointer detected" fallbacks and `reclaim_guard`'s `ClassId(0)` gate read. Engagement: `dead_headers_skipped=` on `[GC] zgc-alloc-sweep:`; set explicitly, `--verbose:gc` also prints `[GC] zgc-sweep-skip-dead-headers:` per cycle. |
| `CRATONVM_JMOD_LAZY_INDEX` | **on** (`=0` kill) | *zr z8/fm (2026-10-13). Not a GC switch; listed here with the round's boot-footprint rows.* A JMOD on the boot class path is indexed from its ZIP central directory alone, and an entry's local header is read only when that entry is served (`classloading/src/jmod_index.rs`). `zip::ZipArchive::new` (zip 2.4.2) reads every entry's local header at open, which faults in about one page per entry of a mapped JMOD: about 47 MB of a Hello world's working set on a JDK that ships `jmods/` (70 files). Answers are the same as the `ZipArchive` path's: names, class index, and the bytes of every entry (a unit test compares both arms on every entry of every JDK 25 JMOD). A JMOD the index does not model (ZIP64, encryption, a method other than stored/deflated, a non-ASCII name, a ZIP64/AES/Unicode-path extra field, a prefixed archive) keeps the `ZipArchive` path. Page: `docs/known-issues/footprint/boot-footprint-owners-20261013.md`. |
| `CRATONVM_CLASS_CODE_COMPACT` | **on** (`=0` kill) | *zr z11/md (2026-10-13). Not a GC switch; listed with the round's per-class footprint rows.* At define, every byte view a decoded member attribute holds into the class file (each method's `Code` bytecode, the `StackMapTable` and unknown attributes nested in it, unknown member attributes) is copied into one compact per-class buffer (`compact_member_attribute_views`, `classloading/src/class_manager.rs`), so the class's metadata stops pinning the whole class file: ~0.56 KB kept instead of ~4.1 KB for a java.base class. The bytes are the same; only the buffer behind each view changes. Skipped for a class file backed by an external mapping (a stored JAR entry). The saving shows when the class-bytes cache does not hold the file either (`CRATONVM_BOOT_CLASS_BYTES_REFETCH`, or past the cache's 16 MiB FIFO cap). Page: `docs/internal/gc/zgc-zr8fm-class-metadata-per-class-cost-20261013.md`. |
| `CRATONVM_BOOT_CLASS_BYTES_REFETCH` | **on** (`=0` kill) | *zr z11/md (2026-10-13). Not a GC switch.* A class the boot loader defines from its own class path (the runtime image, a JMOD, `-Xbootclasspath/a`) keeps no copy of its class file in the class-bytes cache; its define-time fingerprint stays. Every reader of a defined class's file (`retransformClasses` / JFR retransform bases, the redefine `ClassFileLoadHook`'s old bytes, `Class.getAnnotatedSuperclass`, a defined class's own `.class` resource, the CDS dump, the debugger's `SourceDebugExtension`) now asks `ClassManager::defined_class_bytes`: the cache, else a re-read through the delegation chain that answers only when the bytes match the fingerprint. HotSpot keeps no class file for these classes either (`InstanceKlass::_cached_class_file` is for a retransformable agent's classes). With `CRATONVM_CLASS_CODE_COMPACT`, ~3.5 KB per boot class; positive control: `ClassManager::class_bytes_refetch_released`. Page: as above. |
| `CRATONVM_LAZY_ZERO_TABLES` | **on** (`=0` kill) | *zs s6/rs (2026-10-15). Not a GC switch; every collector.* The fixed-capacity side tables sized to a class-id bound (the `field_layout` replace counters, the VM statics index, the `getClass` ref-array owners, the lambda-proxy hit bits, the `java/util` and class-init bits, the published-supers arena chunks, the JIT callee negative cache, the lock-slot tables) are allocated zeroed (`alloc_zeroed`: only the pages a slot write reaches become resident) instead of written element by element. Same values. `=0`: every table written in full at its first use, as before. Read once per process. |
| `CRATONVM_JIMAGE_PACKAGE_INDEX` | **on** (`=0` kill) | *zs s6/rs (2026-10-15). Not a GC switch; every collector.* A `lib/modules` jimage on the class path is indexed by package (~900 entries, a module number each) instead of by class (~27 000 `String` pairs, ~5 MB written at every boot); a class is confirmed by the image's own perfect-hash lookup. An image with a package in two modules keeps the per-class map. `=0`: the per-class map for every image, as before. |
| `CRATONVM_JIMAGE_DIRECT_MAP` | **on** (`=0` kill) | *zr z8/fm (2026-10-13). Not a GC switch.* `jdk.internal.jimage.NativeImageBuffer.getNativeMap` answers a direct `ByteBuffer` (`DirectByteBuffer(long, long)`, the JNI constructor) over a read-only mapping of `lib/modules`, made once per path for the life of the process, as HotSpot answers its libjimage mapping. The `=0` arm copies the whole ~145 MiB image into a Java `byte[]` held for the life of the VM: a live huge array on the Java heap, and a `jrt:/` mount under `-Xmx` below ~160 MB threw `OutOfMemoryError` where HotSpot runs. A mapping that fails falls back to the copy. Page: `docs/known-issues/footprint/boot-footprint-owners-20261013.md`. |
| `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` | off (opt-in `=1`, or `force`) | *zr z7/rs (2026-10-13); opt-in since zr z10/mt.* Five interleaved host rounds put its CPU at 0.95 s against 0.79 s eager on `Zr7RsBoot classes 3000` (+20 %) and 0.53 against 0.51 on boot. `MIMALLOC_ALLOW_THP=0` on the eager arm cost the same, so both the CPU (about 35k extra 4 KiB first-touch faults) and the ~90 MB RSS saving are the huge pages'. That is over the 1.05x bar for a default; the RSS side is now `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP`'s job. With the switch set, the `cratonvm` launcher on Linux/macOS (the only binary that installs mimalloc) defaults mimalloc's `arena_eager_commit` to `0` from a pre-`main` constructor. With mimalloc's own default (`2`, eager on an overcommit OS) the first 1 GiB arena is committed up front and so `madvise(MADV_HUGEPAGE)`d by mimalloc, and on a THP=`madvise` host every touched 2 MiB of the allocator became a huge page: 141 MB of a 209 MB Hello world. An explicit `MIMALLOC_ARENA_EAGER_COMMIT` still wins, under any spelling mimalloc accepts (it matches case-insensitively and takes the legacy `MIMALLOC_EAGER_REGION_COMMIT`; since zr z9/rv8 the hook no longer forces one it did not spell itself, `=force` restores that); environment only (it is read before `main`). The `CRATONVM_GC_STATS` summary prints `[cratonvm] mimalloc: lazy_arena_commit=<default|env|late|off|not-run> arena_eager_commit=<n>`. Page: `docs/known-issues/footprint/boot-rss-by-owner-20261013.md`. |
| `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP` | off (opt-in `=1` / `zeroed`, or `=all` for plain allocs too; threshold `CRATONVM_MIMALLOC_SPARSE_BLOCK_MIN_KB`, default 1024, floor 64) | *zr z10/mt (2026-10-13).* Linux launcher only: the global allocator is mimalloc behind a thin wrapper. With the switch, every `alloc_zeroed` of at least the threshold (alignment up to a page) is served from its own anonymous `mmap`, advised `MADV_NOHUGEPAGE`, instead of mimalloc's arena, which the eager default commits and `madvise(MADV_HUGEPAGE)`s (a big zeroed block written sparsely then faults in every 2 MiB it touches, and a reused arena block is memset in full). The mapping is zero-filled lazily in 4 KiB pages, `realloc` is `mremap` (no copy), a shrink below the threshold moves back into mimalloc, and `dealloc` is `munmap`; a block's owner is decided by mimalloc's own page map (`mi_is_in_heap_region`), never by the mode. Opt-in because it reaches every collector's and every crate's big zeroed blocks; the ZGC bitmaps have their own default-ON lever, `CRATONVM_ZGC_SIDE_TABLE_NO_THP`. Environment only (latched at the first allocator call); `libcratonvm` / `cratonvm-embed` use their host's allocator. `CRATONVM_GC_STATS` prints `[cratonvm] mimalloc-sparse-blocks: mode=<off|zeroed|all|not-installed> min_kb= big_allocs= big_zeroed_allocs= big_zeroed_kb= big_live_kb= big_live_peak_kb= mapped= mapped_live_kb= mapped_peak_kb= remaps= map_failed=`; the `big_*` counters run in every mode, so the default arm already says how much of the arena big blocks held. Report: `docs/internal/zgc-round-zr-20261012/z10-mt-report.md`. zr z11/rv9: the mapping modes read as `off` on a mimalloc v2 backend (`mi_version() < 300`): v2's `mi_is_in_heap_region` stops at 48 TiB, so the ownership-by-address rule could unmap a mimalloc segment placed above it. |
| `CRATONVM_MIMALLOC_FIRST_TOUCH` | off (opt-in: `lazy`, `huge`, `populate`, combined with `,` or `+`; `=1` is `huge+populate`; arena size `CRATONVM_MIMALLOC_FIRST_TOUCH_ARENA_MB`, default 1024, floor 64) | *zs s1/fp (2026-10-14).* Linux launcher only. A pre-`main` constructor (`.init_array.00102`) maps mimalloc's first arena itself (2 MiB aligned, read-write, `MAP_NORESERVE`, `MADV_NOHUGEPAGE`) and registers it uncommitted with a commit callback (`mi_manage_memory`), so mimalloc reports every slice range it is about to use. `huge` advises the whole 2 MiB chunks of a commit `MADV_HUGEPAGE` (only a big singleton page or a medium-page run is that large: dense, one fault per 2 MiB); `populate` pre-faults the rest with one `MADV_POPULATE_WRITE` per commit (Linux 5.14+, latched off on `EINVAL`); `lazy` does neither (the control: it must equal `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT=1`). A purge is `MADV_DONTNEED`, reported decommitted so the reuse comes back through the policy. The aim is lazy commit's RSS (-80 MB on the zr rows) at eager's fault count; the host's interleaved rows decide the default. Environment only; `libcratonvm` / `cratonvm-embed` use their host's allocator. `CRATONVM_GC_STATS` prints `[cratonvm] mimalloc-first-touch: mode= outcome=<off|registered|map-failed|refused|not-run> arena_mb= commits= commit_kb= huge_kb= populate_kb= populate_failed= populate_unsupported= purges= purge_kb=`. Report: `docs/internal/zgc-round-zs-20261014/s1-fp-report.md`. |
| `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP=boot` (budget `CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT`, default 64; `0` maps nothing) | off (a value of the opt-in switch above) | *zs s9/rs2 (2026-10-16); reviewed zt t2/fp.* Mode `zeroed` with a budget: only the first N big zeroed blocks of the process get their own `MADV_NOHUGEPAGE` mapping (the boot's long-lived tables: shadow stacks, the class-id side tables, jimage-sized maps); later ones, the per-cycle scratch that made mode `1` cost +5 % CPU on a GC-heavy row (zr xmt), stay in mimalloc. The budget is latched at the first allocator call, a freed block does not return it, and (zt t2/fp) a mapping the kernel refuses gives it back. `[cratonvm] mimalloc-sparse-blocks:` adds `boot_budget_left=` / `boot_declined=`. Not host-measured yet: rows in `docs/internal/zgc-round-zt-20261018/t2-fp-report.md`. |
| `CRATONVM_TIER_C2_STANDBY` (`_DEPTH`, default 4, floor 1; `_SCALE`, opt-in) | off (opt-in `1`/`on`) | *zs s9/rs2 (2026-10-16); `_SCALE` and the epoch fix zt t2/fp.* Every C2 compile worker but the first is spawned parked before its first allocation (a parked thread owns no mimalloc pages; the zs s6a `TIER_C2_THREADS=1` row put three C2 threads' pages at 7-13 MB of boot RSS) and is promoted for good when an enqueue leaves `_DEPTH` requests on the C2 lane; under `_SCALE` the k-th promotion waits for `_DEPTH x` the live workers (with `_DEPTH=2`, the shape of HotSpot's `UseDynamicNumberOfCompilerThreads`). Shutdown and a restart release parked workers. Collector-independent. `[jit] c2-standby: depth= scale= started= promoted=` under `CRATONVM_DBG_RSS_BY_MAPPING`. Not host-measured yet: rows in the zt t2/fp report. |
| `CRATONVM_VTABLE_LAZY_DISPATCH` | off (opt-in `1`/`on`) | *zt t2/fp (2026-10-19).* A declared virtual bytecode method's dispatch snapshot (the interpreter's `CachedBytecodeMethod` and padded bytecode, ~1.9 KB a java.base class by the zs s3/fp3 census) is built on the first vtable dispatch through one of its slots instead of at link, one per method (an inherited slot shares its declaring method's), with the same contents; HotSpot builds a method's counters only once it runs. Every collector (class metadata, not heap). Read once per VM. `[vm] vtable-lazy-dispatch: on= deferred= built=` under `CRATONVM_DBG_RSS_BY_MAPPING`. Rows (RSS, `--nojit` virtual-call CPU, SAME) in the zt t2/fp report. |
| `CRATONVM_PADDED_CODE_MEMO_KEEP_LIVE` | **on** (`=0` kill) | *zt t3/fp (2026-10-19).* The interpreter's process-wide padded-bytecode memo (`vm/src/runtime/frame.rs`, `padded_bytecode_for_method`) no longer clears its whole table at 16 384 entries: it drops only the entries whose padded body nothing else holds and raises its limit to twice what it kept. The vtable link pads ~6 methods a class, so a class-heavy start passed the cap during the link and a live method padded again got a second `Arc`, hence a second quickened stream and liveness table. Not Java-visible; every collector. `[vm] padded-code-memo: entries= limit= evictions= evicted= clears=` under `CRATONVM_DBG_RSS_BY_MAPPING`. `=0`: clear whole (`clears=` counts it). |
| `CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS` | **on** (`=0` kill) | *zt t3/fp (2026-10-19).* A `lib/modules` runtime image on the class path leaves its `/packages/<package>/<module>` and `/modules` directory records out of the resource index (`resource_to_modules`) and the module list: libjimage's package table and `jrt:/`'s directories are not resources, and `packages` / `modules` are not modules. Before, `find_resource("java.lang/java.base")` read the `/packages` record, so any VM resource door that falls back to the class path could answer a name HotSpot answers `null`. Saves ~2 300 index entries (~0.45 MB) a boot; the `jrt:` file system reads the image directly and is unaffected. Every collector. Probe `Zt2FpJimageDirRecords`. `=0`: indexed as before. |
| `CRATONVM_ZGC_REFILL_CAPACITY_GATE` | **OFF** (`=1` opt-in) | *zr z5/cr.* The compiled TLAB refill gate on ZGC asks the arena (can it carve a chunk?) instead of `!needs_gc()`; collections stay the trigger's and the failure doors' decision. CPU lever for the per-object path near full. Measured by zr z5b: CPU equal to the default and `gate_carved=0` on the rows run, so it stays opt-in. |
| `CRATONVM_ZGC_REFILL_GATE_COLLECTS` | **OFF** (`=1` opt-in) | *zt t4/ch (2026-10-19).* On ZGC the compiled refill gate is `!needs_gc()`, so it refuses exactly when a collection is due; today the collection waits for the refill consult (4 MiB of successful refills, which a refused gate never makes, or 65,536 slow-path entries) or the wedge breaker (16,384 refusals), and every object of every compiled thread takes the locked per-object path meanwhile. With the switch the refused refill takes the due collection at once (stamped occupancy door `tlab-alloc-shaped`, at most one per 1/32 of the heap allocated since the last collection) and re-probes. The window is counted on every run: `gate_refused=` on `[GC] zgc-refusal:`; engagement `gate_collects=` / `gate_collects_dropped=` / `gate_collects_joined=` (zt t6/rv3: a request that ran no pause of its own, counted apart). Rows: `t4-ch-report.md` CH-3. |
| `CRATONVM_ZGC_JIT_FAILURE_DOOR_STAMPED` | **OFF** (`=1` opt-in) | *zt t4/ca2 (2026-10-19).* The JIT array helpers' allocation-FAILURE door (`jit_newarray`, `jit_anewarray` after a failed `try_alloc_array_full`) was unstamped, so several large-array threads that failed on one full arena each ran a collection in turn. With the switch the door stamps the request right after the failure and drops it when another thread's collection sealed since (HotSpot's `skip_operation`); the helper then retries as after a lost STW race. A current request is still an allocation refusal (futile rule, owed clear-soft rung, `[GC] zgc-refusal:` windows unchanged). Engagement: `failure_door_dropped=` on `[GC] zgc_stale_gc_requests:`. ZGC only; needs `CRATONVM_ZGC_DROP_STALE_GC_REQUESTS` (default ON). |
| `CRATONVM_JIT_ZGC_INLINE_ARM_SKIPS_REPUBLISH` | **on** (`=0` kill) | *zr z7/ca (2026-10-13).* In the optimizing tier's inline ZGC allocation (`new`, `newarray`, `anewarray`), the inline start-bit arm, which makes no call, jumps straight to the site's join instead of running the post-call frame republish (two TLS stores) that follows the `zgc_note_tlab_object` helper call. Value-identical; the helper arm is unchanged. ZGC only. |
| `CRATONVM_JIT_ZGC_ANNOUNCE_OWNED_WINDOW` | **on** (`=0` kill) | *zr z11/ia (2026-10-13).* The LONG inline ZGC start-bit store (both tiers' inline `new` / `newarray` / `anewarray`, box prefixes) proves the bitmap word it ORs into lies wholly inside the thread's chunk by comparing the word index against the buffer's owned-word window (`Tlab::zgc_owned_words`, two u32 written by `Tlab::new`), instead of re-deriving the word's 512-byte granule from the table base and comparing it against `start` and `end`. Same inequalities, so value-identical; 11 instructions become 4 (7 fewer, one load fewer) per compiled allocation. The SHORT form (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE`) keeps its own test. ZGC only. |
| `CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_GRID_TEST` | **on** (`=0` kill) | *zr z11/ia (2026-10-13).* The LONG inline ZGC start-bit store drops its `test rcx, 7; jnz` (object offset on the 8-byte grid). Every caller's object is an 8-aligned bump, and the collector publishes the announce table only for an 8-aligned bitmap base, so the branch was never taken. ZGC only. |
| `CRATONVM_JIT_ZGC_ANNOUNCE_COMMIT_FROM_RAX` | **on** (`=0` kill) | *zr z11/ia (2026-10-13).* In both tiers' inline ZGC allocation (start bit before the commit), the start-bit store keeps the announce table in R9, so RAX still holds the bump's end, and the inline arm commits with one store instead of re-deriving the end (5 instructions for an array, `lea` for a `new`). Both announce forms; the long form only with `CRATONVM_JIT_ZGC_ANNOUNCE_OWNED_WINDOW` on. Value-identical; the helper arm is unchanged. ZGC only. |
| `CRATONVM_JIT_ZGC_ANNOUNCE_SKIP_SPAN_TEST` | **on** (`=0` kill) | *zs s1/ca (2026-10-14).* With the owned-word window, the LONG inline ZGC start-bit store (both tiers) drops its `base <= obj` and `obj - base < span` tests (3 instructions, 1 load), which the window implies for every armed buffer. `words != 0` stays. ZGC only. |
| `CRATONVM_JIT_ZGC_INLINE_ARM_FAST_JOIN` | **on** (`=0` kill) | *zs s1/ca (2026-10-14).* In the optimizing tier's inline ZGC `new` / `newarray` / `anewarray`, the inline start-bit arm (no call made) lands past the frame-identity store and the zero test that only the stub owes, at sites whose shadow reload is empty. Value-identical; the safepoint-id store is unchanged. ZGC only. |
| `CRATONVM_JIT_IR_NEWARRAY_CONST_LENGTH` | **on** (`=0` kill) | *zs s1/ca (2026-10-14).* The optimizing tier's inline ZGC `newarray` / `anewarray` with a constant length bakes its size: no length load, screen or size arithmetic (7 instructions). Value-identical. ZGC only; Generational and G1 keep their bytes. |
| `CRATONVM_JIT_ZGC_INLINE_ARM_FALL_THROUGH` | off (opt-in `=1`) | *zs s9/ca4 (2026-10-16).* The optimizing tier's inline ZGC `new` / `newarray` / `anewarray` keeps only its fast half at the site (bump, header, inline start-bit store), whose inline arm falls through to the continuation instead of ending in `JMP allocated`. The announce helper arm, the allocation stub, the identity republish, the null test and the exception exit move after the method body. One taken branch fewer per compiled allocation, and about 100 bytes less hot-loop code per site. Value-identical. ZGC only; sites with a pending shadow publication, a protected bci or an armed call-throw / catch plan keep the unsplit layout. |
| `CRATONVM_JIT_ZGC_ALLOC_PREFETCH` | `0` (off; opt-in `1`..`4`) | *zs s10/ca5 (2026-10-16).* After the optimizing tier's inline ZGC bump passes its end test, emit that many `PREFETCHW [new cursor + distance + 64*i]`, so the lines the next allocations will write are owned before their header stores (HotSpot's `AllocatePrefetch*` shape). Value-identical: a prefetch has no architectural effect and never faults. ZGC only. |
| `CRATONVM_JIT_ZGC_ALLOC_PREFETCH_DISTANCE` | `256` | *zs s10/ca5.* The first prefetched line's distance past the new cursor, in bytes (0..4096). |
| `CRATONVM_JIT_ZGC_NEWARRAY_SKIP_WORD_ROUND` | off (opt-in `=1`) | *zs s10/ca5.* The optimizing tier's inline ZGC `newarray` with a run-time length and 8-byte elements (`long[]`, `double[]`) drops `ADD RDX, 7; AND RDX, -8` from its size arithmetic: `length << 3` is already a multiple of 8. Two instructions fewer per varlen allocation. Value-identical. ZGC only. |
| `CRATONVM_JIT_ZGC_NEWARRAY_SIZE_OFF_CHAIN` | off (opt-in `=1`) | *zt t2/ca (2026-10-19).* The optimizing tier's inline ZGC `newarray` / `anewarray` with a run-time length computes its whole footprint from the length alone (`LEA RDX, [RCX * 2^log2 + 23]; AND RDX, -8`, no `AND` for 8-byte elements) and bumps with the two-component `LEA RAX, [R11 + RDX]`. The old bump `LEA RAX, [R11 + RDX + 16]` has three components, the slow `LEA` on Intel Sandy Bridge to Skylake, and it sits on the TLAB cursor's loop-carried chain. Two to three instructions fewer per varlen allocation (subsumes `CRATONVM_JIT_ZGC_NEWARRAY_SKIP_WORD_ROUND`). Value-identical. ZGC only; a constant length keeps its baked footprint. |
| `CRATONVM_JIT_ZGC_ALLOC_PREFETCH_INSTANCE_LINES` | unset (= the `CRATONVM_JIT_ZGC_ALLOC_PREFETCH` count) | *zt t2/ca (2026-10-19).* The prefetch line count for an inline `new` only (`0`..`4`), as HotSpot keeps `AllocateInstancePrefetchLines` beside `AllocatePrefetchLines`: an array site keeps `CRATONVM_JIT_ZGC_ALLOC_PREFETCH`'s count. ZGC only. |
| `CRATONVM_JIT_ZGC_TLAB_ALIGN_TEST` | **on** (`=0` kill) | *zs s6/ca3 (2026-10-15).* The optimizing tier's inline ZGC bump (`new`, `newarray` / `anewarray`, box prefixes) tests the loaded TLAB cursor for 8-alignment (`TEST R11B, 7; JNE slow`) instead of aligning it (`ADD R11, 7; AND R11, -8`). The cursor is the one value a compiled allocation loop carries through memory from iteration to iteration; the test takes two dependent ALU operations off that chain (HotSpot's C2 bump has none). An unaligned cursor (none is known) takes the slow path before anything is written. ZGC only; Generational and G1 keep their bytes. |
| `CRATONVM_JIT_IR_ZGC_ARRAY_HEADER_QWORDS` | off (opt-in `=1`) | *zs s2/ca2 (2026-10-14); made opt-in after the s2b host A/B: `alloc_fixed0` 2.81 ns vs 2.56 ns with the dword stores, process CPU +5.8 %.* The optimizing tier's inline ZGC `newarray` / `anewarray` writes its 16 header bytes with two qword stores (`MOV RDX, imm64; MOV [R11], RDX` for the header word, `MOV QWORD [R11 + 8], RCX` or `imm32` for the length with the zero hash word) instead of four dword stores: two stores and one instruction fewer per compiled array allocation. Value-identical. ZGC only; the `new` arm, Generational and G1 keep their bytes. |
| `CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE` | **OFF** (`1` opt-in) | *zs s2/ca2 (2026-10-14).* The LONG inline ZGC start-bit store (both tiers) drops its `blocked` test and the zero test of `words` (5 instructions, 2 branches, 1 load per compiled allocation). The collector keeps the invariant that makes the ownership-epoch compare imply both: raising the table's `blocked` (a mark start; a generational switch under `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE=0`) and withdrawing the table bump the epoch, and no buffer is armed while `blocked` is set. Cost: after a mark start each thread's current buffer uses the announce helper until its next refill; a buffer carved during a concurrent mark uses it for its whole life. Both halves read the key (the collector latches it per heap). |
| `CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE` | **on** (`=0` kill) | *zs s3/bt (2026-10-14).* The optimizing tier's gated reference `putfield` / `aastore` on ZGC's age-floor plan opens with ONE 16-bit compare of the adjacent `pre_active` / `post_active` gate bytes (`CMP word [rip+disp32], 0; JE store`, or through R11 when out of RIP reach) and stores at once when both read zero; otherwise the full gate sequence runs unchanged behind it. Both bytes zero is sufficient for every arm of that sequence that reaches the store, so the outcome is identical; the fast path drops from 9 gate instructions (`aastore`: pre gate, null test, flags byte, floor) to 2-3. ZGC only (the generational mask plan and G1 keep their bytes; the single-pass tier is unchanged). Needs `CRATONVM_ZGC_JIT_POST_GATE_TRUTH` to engage. |
| `CRATONVM_JIT_ZGC_FUSED_GATE_ALL_SITES` | **on** (`=0` kill) | *zs s5/bt2 (2026-10-15).* The fused gate above also opens the remaining compiled reference stores on ZGC: the single-pass `putfield` (a compact receiver stores at once on a zero word; legacy receivers and a set word run the unchanged sequence) and `aastore`, and the constant-`VarHandle` / `Unsafe.compareAndSetReference` gate screen of both tiers (`MOV R11, imm64; CMP word [R11], 0; JE` past the screen). Same soundness argument; needs `CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE`. G1 and the generational mask plan are byte-identical. |
| `CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE` | off (opt-in `=1`) | *zs s5/bt2 (2026-10-15).* The optimizing tier's gated reference `putfield` into a FRESH receiver (an `Op::New` of the same block; only pure arithmetic and stores into that same object scheduled in between; the object not stored anywhere; the field not yet written) stores with no gate at all: its old value is null, so the SATB pre barrier logs nothing, and it is age 0, so the floor would pass. Engages only where the fused gate does (ZGC's age-floor plan). `CRATONVM_DBG_JITC=1` prints one `zgc fresh-receiver ref store` line per elided site. |
| `CRATONVM_ZGC_JIT_POST_GATE_TRUTH` | **on** (`=0` kill) | *zs s3/bt (2026-10-14).* A new ZGC heap publishes its JIT post gate as the truth it mirrors (`has_old_objects` = false, so 0) instead of armed; the promoting sweep raises it before the truth, as before. On the default non-generational collector the gate therefore reads 0 for the heap's lifetime. `=0` starts it armed (the fused gate then never answers "store" and every compiled store pays it on top of the old sequence). |
| `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM_RATE` | **on** inside the opt-in arm (`=0` removes the limit) | *zs s1/ca (2026-10-14).* The opt-in `CRATONVM_ZGC_JIT_NEWARRAY_HEADROOM=1` arm asks for at most one occupancy collection per 1/32nd of the heap allocated since the last collection. Counted as `newarray_headroom_rate_skips=` on the `[GC] zgc_stale_gc_requests:` line. |
| `CRATONVM_ZGC_LAZY_TLAB_STARTS` | **OFF** (`census` or `1` opt-in) | *zr z8/st.* The census and materialize half of a ZGC start registry with no per-allocation bit inside a live VM-TLAB chunk (proposal C of `zgc-zr7ca-proposal-ir-inline-alloc-fast-path-trims-20261013.md`). The start bit is still set at every allocation and the JIT is unchanged. On, the heap keeps a table of live VM-TLAB chunks, walks each chunk's consumed span by its headers when the buffer retires and when a pause publishes a frozen peer's tail, and counts the oracle hits (`is_object_address`, `is_heap_addr`, the pin probe) that land inside a live chunk, own thread or peer, re-checking one in 8 by a read-only walk. `census` registers nothing; any other on value also registers a walked base the registry lacks. Figures on `[GC] zgc-lazy-starts:` (`lz_unregistered_found`, `lz_q_own`, `lz_q_peer`, `lz_q_mismatch`). Remaining steps to drop the bit: `docs/internal/gc/zgc-zr8st-lazy-tlab-starts-remaining-steps-20261013.md`. |
| `CRATONVM_ZGC_LAZY_STARTS_DROP_UNPUBLISHED` | **ON** (`=0` kill) | *zr z9/rv8.* Only with `CRATONVM_ZGC_LAZY_TLAB_STARTS` on. A collection forgets the lazy-starts record of every live VM-TLAB buffer that the pause neither retired nor published: the sweep may free the dead objects below that buffer's cursor, and since zr z8/zb (`CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS`) their headers survive, so the retire walk would parse the corpses as objects and `materialize` would register them again. Counted as `lz_chunks_unpublished_dropped` on `[GC] zgc-lazy-starts:`. |
| `CRATONVM_ZGC_JIT_SHORT_ANNOUNCE` | **OFF** (`1` opt-in) | *zr z9/bo (2026-10-13).* Every inline ZGC start-bit store (the single-pass and optimizing tiers' inline `new` / `newarray` / `anewarray`, the box prefixes, the inline `StringBuilder.toString()` pair) emits the SHORT form: 25 instructions, 5 branches and 7 loads against 38 / 9 / 10. It sets the same bit of the same bitmap word under the same ownership test, so every start-registry reader answers as before. The collector publishes two more table words whatever the switch says: `short_blocked` (zero only while the table is published, not blocked, and the arena base is 512-byte aligned) and `granule_bias` (`words - (base >> 9) * 8`). On a misaligned arena base every short site takes the announce helper (correct, slower). Decided against a card / block-offset start index: `docs/internal/gc/zgc-zr9bo-proposal-card-start-index-refuted-by-the-query-census-20261013.md`. |
| `CRATONVM_ZGC_SOFT_CLEAR_CONSERVATIVE` | **on** (`=0` kill) | *zr z2/cp.* On the last-ditch clear-all-soft collection, a condemned soft referent that only a conservatively scanned compiled-frame word names is cleared, not restored (`SoftRefBeforeOome` printed `cleared false` with the JIT on). Trade: a live compiled local holding the referent also loses its soft reference, before an `OutOfMemoryError` only. |
| `-XX:SoftMaxHeapSize=<size>` | `-Xmx` | *zr z1/tr (2026-10-12).* ZGC only, as on HotSpot: the size the warm-up rule (10/20/30% of it) and the `CRATONVM_ZGC_RATE_TRIGGER` rules aim under; printed as `Soft Max Capacity:` in `-Xlog:gc+init`. Clamped to `-Xmx`. A goal, not a limit. The legacy 75% live-bytes threshold still reads `-Xmx`. |
| `CRATONVM_ZGC_MARK_REF_CHUNK` | **4096** | Numeric, not boolean. How many of one object's strong out-edges a mark worker traces before returning to its loop head, where it re-tests the safepoint yield flag — so it is what bounds time-to-yield on a huge reference array instead of leaving it proportional to the array's length. Read once per mark pool (at construction) and once per serial-mark call (`mark::ref_chunk_budget`, over `mark::Z_MARK_REF_CHUNK`, in `gc/src/zgc/mark.rs`); it was latched once per process until zt t1/mk (2026-10-19). **Live on every ZGC configuration**, both through `ZgcRealHeap`'s own `ZMarkContext::visit_refs_chunked` and through the `ZHeapMarkBridge` wrapper that forwards it (both in `gc/src/zgc.rs`) — the bridge inherited the unsplit default until 2026-09-21, which made the in-crate test population exercise the unchunked path while the counter read zero. `0` and unparseable values fall back to the default; there is deliberately no "disable" value. Engagement counter: `ref_chunks=` on `[GC] zgc-markend:` (a concurrent cycle) and `[GC] zgc-mark-parallel:` (`CRATONVM_ZGC_PARMARK`), and `chunks=` on `[GC] zgc-mark-serial:` for the default serial marker, which chunks with the same budget since zr z1/mk (this row named a `[GC] zgc-mark:` line, which never existed, until 2026-10-12) — **zero means "no object on this workload needed splitting", not "broken"**. |
| `CRATONVM_ZGC_MARK_SPLIT_ARRAYS` | **on** (`=0` kill) | *zs s5/mc3 (2026-10-15); row added zt t1/mk.* A reference array longer than one split unit is split on its first visit into STEALABLE chunk tasks (tagged work-queue words, halved like HotSpot's partial-array entries), so one huge `Object[]` is no longer one mark worker's serial slot loop. Every element is walked exactly once. Inert on a one-worker pool. Read once per mark pool. Engagement: `array_splits=` / `array_tasks=<pushed>/<run>` on `[GC] zgc-mark-parallel:` and, since zt t1/mk, on `[GC] zgc-markend:`. `=0` restores the private in-progress slot (one worker per array). |
| `CRATONVM_ZGC_MARK_SPLIT_CHUNK` | **unset** (= the yield chunk) | *zt t1/mk (2026-10-19), opt-in, numeric.* The unit of a split array's chunk tasks, in elements, apart from `CRATONVM_ZGC_MARK_REF_CHUNK` (the yield chunk). Smaller (512-1024; HotSpot splits at `ZMarkPartialArrayMinSize`, 512 wide refs) spreads a mid-size array over more workers; larger cuts the task count on a 10^7-element array (each task is still walked in yield-chunk steps). Unset or `0` is byte for byte the behaviour before the key. Read once per mark pool; inert without `CRATONVM_ZGC_MARK_SPLIT_ARRAYS`. Deciding rows G1-G3 of `docs/internal/proposals/gc/zgc-zs5mc3-proposal-array-split-granularity-20261015.md`. |
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
  budget off. `-XX:MaxGCPauseMillis=<n>` no longer reaches it (zr z3/cs,
  `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS`, default on): HotSpot's ZGC ignores
  the flag, so it must not change how often this ZGC collects. With
  `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS=0` the flag sets the target and wins
  over the variable, as before. The feedback loop is blind to the first cycle, so that one still
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
  It is NOT the whole of `-Xms`: HotSpot's `-Xms` also sets `MinHeapSize`,
  which is `VmConfig::min_heap_size` here, and only vm-cli sets both. ZGC
  uncommits down to `min_heap_size` (`CRATONVM_ZGC_UNCOMMIT_TO_MIN_HEAP`), and
  when it is `None` that floor is HotSpot's ergonomic `min(8M, initial)`. An
  embedder (`VmConfig::default`, `libcratonvm`, `cratonvm-embed`) that wants
  the old "never uncommit below the startup commit" floor sets
  `min_heap_size = Some(initial_heap_size)` as well (zt t2/rv, from
  `zgc-zr4rv4-footprint-residuals-CLOSED-20261019.md` item 2).
- **What `Runtime.totalMemory()` reports.** The heap's COMMITTED size
  (`VmHeap::committed_bytes`), on every backend since 2026-09-23 — so a fresh
  `-Xms64m -Xmx512m` process reports about 64m, as on HotSpot, and
  `freeMemory()` is that minus occupancy. Before that date Generational and
  ZGC reported their reservation (`-Xmx`), and a cache sized from
  `totalMemory()` or `freeMemory()` was sized against a heap that did not
  exist. **ZGC** preserves the startup committed-byte floor. It returns
  eligible unused granules at a collection safepoint, after forwarding records
  are retired, with `-XX:+ZUncommit` (default) and
  `-XX:ZUncommitDelay=<seconds>` (default 300). `-XX:-ZUncommit` disables
  every arena give-back path; `-XX:ZUncommitDelay=0` enables immediate
  give-back while retaining `-Xms`. The readable give-back policy retains
  committed mappings while returning resident pages, and obeys the same delay
  and enable switch. See
  `docs/internal/gc/zgc-decommit-ignores-xms-FIXED-20261001.md`.
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
| `CRATONVM_TLAB_SHARE_SIZER=1` (`CRATONVM_GC=tlab-share-sizer`) | Default ON on the Generational heap since gcg g6/jn (`=0` kills it there); `=1` arms it on G1 and ZGC. HotSpot-shaped TLAB sizing: each thread asks for a fiftieth of its exponentially averaged bytes allocated per young pause (`TLABWasteTargetPercent` 1 %, `TLABAllocationWeight` 35 %), clamped to 8 KiB – 1 MiB, and a compiled allocation that misses with a tail bigger than `desired / 64` keeps the buffer and allocates outside it (the refill-waste limit). Replaces the ladder sizer when on (the ladder's two retired-buffer switches and the Generational pristine-chunk skip were removed in gce e2/o); read back on `[GC] tlab-sizer:`. Its protocol-A win on Generational (W1 610 → 63 minors) made it the Generational default for a moment in gen r5w4, **reverted** before merging: with the tail sink it hands out live memory (`docs/internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`). That page is fixed, and gcg g6/jn flipped the Generational default back on vg5 (3 interleaved pairs: park 82-88 -> 37 minors, correctness rows 5/5). |

All but the share sizer (Generational) are off until their A/B runs are taken; the protocols are in
`docs/internal/reviews/gengc-round4-w2-alloc2-20260923.md` (the first two),
`docs/internal/reviews/gengc-round4-w3-alloc3-20260923.md` (the next three)
and protocol A of `docs/internal/reviews/gengc-round5-w2-alloc6-20260926.md`
(which measured the share sizer and the tail sink on the Generational heap;
the tail sink is in the next table now, the share sizer is the Generational default since gcg g6/jn). Every default lives in one place,
`cratonvm_types::flags::alloc_policy_defaults`: flipping an arm is that
constant (plus its row's `off_word` in `types/src/flag_groups.rs`, for the
generated flag docs).

#### Allocation kill switches (default ON)

One row below (`CRATONVM_SOFTREF_HOTSPOT_LRU`) is ON by default on the **Generational heap** (`-XX:+UseGenerationalGC`, gen r5w4, 2026-09-26)
and, since zr z1/rf, on **ZGC** (`SOFTREF_HOTSPOT_LRU_ZGC`): their default is per backend
(`alloc_policy_defaults::BackendDefault`) because its evidence was taken on
Generational alone (the share sizer used the same mechanism and was reverted). On G1 it stays off unless set to `1`; on every
backend an explicit `=0` / `=1` wins. Each constant in `types/src/flags.rs`
carries its evidence.

| Variable | Effect of `=0` |
|---|---|
| `CRATONVM_GEN_TLAB_TAIL_SINK=0` (`CRATONVM_GC=-gen-tlab-tail-sink`) | Generational (default ON since gen r5w2): buries a retiring TLAB's unused tail under an `int[]` filler again. ON, the tail goes back to the young from-space (the bump cursor retracts, or the tail joins the young free list) instead of being counted as occupied by the young trigger. Read back as the `young_tlab_tail_*` counters of `HeapStatsSnapshot`. |
| `CRATONVM_SOFTREF_HOTSPOT_LRU=0` (`CRATONVM_GC=-softref-hotspot-lru`) | **Generational and ZGC default ON** (gen r5w4; ZGC since zr z1/rf, `SOFTREF_HOTSPOT_LRU_ZGC`; G1 default OFF): the SoftReference LRU policy goes back to the wall clock now and the free space measured before the collection. ON, it takes HotSpot's `LRUMaxHeapPolicy` inputs — the soft clock and the free heap as of the end of the previous collection — so a soft reference read since the last collection is never cleared by the policy. |
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
| `CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX` (`old-compact-forward-index`) | **ON** (`=0` off; WARNING flip gcj j3 close 2026-10-20: pinned compaction 0.80x at -Xmx1g, 0.89x at 64m, CPU 0.95-0.98x, every row SAME on j3a) | *(gcj j1/og)* The old-gen compactors (the full-GC slide and the pinned compaction) resolve each reference slot through a sorted, block-indexed survivor table instead of a hash map with one insert per survivor and one random probe per edge. Same heap after the compaction (unit-tested); `[gcphase] old-gen compact-forward-index: count=` under `CRATONVM_DBG_GCPHASE`. |
| `CRATONVM_GC_OLD_FREE_RATIO_GROWTH` (`old-free-ratio-growth`) | off | *(gcj j1/og)* After a stop-the-world old-gen collection, grow the old generation so `MinHeapFreeRatio` (default 40 %) of it is free, up to HotSpot Serial's old maximum (`2/3 · -Xmx`), as HotSpot's `TenuredGeneration::compute_new_size` does; the young trigger cedes the budget. Needs `CRATONVM_GC_OLD_BORROW_YOUNG` (default ON). Without it the generation grows only after a refused allocation. |
| `CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT` (`old-mark-loader-snapshot`) | off (opt-in `=1`; gcj j2: back to opt-in, no measured gain on j1c -- `orch-notes.md`) | *(gcj j1/og)* The old-gen mark answers each marked object's instance -> defining-loader edge from one `loader_pin` snapshot per collection instead of taking the registry's read lock and probing it once per marked object. Same answers (the rows naming this heap cannot change inside the pause). |

#### gcj j2/og2 switches (old generation, majors, full GC, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN` (`conc-bitmap-growth-span`) | **ON** (`=0` off) | *(gcj j2/og2)* A Generational concurrent cycle's mark bitmap is built over the old generation's growth limit, not its capacity at the driver's read. A growth between that read and the initial-mark pause (another thread's major) used to make the remark refuse the whole traced cycle; `[GC] conc_cycles: conccyc_bitmap_cover_refusals=` counts the refusals. The bitmap is zero-allocated, so the extra span is address space until marked. |
| `CRATONVM_GC_OLD_SHRINK_FACTOR` (`old-shrink-factor`) | off | *(gcj j2/og2)* The committed-size shrink after a stop-the-world old-gen collection releases 0, 10, 40, then 100 % of the excess over the `MaxHeapFreeRatio` target on consecutive shrinking collections (HotSpot's `ShrinkHeapInSteps` `_shrink_factor`), from the top of the committed span; a collection without excess resets it. Off: the whole excess is released unless the committed end advanced since the previous resize. Same `oldsz_shrinks` / `oldsz_shrunk_bytes` counters. |
| `CRATONVM_GC_OLD_MARK_PREFETCH` (`old-mark-prefetch`) | off | *(gcj j2/og2)* The stop-the-world old-gen mark (`old_gen_gc_inner`) moves entries off its LIFO worklist into an 8-entry ring, prefetching each header as it enters, and scans the oldest ring entry. Only the scan order changes; the same objects are marked. `[gcphase] old-gen mark-prefetched: count=` under `CRATONVM_DBG_GCPHASE`. Proposal `gcj-j1og-proposal-old-mark-stack-prefetch`. |
| `CRATONVM_GC_OLD_PAR_ADJUST` (`old-par-adjust`) | off | *(gcj j2/og2)* The full-GC sliding compaction's Phase 2 (every survivor's reference slots rewritten through the forwarding table) runs on scoped worker threads, one contiguous run of survivors each, joined before the slide. `1` / `on`: `min(cpus, 4)` workers; `n >= 2`: `n` (at most 16). Serial below 65 536 survivors. Same heap after the compaction (unit-tested over 2, 3 and 7 workers). `[gcphase] old-gen compact-par-adjust: count=1` under `CRATONVM_DBG_GCPHASE`. Proposal `gcj-j1og-proposal-parallel-full-gc-adjust-and-mark`, step 1. |

<!-- gcj j3/og3 -->
#### gcj j3/og3 switches (old generation, majors, full GC, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GC_OLD_PINNED_COMPACT_REUSE_CLOSE` (`old-pinned-compact-reuse-close`) | off | *(gcj j3/og3)* The pinned old-gen compaction (every requested major without a conservative root, `CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`) takes its Phase 0b answer from the mark's last in-mark closure (the true-root seed's), exactly as the in-place sweep already reuses it, instead of closing the live set again. Phase 0b still runs when the compactor itself marked a pin. `[gcphase] old-gen compact-phase0-skipped: count=1` under `CRATONVM_DBG_GCPHASE`. Proposal `gcj-j1og-proposal-full-gc-drops-the-redundant-phase-0-closure`, the cheap half. |
| `CRATONVM_GC_OLD_COMPACT_TRUST_MARK` (`old-compact-trust-mark`) | off | *(gcj j3/og3)* The full-GC slide skips Phase 0 when the old mark recorded no unfollowed reference (a rejected in-range candidate, an in-extent address outside the readable span, a scan that bailed on a bad header) and nothing was marked without a push (no finalizer round, no in-mark closure). The proposal's full design; decide it with `..._VERIFY=1` first. |
| `CRATONVM_GC_OLD_COMPACT_TRUST_MARK_VERIFY` (`old-compact-trust-mark-verify`) | off | *(gcj j3/og3, debug)* A compaction handed a "closed, no escape" verdict runs Phase 0 anyway, uses its answer, and counts each disagreement (`[gcphase] old-gen compact-trust-mark-disagreement: count=`, an error log line). Must never fire. |
| `CRATONVM_GC_OLD_TRIGGER_SERIAL` (`old-trigger-serial`) | off | *(gcj j3/og3)* An unrequested major runs when the old generation's free space no longer holds the padded average promotion of a young cycle (HotSpot Serial's `promotion_attempt_is_safe`, `AdaptiveSizePolicyWeight=10`, `PromotedPadding=3`), or after a refused old-gen allocation, instead of at 75 % occupancy on every young cycle. The 75 % floor stays as a ceiling for a generation that cannot grow and whose free list is fragmented for the recent requests. Proposal `gcj-j1og-proposal-serial-shaped-major-trigger`. |
| `CRATONVM_GC_OLD_CAPACITY_SHRINK` (`old-capacity-shrink`) | off | *(gcj j3/og3)* After a stop-the-world collection, an old-gen capacity that grew into the reservation's headroom (`CRATONVM_GC_OLD_BORROW_YOUNG`, `CRATONVM_GC_OLD_FREE_RATIO_GROWTH`) comes back down to where the committed resize left the generation (never below the construction capacity or `MinHeapFreeRatio`; not within one collection of a growth), and the young trigger gets back the budget the growth ceded. Blocker B2 of `gcj-j2og2-free-ratio-growth-flip-blockers`. |
| `CRATONVM_GEN_TENURED_POOL_MAX_FIXED` (`tenured-pool-max-fixed`) | off | *(gcj j3/og3)* `Tenured Gen`'s `MemoryPoolMXBean` max is the old generation's growth limit (HotSpot Serial's fixed tenured maximum, `2/3 · -Xmx` with headroom) instead of its current capacity, which moves with every growth; the young pools' max is the matching remainder. Blocker B3 of the same page. |

#### gcj j1/yg switches (young collection, TLAB logging, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_XLOG_GEN_TLAB_TOTALS_ALONE` (`xlog-gen-tlab-totals-alone`) | **ON** (`=0` off) | Generational: a `-Xlog:gc+tlab=debug` rule alone (no `gc` rule at info) arms the collection's events, so HotSpot's `GC(n) TLAB totals:` line prints at every pause. `=0`: the line prints only when another collection tag is also logged, as before. G1 and ZGC are unchanged. |
| `CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK` (`gen-pinned-copy-takes-promotion-risk`) | off | A JIT-warm young cycle (unrewritable conservative JIT roots) that the promotion-OOM guard alone (old and young both at least 90 % full) sent to the in-place sweep is offered to the pinned in-place copy instead, which keeps a survivor nothing can take rather than aborting. Engagement on the shutdown line `[GC] young_term4_offer:` (`t4o_promotion_risk_admitted=`). |

#### gcj j2/yg2 switches (young collection, TLABs, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY` (`gen-young-known-zero-carry`) | **ON** (`=0` off) | Generational: the non-moving young sweep keeps claiming as known-zero the bytes of the previous claim that are still on the free list (merged with the spans it zeroes itself), and the TLAB tail sink returns a tail without dropping the claim. A TLAB refill or slow allocation served wholly inside the claim skips its memset. Engagement on the shutdown line `[GC] young_premium:` (`yp_kz_carried_bytes=`, `yp_kz_carry_sweeps=`); effect on `[GC] gen-alloc:` (`gen_zeroed_reuse_bytes=` falls, `gen_zero_skipped_bytes=` rises). `=0`: the claim is the last sweep's spans only, and a tail return drops it. |
| `CRATONVM_TLAB_HOTSPOT_WASTE_LIMIT` (`tlab-hotspot-waste-limit`) | off | Generational, share sizer on: HotSpot's refill-waste limit -- re-armed to `desired / TLABRefillWasteFraction` at every refill, raised by `-XX:TLABWasteIncrement` words (default 4) per kept miss -- instead of this VM's `desired / TLABRefillWasteFraction` raise. G1 and ZGC unchanged. Ignored under `CRATONVM_TLAB_HOTSPOT_FLAGS=0`. |
| `CRATONVM_GEN_SERIAL_EDEN_NO_PAUSE_GOAL` (`gen-serial-eden-no-pause-goal`) | off | A Generational heap the Serial-shaped young trigger (`CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN`) armed runs no young pause goal (HotSpot Serial has none), unless `CRATONVM_GC_YOUNG_PAUSE_MS` is set explicitly. `[GC] young_trigger:` then reads `young_pause_goal_ms=0` and `adapt_halvings=0`. |

#### gcj j3/yg3 switches (young collection, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE` (`gen-young-mirror-root-when-promotable`) | off (opt-in `=1`; back to opt-in 2026-10-20: it keeps dead loaders from unloading on the conc-unload rows) | Generational: a root scan that publishes its precise-table counts (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, default ON) roots a YOUNG user-loader class mirror from the class-mirror table instead of deferring it under `CRATONVM_GEN_YOUNG_MIRROR_DEFER`, so the scan no longer forces the in-place young sweep (term 3) to keep the deferral's promise: a JIT-warm young cycle can reach the pinned copy, and the mirror is promoted by the copy or by precise-root promotion in a sweep. `[GC] conc_y2o:` `young_mirrors_deferred=` stays 0 on such scans. `=0`: the young mirror is deferred and the cycle swept in place, as before. A dead young loader whose young mirror is its only keeper unloads after the mirror's promotion (HotSpot's young collection also roots every loader's mirrors). |
| `CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS` (`gen-young-mirror-defer-moving-follows`) | off | Generational, with `CRATONVM_GEN_YOUNG_MIRROR_DEFER` on (the default): a root scan that deferred young user-loader class mirrors to their loaders no longer forces the in-place young sweep (term 3) to keep that promise; the moving young collection (the copy and the pinned in-place copy) follows the loaders' `mirror_pin` rows itself -- an old loader's always, a young loader's once the collection keeps it -- as the opt-in moving `System.gc()` does. A dead young loader's young mirror is still reached by nothing and is reclaimed. JIT-warm young cycles can then reach the pinned copy (`[GC] decision histogram:` `moving-pinned-pages=`); `young_mirrors_deferred=` on `[GC] conc_y2o:` still counts the deferrals. |
| `CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR` (`gen-typed-min-tlab-floor`) | **ON** (`=0` off) | Generational: the smallest-whole TLAB refill's floor and the TLAB tail sink's smallest listed tail are the VM's `-XX:MinTLABSize` (as the refill request already is), not the 8 KiB constant. Nothing typed: 8 KiB, unchanged. `=0`: the constant. |
| `CRATONVM_GEN_TLAB_RAISE_BY_GRANTED_BYTES` (`gen-tlab-raise-by-granted-bytes`) | off | Generational, share sizer on: a refill window doubles a thread's desired TLAB size when its GRANTED bytes reach `2 x target_refills x desired` (the same threshold in bytes), not after `2 x target_refills` refills, so a window of short grants (whole blocks below the request, fragmentation mini-TLABs) no longer raises toward 1 MiB. `[GC] tlab-sizer:` `sizer_raises=` falls on such runs. G1 and ZGC unchanged. |
| `CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES` (`gen-cheney-overflow-promotes`) | **ON** (`=0` off) | Generational: the serial (Cheney) moving young copy tenures a survivor that young to-space cannot take, as the parallel copy already does and as HotSpot's young collection does on a survivor overflow, instead of aborting the process. Unreachable while to-space can hold all of from-space (every configuration today); `cheney_overflow_promotions=` on `[GC] par_evac:` counts it. `=0`: the abort. |

#### Pinning and conservative roots (gcj j1/pn, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED` (`gen-pinned-seeds-never-dropped`) | **ON** (`=0` off) | A pinned in-place young cycle whose plan names its seeds (the default watched / finalizable filters, or `_EXACT_ROOTS`) never reclaims a seed whose forward was refused before the pinned arm (a suspect header); it stays in place. `=0` reclaims it, as before. |
| `CRATONVM_GEN_YOUNG_PIN_SHORT_SWEEP_INCOMPLETE` (`gen-young-pin-short-sweep-incomplete`) | **ON** (`=0` off) | A young-pin deposit that licenses moves but cannot sweep to its stack top (top unknown: a foreign stack pointer; or no band at all) marks the pause's ledger incomplete, so the cycle declines to the sweep (`nonmoving-pin-ledger-incomplete`) instead of moving under unswept words. |
| `CRATONVM_GEN_PINNED_COPY_NAMED_ROOTS` (`gen-pinned-copy-named-roots`) | off | The pinned in-place young cycle roots only the objects a pin word names (the containing object, an array ending at a start word, arrays within the 256-byte slack), not every object on the pinned page; the rest stay in place only if reached. Narrower than `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`, and wins over it. Engagement: `[GC] pinned_exact_roots:`. |
| `CRATONVM_GEN_CONSERVATIVE_LOCALS_SKIP_PRIMITIVE_KINDS` (`gen-conservative-locals-skip-primitive-kinds`) | off | Generational only: the conservative interpreter-locals probe stops treating a `long` / `double` local whose value equals an object address as a reference (a minted JNI handle still roots). Less retention, fewer promotion pins; HotSpot never retains through a primitive. gcj j2/pn2: the operand-stack probe follows the same rule (`long` / `double` kind slots). |

#### Pinning and conservative roots (gcj j2/pn2, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_PINNED_REFUSED_FORWARD_KEPT` (`gen-pinned-refused-forward-kept`) | **ON** (`=0` off) | A pinned in-place young cycle whose plan names its seeds keeps a pinned object whose forward was refused (a suspect header, a parallel worker's refusal): something reached it, so it is not reclaimed. `=0` reclaims it as in gcj j1. Count: `prk_kept_sum=` on `[GC] pinned_exact_roots:` (0 on a healthy run). |
| `CRATONVM_GEN_PINNED_COPY_OBJECT_WINDOWS` (`gen-pinned-copy-object-windows`) | off | The pinned in-place young cycle keeps in place only the objects overlapping a pin word's window (64 bytes below, 256 above), not every object on its 4 KiB page; the page's other objects are copied when reached and freed otherwise, and its free bytes outside the windows take copies. The 1/8 page bound and `pycopy_pages_*` still count pages. |
| `CRATONVM_GEN_BAND_DEAD_SPILL_BOUNDED` (`gen-band-dead-spill-bounded`) | off | Generational only: the moving-young coverage verifier's "dead operand spill above the live cursor" excuse covers only operand-spill words, not the typed ranges, the locals tail or offsets past the spill area. More words are checked for shadow publication, so more JIT-warm cycles can be refused a moving copy. |
| `CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED` (`gen-claimant-skips-unmounted`) | **ON** (`=0` off) | Generational only: a parked (unmounted) virtual thread no longer claims its last carrier's OS thread in the blocked-peer stack-capture fold, so a blocked carrier adopts its own captured words and its wake rewrites them after a moving young cycle. `=0` restores the gen r5w1 rule (the newest publish wins). |
| `CRATONVM_GEN_INTERP_LOCALS_TYPE_MAP` (`gen-interp-locals-type-map`) | off | Generational only: an interpreter frame's reference-typed locals at its pc (the bytecode type map HotSpot's interpreter uses) decide the conservative locals probe (a local typed `int` / `float` / conflict is not probed; a minted JNI handle still roots) and the remap of a local that lost its tag (not rewritten unless typed a reference). The precise tag scan is unchanged. |

#### JNI switches (gcj j1/jn, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_JNI_FOREIGN_ALLOC_LADDER` (`jni-foreign-alloc-ladder`) | on (Generational; `=0` kills; gcj j2 flip) | Generational only. A JNI allocation (`NewObject`, `New<Type>Array`, `NewStringUTF`, ...) refused on a foreign-attached thread (`AttachCurrentThread`) runs the native reclaim ladder and retries once before the `OutOfMemoryError`, as HotSpot does and as every other thread's JNI allocation already does (`CRATONVM_GC_JNI_ALLOC_LADDER`); only while the thread is a counted mutator inside its JNIEnv function (the foreign transitions, default ON) and holds only indirect locals. Off, such an allocation never collects: an attached thread that allocates and drops arrays in a loop, with no Java thread allocating, gets an `OutOfMemoryError` once the old generation fills. Deciding rows: `Gcj1JnForeignAllocProbe` in `docs/internal/gc-round-gcj-20261019/j1-jn-report.md`. |
| `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT` (`dedupe-initiator-snapshot`) | on (Generational; `=0` kills; gcj j2 flip), off elsewhere | The collection door's registry gather leaves out the initiator thread's own root snapshot, which the door already scanned (gci i21/pr; `docs/GC.md`). |
| `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` (`stw-undiscovered-refs-strong`) | on (Generational; `=0` kills; gcj j2 flip), off elsewhere | A stop-the-world pause keeps the referent of a `Reference` only a dead finalizable reaches, as HotSpot Serial does (gci i16/fr; `docs/GC.md`). |
| `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK` (`gen-tlab-refill-smallest-whole-block`) | on (`=0` kills; gcj j2 flip) | A young TLAB refill takes the smallest whole free block that makes a useful TLAB before the default carve (gci i22/dr). hashmap-pair 10x faster on j1; alloc-ring costs 1.14x (lane yg2). |
| `CRATONVM_JNI_ARRAY_VM_LIMIT` (`jni-array-vm-limit`) | **ON** (`=0` kill) | Generational only. A JNI `NewObjectArray` / `New<Type>Array` longer than HotSpot's array limit (`Integer.MAX_VALUE - 2`, every element type) leaves `OutOfMemoryError: Requested array size exceeds VM limit` pending at once, as HotSpot's `check_array_allocation_length` does, instead of running the native reclaim ladder (a forced young collection and two majors) and reporting `Java heap space`. The interpreter, the JIT helpers and the VM's own native array doors already refused such a length first. Rows: `Gcj1JnArrayLimitProbe`. |

The per-VM latch of `CRATONVM_JNI_IN_NATIVE_BAND_NARROW` and `CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`
(gcj j1/jn) changes no default: both are still opt-in, now read once per VM instead of once per JNI call.

#### JNI transition cuts (gcj j2/jn2, 2026-10-19)

<!-- gcj j2/jn2 -->
| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_JNI_SWITCHES_ONCE_PER_CALL` (`jni-switches-once-per-call`) | off (opt-in) | Every backend; cost only, no behaviour change. A JNI native dispatch resolves the VM's JNI switches once and passes them to the receiver / argument local recording, the band check and the in-native entry; the entry and the leave read the VM they hold. Off, each of those sites resolves them again through the thread's JNI context (about seven TLS borrows and struct copies per call). Deciding rows: block C2 of `docs/internal/gc-round-gcj-20261019/j2-jn2-report.md` (interleaved CPU A/B). |
| `CRATONVM_JNI_IN_NATIVE_KEEP_TLAB` (`jni-in-native-keep-tlab`) | off (opt-in) | Generational only, with the JNI in-native package (default ON); not with `CRATONVM_JNI_IN_NATIVE_LOCKFREE_LEAVE`. A JNI call going into native keeps its TLAB instead of retiring it (and re-carving one on its next allocation); a collecting pause retires every kept TLAB on its in-native owner's behalf before it collects, as HotSpot retires every TLAB at the safepoint. Engagement: `tlab_kept`, `tlab_kept_reused`, `tlab_kept_retired_by_pause` on the `CRATONVM_DBG_JNI_PHASE=1` line. Deciding rows: block T of the same report. |
| `CRATONVM_JNI_VIRTUAL_UPCALL_SELECTS` (`jni-virtual-upcall-selects`) | on (Generational; `=0` kills; gcj j3 flip) | Generational only (the round's gate; the fix is collector-independent). A JNI `Call<Type>Method*` / `Call<Type>MethodA/V` up-call selects its target against the method ID, as HotSpot's `jni_invoke_nonstatic` does: a private method ID runs as resolved, and a private or cross-package package-private method of the receiver's class does not hide the method ID's override. Off, the first same-signature declaration on the receiver's class chain runs (a private subclass method hides the method ID). Rows: `Gcj2Jn2UpcallSelectProbe`, block U of the same report. |
| `CRATONVM_JNI_NEW_STRING_HOTSPOT_EDGES` (`jni-new-string-hotspot-edges`) | **ON** (`=0` kill) | Generational only. JNI `NewString(p, 0)` answers `""` whatever `p` (HotSpot reads no unit, so `NewString(NULL, 0)` is the empty String), and `NewString(p, len)` with `len < 0` leaves `NegativeArraySizeException: <len>` pending, as HotSpot's byte-array allocation does. `=0`: `NULL` with nothing pending for both, as before. Rows: `Gcj2Jn2NewStringEdgeProbe`, block S of the same report. |

The NB refusal counters (gcj j2/jn2; `dep_band_narrow_no_chain`, `..._truncated`, `..._jit_above`,
`..._spill_outside` on the same line) change no behaviour: they say why `CRATONVM_JNI_IN_NATIVE_BAND_NARROW`
keeps a band whole.

#### Generational observability and launcher (gcj j1/ob)

<!-- gcj j1/ob -- begin -->
| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS` (`gen-hotspot-value-flags`) | **ON** (`=0` kill) | *gcj j1/ob.* On Generational (`-XX:+UseGenerationalGC`, `-XX:+UseSerialGC`) every `-XX:<Name>=<value>` of the launcher's typed table (`vm-cli` `GEN_HOTSPOT_VALUE_FLAGS`: the GC thread counts, pause / occupancy / waste percentages, `SoftMaxHeapSize`, `MaxDirectMemorySize`, `MaxMetaspaceSize`, `MaxJavaStackTraceDepth`, `SoftRefLRUPolicyMSPerMB`, the tenuring flags, the heap free ratios, plus `SurvivorRatio` and the TLAB flags) is read with HotSpot 25's syntax for its type in command-line order; the first bad one ends the launch with `Improperly specified VM option '<Name>=<value>'` (or a verified range line) and the two `Error:` lines on stderr, exit 1 -- also before a `-version` banner. `ParallelGCThreads` / `ConcGCThreads` take `0x10` / `3k`. G1 and ZGC unchanged. `=0`: the lenient `Warning: ignoring ...` lines. |
| `CRATONVM_GEN_THRESHOLD_SET_DRAIN` (`gen-threshold-set-drain`) | **ON** (`=0` kill) | *gcj j1/ob.* On Generational a usage-threshold crossing that `setUsageThreshold` finds (`Tenured Gen`, or a non-heap pool such as `Metaspace`) is handed to the delivery thread at once, as zs s9/ob6 does on ZGC (`CRATONVM_ZGC_THRESHOLD_SET_DRAIN`, a separate key so each collector's kill row attributes on its own): the `MEMORY_THRESHOLD_EXCEEDED` notification arrives while the setting thread sleeps, as from HotSpot's Service Thread. Read once per heap. `=0`: the drain hint alone (the setting thread's next allocation door, or the next collection). |
| `CRATONVM_GEN_XLOG_SAFEPOINT_STATS` (`gen-xlog-safepoint-stats`) | **ON** (`=0` kill) | *gcj j1/ob.* On Generational, `-Xlog:safepoint+stats` (`safepoint*`, `all`) prints HotSpot 25's statistics table -- the `VM Operation [ threads: total initial_running ][ time: sync vmop total ] page_trap_count` header every 30 safepoints and one row per safepoint, before its `Safepoint` line -- and at exit the summary (`<operation> <count>` with the terminating `Halt`, `Maximum sync time`, `Maximum vm operation time (except for Exit VM operation)`), in the shape of the 25.0.3 oracle `xl47_r_stats_capture`. The rows also widen the `-Xlog:safepoint*` tag column to `safepoint,stats`, as HotSpot's do. G1 and ZGC unchanged. `=0`: nothing on `safepoint,stats`. |
| `CRATONVM_GEN_SERIAL_SELECTOR_SILENT` (`gen-serial-selector-silent`) | **ON** (`=0` kill) | *gcj j1/ob.* `-XX:+UseSerialGC` selects Generational (its HotSpot twin) without the `Warning: unsupported garbage collector -XX:+UseSerialGC ...` stderr line; HotSpot prints nothing for it. `-XX:+UseParallelGC` / `UseShenandoahGC` / `UseEpsilonGC` keep the warning. `=0`: the warning for Serial too. |
<!-- gcj j1/ob -- end -->

#### Generational observability and launcher, wave 2 (gcj j2/ob2)

<!-- gcj j2/ob2 -- begin -->
| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_JCMD_STRINGTABLE_LIVE` (`gen-jcmd-stringtable-live`) | **ON** (`=0` kill) | *gcj j2/ob2.* On Generational, `jcmd <pid> VM.stringtable [-verbose]` (and the DiagnosticCommand MBean's `vmStringtable`) reads the live interned-String table: `Number of entries` / `Number of literals` are the table's size, the literal bytes its compact-String payload, and `-verbose` lists the texts (sorted). It answered a constant 0-entry text whatever the table held. The byte columns are layout-dependent (rows mask them). G1 and ZGC unchanged. `=0`: the constant text. |
| `CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE` (`gen-one-ordered-value-parse`) | off (opt-in) | *gcj j2/ob2.* The Generational slice of the one ordered `-XX:` value parse (proposal `gcj-j1ob-proposal-one-ordered-vm-option-parse-for-every-collector`), on top of `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS`: the syntax of `NewSize`, `MaxNewSize`, `NewRatio`, `InitialHeapSize`, `MinHeapSize` and `MetaspaceSize` is checked (`Improperly specified VM option`); `MaxDirectMemorySize`, `MaxMetaspaceSize`, `MaxJavaStackTraceDepth`, `SoftRefLRUPolicyMSPerMB` and the young / heap sizes reach their consumers as HotSpot read them (`0x1000000`, `1t`, `1k` no longer print `Warning: ignoring ...`); the G1 / ZGC knobs that configure nothing on Generational (`InitiatingHeapOccupancyPercent`, `G1HeapWastePercent`, `MaxGCPauseMillis`, `G1HeapRegionSize`, `SoftMaxHeapSize`, `ZUncommitDelay`, `ZAllocationSpikeTolerance`) are taken silently within their source ranges; and a `-version` launch prints the `AfterMemoryInit` constraint lines (heap free ratios; `SurvivorRatio` when `-Xmx` is given; `TLABSize` / `MinTLABSize`) instead of the banner. G1 and ZGC unchanged. Off by default: the types are read from `globals.hpp`, not from rows. |
| `CRATONVM_GEN_PRINT_FLAGS_FINAL` (`gen-print-flags-final`) | off (opt-in) | *gcj j2/ob2.* On Generational, `-XX:+PrintFlagsFinal` prints `[Global flags]` and one line per flag this VM models (`InitialHeapSize`, `InitialTenuringThreshold`, `MaxHeapFreeRatio`, `MaxHeapSize`, `MaxJavaStackTraceDepth`, `MaxTenuringThreshold`, `MinHeapFreeRatio`, `MinHeapSize`, `ResizeTLAB`, `SoftRefLRUPolicyMSPerMB`, `SurvivorRatio`, `TargetSurvivorRatio`, `UseG1GC`, `UseParallelGC`, `UseSerialGC`, `UseTLAB`, `UseZGC`), sorted, with the effective value and its `{command line}` / `{ergonomic}` / `{default}` origin, on stdout before the VM starts; `-XX:+PrintFlagsFinal -version` boots the VM to print it before the banner. A flag this VM does not model is absent, not invented (proposal `gcj-j1ob-proposal-print-flags-final-from-the-typed-table`, scope (a)). Off by default: the column layout is read from `jvmFlag.cpp` and needs the capture row of `j2-ob2-report.md` before a flip. G1 and ZGC unchanged. |
| `CRATONVM_JFR_STARTUP_BANNER` (`jfr-startup-banner`) | off (opt-in) | *gcj j2/ob2.* A `-XX:StartFlightRecording` start-up recording prints HotSpot 25.0.3's three `[jfr,startup]` lines on stdout once the VM exists, for the option shapes with captured rows: none (`... name=1 filename=FILEPATH ...`), `filename=` with or without `name=` (`Started recording 1. No limit specified, using maxsize=250MB as default.`, an empty line, `Use jcmd <pid> JFR.dump name=<n or 1> to copy recording data to file.`), `filename=` with `maxsize=` or `maxage=` (`Started recording 1.`), `filename=` with `duration=` (`Started recording 1. The result will be written to:`, an empty line, the absolute file name). Decorated and padded as the `-Xlog` logger's stdout lines, or `[<uptime>s][info][jfr,startup]` without `-Xlog`. Other shapes print nothing, and so does a run whose `-Xlog` spec names `jfr` or `disable`. Every collector: the banner is JFR's. |
| `CRATONVM_GEN_HOTSPOT_VALUE_RANGES` (`gen-hotspot-value-ranges`) | **ON** (`=0` kill) | *gcj j2/ob2, from the HotSpot 25.0.3 captures of the j2a rows.* On Generational (with `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS`), the typed scan refuses with HotSpot's lines: the ranges of `SoftRefLRUPolicyMSPerMB` (`intx`, 0 ... 9223372036854775807), `MaxJavaStackTraceDepth` (`int`, 0 ... 1073741823), `MaxGCPauseMillis` (`uintx`, 1 ... 18446744073709551614), `InitiatingHeapOccupancyPercent` / `G1HeapWastePercent` (`uint`, 0 ... 100), `MaxDirectMemorySize` (`uint64_t`, 0 ... 9223372036854775807); `-XX:<BoolFlag>=<v>` (`Missing +/- setting for VM option '<BoolFlag>=<v>'`); `MaxTenuringThreshold` above `u32` (`Improperly specified VM option` alone). A negative `SoftRefLRUPolicyMSPerMB` / `MaxJavaStackTraceDepth` therefore no longer reaches clap. On the parsed path: `-XX:ParallelGCThreads=0` is taken silently, `-XX:MaxDirectMemorySize=0` is a zero cap, `-XX:MaxMetaspaceSize=0` ends the launch with `Error occurred during initialization of VM` / `OutOfMemoryError: Metaspace` on stdout. G1 and ZGC unchanged. `=0`: the j1 answers. |
| `CRATONVM_GEN_JFR_NAME_MAXSIZE_OPTIONS` (`gen-jfr-name-maxsize-options`) | **ON** (`=0` kill) | *gcj j2/ob2.* On Generational, `-XX:StartFlightRecording=name=<n>,...` and `maxsize=<size>` are accepted (not modelled: the recorder is bounded by `maxevents`); the launcher ended the run with `unrecognized option` where HotSpot starts the recording. G1 and ZGC unchanged. `=0`: the refusal. |
| `CRATONVM_GEN_JCMD_HOTSPOT_TEXTS` (`gen-jcmd-hotspot-texts`) | **ON** (`=0` kill) | *gcj j2/ob2.* On Generational, `jcmd VM.uptime` prints HotSpot's `<seconds> s` (without `-date`) and `VM.command_line` HotSpot's `VM Arguments:` / `jvm_args: ...` / `java_command: ...` / `java_class_path (initial): ...` / `Launcher Type: ...` block, as captured on 25.0.3. G1 and ZGC unchanged. `=0`: `VM uptime: <s> seconds` and the rebuilt `cratonvm ...` line. |
<!-- gcj j2/ob2 -- end -->

#### String table switches (gcj j1/st, 2026-10-19)

<!-- gcj j1/st -->
| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE` (`weak-string-table-insert-grace`) | off (opt-in) | Generational under the weak String table only: a String the VM's pooled constructors (`create_java_string*`, `try_create_java_string*`: `ldc` literals, native `intern_string`) pooled since the last pause stays rooted through the next pause, so a Rust holder of it across one non-moving pause keeps a live String (`gcg-g4rs`'s blanket mitigation). Java-visible over-retention of one pause (a `WeakReference` to such a String clears one collection late), hence opt-in. Engagement: `grace_rooted=` on the `CRATONVM_DBG_STRING_TABLE=1` line. G1 / ZGC: no-op. |

`CRATONVM_GC_STRING_TABLE_FUNNEL` (opt-in, gci i25/st2) is unchanged as a switch; since gcj j1/st its
young-entry list carries each listed String's current address (kept exact by every pause), so a young
pause no longer hashes every listed text (`gci-i47w-oa47`, the cost finding that kept it off).

#### JIT <-> GC barrier switches (gcj j2/jt2, 2026-10-19)

<!-- gcj j2/jt2 -- begin -->
| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_JIT_POST_ONLY_CARD_MARK` (`post-only-card-mark`) | off (opt-in) | Generational only. Both tiers' inline card barrier drops its PRE card check and keeps the check after the store (HotSpot Serial's store-then-mark order, still behind the young / null / old-value / range filters): the single-pass dirty-card store is one check instead of two (~18 instructions instead of ~36), the optimizing tier loses its PRE compare. The BUG-03 take-over finishes what a frozen peer owed: it records each frozen peer's general-purpose registers and, when it resumes them after the pause's card scan, dirties the card of every register word in the old generation (`CardTable::complete_frozen_register_marks`). Engagement: `gen_jit_card_post_only_sites=` (sites compiled) and `gen_jit_card_post_only_completed=` (register words completed) on the `[GC] gen-alloc:` line. Refused beside `CRATONVM_GC_PRECISE_ARRAY_HEADER_CARD`. G1 / ZGC: no card view, no byte changes. |
| `CRATONVM_JIT_CARD_BYTE_MAP_BASE` (`card-byte-map-base`) | off (opt-in) | Generational only. The inline card check forms the card address as HotSpot's `(addr >> 9) + byte_map_base` (one shift, one load) instead of subtracting the old base and adding the map, when the card table's base is card-aligned (`JitCardView::FLAG_BYTE_MAP_BASE`). Same card byte. |
| `CRATONVM_JIT_IR_FRESH_STORE_NO_BARRIER` (`ir-fresh-store-no-barrier`) | off (opt-in) | Generational only. The optimizing tier's gated reference `putfield` into an object the same block just allocated, with nothing but arithmetic and first stores into it in between, emits no SATB pre gate (the field's old value is null). The young-receiver test is kept (register-only), so an object the allocation returned old still takes its card. |
<!-- gcj j2/jt2 -- end -->

### G1 (`gc/src/g1.rs`)

`G1CollectorConfig` is exposed publicly. Every row below is the value in
`G1CollectorConfig::default()`; the `-XX:` column is the operator spelling
where one exists (`VmHeap::new_with_overrides`).

| Field | Default | `-XX:` | Effect |
|---|---|---|---|
| `heap_size` | 256 MiB | `-Xmx` | Total managed heap. Since F-16 this is the size of the **reservation** — address space — not of the memory charged to the process at startup. |
| `initial_heap_size` | `0` = ergonomic | `-Xms` | Bytes committed up front. The ergonomic is a **sixteenth of the heap, floored at four regions**, so a short-lived process pays almost nothing for a large `-Xmx`. The prefix then grows on demand as regions are claimed; `CRATONVM_G1_RESERVE_HEAP=0` restores the pre-F-16 "commit everything at startup". |
| `region_size` | 1 MiB, **but see the ergonomic** | `-XX:G1HeapRegionSize` | Granularity for evacuation and remembered-set tracking. `VmHeap` overrides the default with `g1_ergonomic_region_size` = `max(heap / 2048, 1 MiB)` clamped to `[256 KiB, 32 MiB]`, so the region COUNT stays near 2048 at any heap size. Raising it reduces remembered-set overhead at the cost of more wasted bytes per part-full region — and raises the humongous threshold, which is `region_size / 2`. |
| `max_gc_pause_ms` | 200 | `-XX:MaxGCPauseMillis` | The pause goal (on ZGC the flag is ignored as on HotSpot, unless `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS=0`). It reaches **two** decisions: how many OLD regions a mixed collection set may take (`old_cset_copy_budget_ns` = the goal minus the rolling fix-up cost), and the adaptive young-region target (`update_young_target`). It does **not** move the marking threshold any more — see `ihop_percent`. |
| `ihop_percent` | **70** | `-XX:InitiatingHeapOccupancyPercent` (1–100) | Old-generation occupancy at which a concurrent mark cycle starts. A **ceiling** on the adaptive threshold, never a floor: `CRATONVM_G1_ADAPTIVE_IHOP=0` restores the old pause-time-driven model. Raised from HotSpot's 45 because the default heap is small; on a 16 GiB heap you want it back near 45. |
| `promotion_age` | 15 | — | The tenuring **ceiling**. Since F-18 the effective threshold is re-derived after every pause from an age histogram of surviving bytes and may be lower — never higher — when survivor space would overflow its target (an eighth of the young generation). `CRATONVM_G1_ADAPTIVE_TENURING=0` pins it at the configured value. |
| `gc_worker_threads` | `0` = ergonomic | `-XX:ParallelGCThreads` | **No longer a no-op.** Both `-XX:ParallelGCThreads=<n>` and `--XX:ParallelGCThreads <n>` reach G1 and ZGC; the last occurrence wins. G1 reports its applied count as `Parallel Workers:` under `-Xlog:gc+init`. G1 refuses zero and malformed HotSpot values before boot. The parallel evacuator is on by default; `0` derives the count from the machine (`ergonomic_gc_worker_threads`, HotSpot's taper), a non-zero value is an explicit request honoured even above the CPU count, and `CRATONVM_G1_WORKERS=<n>` overrides both. `=1` drains the parallel path serially, which is the lever that separates a concurrency race from a logic divergence. The generational young collector has its own worker policy (`CRATONVM_GC=par-threads=<n>`) and does not read this field at all. |
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
> `docs/internal/gc/common-w34-dev-f144bb3d2-probe-regressions-heap-oome-and-gc-notifications.md`.

<!-- gcj j1/rf -->
### Reference keep-alive and the watched set (gcj j1/rf, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_REFERENT_READ_KEEP_ALIVE` | **ON** (`=0` kill) | Generational only. A read of `Reference.referent` that hands the referent to Java without `Reference.get()` -- `Unsafe.getReference[Volatile]`, JNI `GetObjectField`, reflective `Field.get`, a field `MethodHandle` / `VarHandle` -- logs it through the SATB barrier while a concurrent old-generation mark is open, as `Reference.get()` does (HotSpot's `ON_UNKNOWN_OOP_REF` / `jni_GetObjectField` referent keep-alive). Only slot 0 of a `java.lang.ref.Reference` subclass qualifies, so no other read adds floating garbage. Without it, a referent parked in an already-scanned object during a cycle could have its weak reference cleared at remark while strongly reachable. G1 and ZGC are unchanged. `=0`: the raw read. |
| `CRATONVM_GC_WATCHED_SET_PER_HEAP` | **ON** (`=0` kill) | The watched-referent set the VM publishes before each collection is keyed by heap (a Generational heap's old-generation base), so an embedder interleaving two VMs on one thread cannot have the second VM's publication replace or be read for the first VM's collection. One VM per thread: no change. `=0`: the one-slot per-thread set. |
| `CRATONVM_GEN_DEFER_ROOT_FREED_OLD_FILTER` | off (opt-in `=1`) | Generational only. A rooting scan of a loader-licensed native-root row (`class-atomic-slots`, `indy-call-sites`, `type-variables`, `defined-packages`, `lambda-singletons`, ...) whose owner is a user-loader class does not root an OLD value whose block is free: a licensed major pinned the value to a loader that a true-root major then left out (a dead young `ClassLoader` kept in place) and freed it, and a rooting scan before the next young collection would hand the freed block to the marker. The rule `scan_inherited_tl_pending` and the ephemeron side rows already apply. Cost: one old-generation lock per rooted old value of such a row. |
| `CRATONVM_GEN_SOFT_CLEAR_ALL_ADVANCES_CLOCK` | **ON** (`=0` kill) | Generational only. The last-ditch clear-all-soft collection before an `OutOfMemoryError` advances the soft-reference clock as an ordinary collection does (HotSpot's `update_soft_ref_master_clock` runs after every reference-processing round). Before, the next ordinary collection measured soft-reference idle times against the clock of the last LRU round and cleared fewer of them than HotSpot. `=0`: the clear-all round leaves the clock where it was. |
| `CRATONVM_GEN_SIDE_ROW_EPHEMERON` | **ON** (`=0` kill) | *gci i23/ep; flipped 2026-10-17 (WARNING gci-i48-flipB); row added by gcj j1/rf, the flip had none here.* Generational only. The per-object JUL rows (a `Logger`'s handlers / parent / filter, a `Handler`'s `ErrorManager`) and the `SSLContext`-keyed `TrustManager` / `KeyManager` rows are pinned to their key on a licensed stop-the-world scan instead of rooted, so a webapp handler or trust manager that reaches its own key no longer keeps its class loader alive (the ephemeron semantics of the field each row stands for). `=0`: the values are roots again. |

<!-- gcj j2/rf2 -->
### Finalizer seeds and the String table hit grace (gcj j2/rf2, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS` (`gen-major-defers-finalizer-seeds`) | **ON** (`=0` kill) | Generational only. A major marks the objects it keeps only for a finalizable -- the true-root seed's finalizer candidates and (on the moving path) the resurrected finalizables the young phase promoted -- in its finalizer-resurrection round instead of seeding them as roots. The OLD children of a dead YOUNG finalizable are then finalizer-reachable, not strongly reachable, and their soft and weak references are cleared at the collection that finds the finalizable dead, before `finalize()` runs, as HotSpot clears them (ec47). The objects kept are the same; only the depth-2 set grows. An old finalizable reachable only from a dead young one is finalized at the same collection (HotSpot enqueues both). Engagement: `[watchref] major: deferred finalizer seeds marked N` under `CRATONVM_DBG_WATCHREF=1`. `=0`: the seeds are roots again. |
| `CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE` (`weak-string-table-hit-grace`) | off (opt-in) | Generational under the weak String table, and only with `CRATONVM_GC_WEAK_STRING_TABLE_INSERT_GRACE`: a pool HIT (a pooled constructor, `NativeContext::create_string`, the exception-message cache) is rooted through the next pause too, as the insert grace roots a String the call pooled -- the residual of `gcg-g4rs` (a Rust local holding a hit's String across a non-moving pause). One lock and push per hit while on; Java-visible over-retention of one pause. Engagement: `grace_hits=` on the `CRATONVM_DBG_STRING_TABLE=1` line. G1 / ZGC: no-op. |

<!-- gcj j3/rf3 -->
### Legacy-seed finalizer deferral, watched-set reuse, String table rows (gcj j3/rf3, 2026-10-19)

| Variable | Default | Effect |
|---|---|---|
| `CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED` (`gen-legacy-seed-defers-resurrected`) | **ON** (`=0` kill) | Generational only; engaged only while `CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS` is. The legacy young seed of a major (`CRATONVM_GC_FULL_GC_TRUE_ROOTS=0`, a declined true-root ask, a walk-gap or compaction fallback) marks the old referents of every young object except the young phase's resurrection set (the dead young finalizables and what only they reach), whose old referents are marked in the major's finalizer-resurrection round instead: they are finalizer-reachable, so their weak references clear before `finalize()`, as on the true-root seed (ec47). The objects kept are the same. Engagement: `[watchref] major: legacy young seed deferred N old referent(s)` under `CRATONVM_DBG_WATCHREF=1`. `=0`: every young object seeds as before. |
| `CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED` (`gen-watched-set-reuse-unchanged`) | off (opt-in) | Generational only. When the reference processor's watched list is element for element the one the heap's watched-set row was last built from, and no snapshot of the row is held, the row is reused (the addresses the String table and the JNI weak globals added since are removed again) instead of being cleared and re-hashed. The published set is the list either way. Engagement: `[watchref] watched list reused` under `CRATONVM_DBG_WATCHREF=1`. G1 / ZGC: unchanged. |
| `CRATONVM_GC_STRING_TABLE_SEED_ONLY_YOUNG` (`string-table-seed-only-young`) | off (opt-in) | Generational under the weak String table: the young pooled Strings are published into a per-heap seed-only row that only a major's young seed and its watched-dead verdict read, not into the watched set every young sweep reads, so the young sweeps emit no identity `pointer_map` entry for them. A true-root major still judges every young pooled String it did not reach. G1 / ZGC: unchanged. |
| `CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE` (`gen-weak-string-table-young-in-cycle`) | off (opt-in) | Generational under the weak String table: while a concurrent old cycle is open, a young pause's root scan leaves the pool out too (the cycle's own initial-mark and remark scans still root it), so a dead young interned String is dropped at the young pause that finds it dead instead of surviving the whole cycle; old entries are kept by range as on any young pause. Engagement: the `[string-table]` line's `skipped=` stops growing during the cycle. |
| `CRATONVM_DBG_UNHANDLED_EXCEPTION_REPORT` (`unhandled-exception-report`) | off (opt-in) | Windows diagnostic: an unhandled-exception filter prints `[cratonvm] fatal: unhandled exception 0x<code>` on stderr for any exception that ends the process (the crash handler reports only access violations, illegal instructions, stack overflow and fast-fail). For a death that leaves nothing on stderr, e.g. `0x80000001` (a guard-page violation), which Git Bash reports as `rc=1`. |

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
> | `jdk.GCPhasePause` | generational collections, while a recording runs | the collector's phase marks laid out consecutively from the pause start, plus an `other` row for the residual (gen r4w2/obs). G1 and ZGC: none yet; concurrent cycles emit nothing (`docs/internal/gaps/gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted-FIXED-20261012.md`) |
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

<!-- gcj j1/cm -- begin -->
### Safepoint and handshake switches (gcj j1/cm, 2026-10-19)

| Switch | Default | What it does |
|---|---|---|
| `CRATONVM_GC_HANDSHAKE_LOOP_KICK=0` | ON (`=0` kills); reached only under the opt-in `CRATONVM_GC_HANDSHAKE_FRAME_TRACE=1` | After a cross-thread `getStackTrace()` queues its frame-trace handshake, every interpreter dispatch loop's poll word is bumped, so a running interpreted target that allocates nothing serves the handshake at its next bytecode (`interpreter::loop_top_poll`) instead of at its next allocation slow path; the other loops take one cold path and settle. `=0`: the gci i23/hs reach (a non-allocating interpreted loop misses the 1 ms budget and the requester takes the `door=frame-trace` pause). No effect with the handshake switch off, on any collector. |
| `CRATONVM_GC_HANDSHAKE_SPIN_WAIT=0` | ON (`=0` kills); reached only under `CRATONVM_GC_HANDSHAKE_FRAME_TRACE=1` | The requester of a frame-trace handshake waits each 100 us slice by polling its ticket with `yield_now` instead of a timed condvar wait. The OS rounds a timed wait up to its timer tick (15.6 ms on a default Windows host), so a target that never served -- a JIT-compiled peer that takes no allocation slow path -- cost about 13 ms per sample before the fallback `door=frame-trace` pause instead of the 1 ms budget (vi23-hs L3, p50 13363 us on vs 477 us off). `=0`: the timed wait. |
<!-- gcj j1/cm -- end -->

<!-- gcj j2/cm2 -- begin -->
### Safepoint, handshake and SATB switches (gcj j2/cm2, 2026-10-19)

| Switch | Default | What it does |
|---|---|---|
| `CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK=0` | ON on the Generational backend (`=0` kills); G1 and ZGC unchanged | A registered native-builtins thread (the XNIO I/O thread, the Vert.x event loop) retires itself when its loop ends, on its own thread and through the blocked-region protocol a dying platform thread uses: it enters the blocked population (so every later stop-the-world census excludes it), then goes dead under the barrier lock after waiting out any pause in progress, and releases its monitors; its death is no longer queued for the next XNIO / Vert.x native call to drain. Before, a census taken between the loop's end and the OS thread's finish counted a thread that runs no Java and reaches no safepoint, and the pause waited for it forever (the take-over keeps only peers in compiled code, and a finished OS thread leaves the roster). Also: a registered native thread whose OS thread could not be spawned is unregistered (`NativeContext::abandon_native_thread`) instead of being counted by every later census. `=0`: the thread stays counted until its OS thread finishes, and its death is queued. |
| `CRATONVM_GC_HANDSHAKE_TARGET_KICK=1` | OFF (opt-in); reached only under `CRATONVM_GC_HANDSHAKE_FRAME_TRACE=1` with `CRATONVM_GC_HANDSHAKE_LOOP_KICK` on, and only on the Generational backend | Each interpreter dispatch-loop entry publishes the poll word it runs on (one compare per entry while it is unchanged), and a frame-trace handshake kicks only its target's published word instead of every loop of the VM (`[HANDSHAKE_TRACE] kicked=2`; `kicked=1` is the kick-all, the fallback when the target published nothing). A target that moved to another carrier since its last loop entry serves at its next poll or the requester takes its pause at the 1 ms budget, as without a kick. Proposal `gcj-j1cm-proposal-target-only-handshake-kick`. |
| `CRATONVM_GEN_SATB_DIRTY_THREAD_WALK=1` | OFF (opt-in); Generational only (G1's queue is never switched) | The concurrent old-gen cycle's remark and cleanup walks of the per-thread SATB buffers visit only the threads that logged an SATB entry during the cycle -- a per-queue list each thread joins at its first bucket opening of the cycle, emptied at the next activation -- instead of locking every registered thread's buffer inside the pause. Exited threads' buffers are walked as before. Takes effect from the second activation after VM start. Engagement and gain: `threads_visited` vs `threads_held` on the `[GC] g1 satb-walk:` line of `CRATONVM_GC_STATS=1` (the line moves on Generational runs too). Proposal `gcj-j1cm-proposal-dirty-thread-satb-walk`. |
<!-- gcj j2/cm2 -- end -->

<!-- gcj j3/rt3 -- begin -->
### Runtime identity, pause request and SATB gate switches (gcj j3/rt3, 2026-10-19)

| Switch | Default | What it does |
|---|---|---|
| `CRATONVM_JIT_MH_SITE_IDENTITY=0` | ON (`=0` kills); every collector (a JIT memo bug, not a collector's) | A compiled `MethodHandle.invokeExact` / `invoke` site's per-thread rows -- "already linked" (the first-call link that resolves the site's descriptor classes through the caller's loader) and the constant bind's seen bit and give-up budget -- are keyed by the site's `JitInvokeInfo` address, which the allocator re-issues after a code retirement. Each row now proves it still names its site: the JIT cache's retire count it was made at, re-proved after a retire by the caller class, the `invoke` / `invokeExact` choice and the descriptor. A new site at a re-issued address links again instead of inheriting "linked". `=0`: the address-only rows. |
| `CRATONVM_OS_TID_IDENTITY_SKIPS_UNMOUNTED=0` | ON (`=0` kills); every collector (thread identity) | "Which registered thread runs on this OS thread?" (an unbound JNI `MonitorEnter`, the stop-the-world identity of an unbound JNI caller) skips unmounted entries: a parked virtual thread's mount published the carrier's tid after the carrier did and nothing republishes the carrier at unmount, so after the unmount the carrier was answered with the parked continuation (a monitor owned under, or a pause excusing, a thread that runs nowhere). `=0`: the newest publish wins, unmounted or not. |
| `CRATONVM_NATIVE_THREAD_DEAD_QUEUE_PER_VM=0` | ON (`=0` kills); every collector (thread identity); a one-VM process drains exactly as before | The XNIO I/O thread and Vert.x event loop death queues (used when the thread does not retire itself: G1, ZGC, `CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK=0`) carry the identity of the VM that registered each thread; a drain marks dead only its own VM's threads (another VM's same-numbered live thread is no longer marked dead and its monitors no longer released), a full queue drops its oldest row instead of every pending death, and the XNIO queue -- which nothing drained, so every queued XNIO death was lost -- is drained at the next I/O-thread spawn. `=0`: one process-wide drain, clear-on-full, no XNIO drain. |
| `CRATONVM_GEN_STW_HOLD_SEES_SIBLING_PAUSE=1` | OFF (opt-in); Generational allocation-driven collections only (explicit doors -- `System.gc()`, jcmd, JVMTI, heap dump / inspection -- always collect) | A collection request whose resume-fairness hold released the barrier lock and let a sibling's whole pause run and complete (the barrier generation moved) loses its request, as HotSpot skips a queued `VM_CollectForAllocation` when the collection count moved: its allocation retry then uses the sibling's collection instead of requesting a back-to-back duplicate one. Losses are counted (`GcBarrier::hold_sibling_pause_losses`). |
| `CRATONVM_JIT_IR_SATB_GATE_ONCE=1` | OFF (opt-in); Generational mask plan with an inline card view only (G1 and ZGC keep every byte) | The optimizing tier's gated reference `putfield` / `aastore` tests its SATB pre gate once, at the top, as one RIP-relative `cmp byte [rip+d32], 0` when the gate byte is within 2 GiB (else the old two instructions), and the inline card arm no longer tests it again before the store: nothing in between is a GC point, and a peer frozen there by the take-over is covered by the initial mark's frozen-peer scan. Refused under `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN=1`. `CRATONVM_DBG_JITC=1` prints `gen ref store: one SATB gate rip=<bool>` per site. Proposal `gcj-j2jt2-proposal-generational-satb-gate-in-one-instruction-once-per-store`. |
<!-- gcj j3/rt3 -- end -->

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
  [`internal/zgc-round-20260920/zgc-roadmap-20260920.md`](internal/zgc-round-20260920/zgc-roadmap-20260920.md).
  It also lists which claims in the two older ZGC plans are now out of date.
- Architecture overview: [`ARCHITECTURE.md`](../ARCHITECTURE.md).
- Per-module algorithm notes: every `gc/src/*.rs` file starts with a `//!`
  doc block (e.g. [`gc/src/g1.rs`](../gc/src/g1.rs),
  [`gc/src/concurrent_mark.rs`](../gc/src/concurrent_mark.rs),
  [`gc/src/satb.rs`](../gc/src/satb.rs)).
- Lock hierarchy (heap is L8): [`vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs).
- Embedding the VM from Rust: [`docs/EMBEDDING.md`](EMBEDDING.md).
- Profiling: [`docs/PROFILING.md`](PROFILING.md).
