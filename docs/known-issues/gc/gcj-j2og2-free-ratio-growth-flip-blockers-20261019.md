# What blocks a default flip of `CRATONVM_GC_OLD_FREE_RATIO_GROWTH` (gcj j2/og2)

> **STATUS (2026-10-20, gcj j3/og3, j3a rows): B3 PARTIAL.** `og3_b3_nums` DIFF on j3a (the pools no longer summed to `maxMemory`); fixed in code: the tenured max is `min(growth limit, maxMemory - max(from, to))`, never below the capacity, which is HotSpot's fixed figure only while the young arena is below its maximum (residuals page P6). B2 VOID on j3a (no growth at peak 55 %): re-run with `Gcj3Og3PeakThenFallProbe 80 2048`.

> **STATUS (2026-10-19, gcj j3/og3): B2 and B3 BUILT behind opt-in, PENDING BUILD/PROBE.** B2: `CRATONVM_GC_OLD_CAPACITY_SHRINK` (the capacity shrinks back; the young trigger gets the ceded budget back). B3 decided: `CRATONVM_GEN_TENURED_POOL_MAX_FIXED` reports `OldGen::growth_limit` (HotSpot Serial's fixed tenured maximum, `2/3 · -Xmx` with headroom) as the `Tenured Gen` max and feeds the same figure to the young pools' max, so the pools still sum to `maxMemory()`; it still moves if a refused request grows the generation past the soft cap (residuals page P6). B4 (the measurement) now includes block B2 of `docs/internal/gc-round-gcj-20261019/j3-og3-report.md`: the flip of `CRATONVM_GC_OLD_FREE_RATIO_GROWTH` should be tried together with `CRATONVM_GC_OLD_CAPACITY_SHRINK=1`.

> **STATUS (2026-10-19, gcj j2/og2): OPEN -- B1 FIXED IN CODE (pending build/rows); B2 and B3 open, found by reading this wave; B4 is the measurement.** Generational only. Line numbers are against branch `gcj2-og2`; search the names.

`CRATONVM_GC_OLD_FREE_RATIO_GROWTH` (gcj j1/og, opt-in) is HotSpot Serial's `TenuredGeneration::compute_new_size` expansion: after a stop-the-world old-gen collection the old generation grows until `MinHeapFreeRatio` (40 %) of its capacity is free, up to the soft cap (`2/3 · -Xmx`). `OldGen::grow_to_min_free_ratio`, called from `GenerationalHeap::after_old_gen_collection`. Without it a live set between `Xmx/2 · 0.75` and `2/3 · Xmx` runs a major on almost every young cycle (j1-og finding 4). This page is what has to hold before it can be ON by default.

## B1. FIXED IN CODE: a growth wasted the concurrent cycle that was about to open (C2)

- `maybe_concurrent_gc_at` (`vm/src/runtime/interpreter/gc_and_alloc.rs`) sized the cycle's mark bitmap from `old_gen_info()` before the initial-mark pause. A growth in that window (another thread's major) left the grown span outside the bitmap, and `remark_begin` refused the whole traced cycle (`bitmap_covers_generation`), counted only as `swept_nothing`.
- With the switch ON every full collection that leaves less than 40 % free grows, so this window stops being rare.
- Fix: the bitmap spans `OldGen::growth_max` (`GenerationalHeap::old_gen_mark_span`), default ON, `CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN=0` kills; refusals are counted as `[GC] conc_cycles: conccyc_bitmap_cover_refusals=`. Unit test `concurrent_mark::tests::gcj_j2og2_a_bitmap_over_the_growth_limit_survives_a_growth_before_initial_mark`.
- Only the marker's bitmap takes the wider span: the reference-skip candidate screen (`gen_conc_reference_skip_candidates`) and the class-unload capture read headers in the span they are handed and keep the capacity span.
- Pending: block C of `docs/internal/gc-round-gcj-20261019/j2-og2-report.md` (`conccyc_bitmap_cover_refusals=0` on every ON row).

## B2. OPEN: the young generation's cede is one-way, because the old capacity never shrinks

- `grow_in_place_to` raises `OldGen::data.len()`; nothing lowers it again. `resize_after_collection` shrinks the COMMITTED size, never the capacity.
- After any growth past the construction capacity, `GenerationalHeap::borrowed_young_trigger_cap` caps the young trigger at `(Xmx - old capacity) / 2` (`cede_young_budget_to_old`, and again in the serial-eden path, `gen_heap.rs` ~13745). The cap is computed from the capacity, so it stays at the PEAK capacity's value for the rest of the run, also after the live set falls and the shrink has returned the committed memory.
- With the switch ON the capacity reaches the soft cap (`2/3 · Xmx`) on any program whose live set once passed 60 % of it: the young trigger is then at most `Xmx / 6` for good. HotSpot Serial's eden stays at its own size (about `0.27 · Xmx` at `NewRatio=2`, `SurvivorRatio=8`) whatever the tenured generation does. That is up to ~1.6x the young collections, on every later young cycle -- the shape lane yg2 is chasing on the young side ("young triggers at ~0.6x HotSpot's eden").
- Closes when the capacity can come back down: a capacity shrink (`OldGen::ungrow_to`) in the stop-the-world resize, when the trailing free run is above `max(soft target, construction capacity)` and wholly uncommitted, followed by the same republish the growth does and a recomputed (raised) young cap. Proposal `docs/internal/proposals/gc/gcj-j2og2-proposal-old-capacity-shrinks-back-so-the-young-cede-is-reversible-20261019.md`.
- Until then the flip rows must include the young-collection count (`[GC]` summary `young`) against HotSpot's `-Xlog:gc` `Pause Young` count, on a row whose live set peaks and falls.

## B3. OPEN (parity, observability): `Tenured Gen`'s pool `max` moves with every growth

- `GenerationalHeap::jmx_memory_pools` reports `Tenured Gen` with `max: Some(old_cap)`, the CURRENT capacity (`gen_heap.rs` ~20000).
- HotSpot Serial's tenured max is fixed at start (`MaxHeapSize` minus the young maximum). With the switch ON, `MemoryPoolMXBean.getUsage().getMax()` grows during the run on any program that triggers a growth (it already does after a REFUSED allocation under `CRATONVM_GC_OLD_BORROW_YOUNG`, but rarely).
- Closes with the pool max reported as the growth limit HotSpot would report (`min(growth_soft_max, growth_max)` when the generation was built with headroom), and the young pools' `max` as the matching remainder, so the pools still sum to `Runtime.maxMemory()`. That function is the observability lane's (pool functions); cross-lane hunk X2 of `j2-og2-report.md` has the diff.
- Probe for the closure: `tools/bench/GenR5W2HeapNumbersProbe.java` with the switch ON, and a `getMax()` read before and after the live set grows.

## B4. The measurement

The j1/og block G rows (`Gcj1OgFreeRatioGrowthProbe 96 400 8 4` at `-Xmx256m`, `g_off` / `g_on` / `g_on_borrow_off`) and the gm-I5 javac rows (`cvm_wide_192m`, `cvm_wide_256m`) with the switch ON, plus block C of `j2-og2-report.md` for B1. The flip bar: every SAME row stays SAME, interleaved CPU <= 1.05x on every row, `trig_collections` toward HotSpot's `Pause Full` count, and the young-collection count not above the OFF arm's by more than 5 % (B2) until B2 is closed.
