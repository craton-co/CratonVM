# Generational concurrent cycle and old generation: review residuals (gcj j1/og)

> **STATUS (2026-10-19, gcj j3/og3): see `gcj-j3og3-old-gen-review-residuals-20261019.md`** for what of C1, C4-C7 and O4-O10 is still open after this wave (O5, O10 and parts of O6, O9, C7 fixed in code).

> **STATUS (2026-10-19, gcj j2/og2): PARTLY FIXED IN CODE, PENDING BUILD/PROBE.** Fixed: **C2** (default ON, `CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN=0` kills: the cycle's mark bitmap spans `OldGen::growth_max`; new counter `[GC] conc_cycles: conccyc_bitmap_cover_refusals=`; option (b) without dropping the `reclaim_epoch` bump, which stays: a growth runs only inside a stop-the-world major, which moves the epoch anyway), **C3** (seen-set dedup past 32 entries, same rows), **C8** (the comments; `is_marking_active` doc only, shared with G1, no behaviour change), **O1** (opt-in `CRATONVM_GC_OLD_SHRINK_FACTOR`: HotSpot's 0/10/40/100 % `_shrink_factor`), **O2** (`grow_after_refusal` treats `MinHeapFreeRatio=100` as `min_free_ratio_capacity` does), **O3** (range screen before the young grid search), **O8** (the y2o cost note and the closure-admission default). Still open: C1, C4, C5, C6, C7, O4, O5, O6, O7, O9, O10, carried to `gcj-j2og2-old-gen-review-residuals-20261019.md` with this wave's new findings. Rows and commits: `docs/internal/gc-round-gcj-20261019/j2-og2-report.md`.

> **STATUS (2026-10-19, gcj j1/og): OPEN -- found by reading this wave; NOT fixed (the wave closed first). Carried to wave 2.** Generational only unless an item says "shared with G1". Line numbers are against branch `gcj1-og` at the commit that filed this page; search the names.

These come from this wave's read-only review of `gc/src/concurrent_mark.rs` (h4pc item 6) and of the old-gen helpers. The one review item fixed in code is listed in `docs/internal/gc-round-gcj-20261019/j1-og-report.md` and is not repeated here. "Confirmed" means confirmed by reading; none was measured.

## Concurrent cycle (`gc/src/concurrent_mark.rs`)

**C1. MEDIUM-HIGH (perf): each concurrent sweep slice re-sorts the whole free list.** Confirmed.
- What happens: `old_gen.free` (sweep, ~6272/6299) does not coalesce and dirties the sorted-free cache. The next slice's `walk_objects_from` then rebuilds and sorts the cache and skips linearly to `from`. The block count grows with every dead run freed so far, so the sweep is roughly S·F·log F.
- Fix: coalesce at the end of each sweep slice (under the lock), or merge the freed spans into the cache. Then replace the linear skip with a `partition_point` plus a prefix-max of block ends.

**C2. MEDIUM (wasted cycle): the cycle's bitmap is sized BEFORE the initial-mark pause.** Confirmed.
- Where: `gc_and_alloc.rs` (~25807) sizes it from `old_gen_info()`.
- A growth (`grow_after_refusal`, and the new opt-in `grow_to_min_free_ratio`) can land between the sizing and the pause, on another thread's major: during `InitialMark`, `cycle_in_progress()` is false.
- The cycle then traces fully, and only the remark refuses it (`bitmap_covers_generation`, ~5653). It is counted only as `swept_nothing`.
- Fix (b preferred):
  - (a) Test coverage in `initial_mark` under the lock and refuse early.
  - (b) Size the per-cycle `MarkBitmap` / `reference_skip` to `old_gen.growth_max()` (`alloc_zeroed`, so it costs nothing until touched). Then drop the `reclaim_epoch` bump from `OldGen::grow_in_place_to`, after auditing the other epoch readers.
  - Either way, add a `conccyc_bitmap_cover_refusals` counter.
- **Run this before any flip of `CRATONVM_GC_OLD_FREE_RATIO_GROWTH`.**

**C3. MEDIUM (pause): `ClassUnloadTables::absorb` dedups each row by `Vec::contains`, O(n²) inside the remark pause.** Confirmed (~3628). Fix: a seen-set, or extend + `sort_unstable` + `dedup`.

**C4. MEDIUM (pause): the initial mark walks old gen into a `Vec` (16 B per object) and builds two start bitmaps from it, inside the pause.** Confirmed (~4822; `OldGenObjectStarts::build` ~7183 on each rebuild). Fix: build once with `for_each_object`, and clone the bits for the cache copy.

**C5. MEDIUM (perf): a stripe mutex is taken per 16-byte legacy slot (`volatile_stripe_lock`, ~6802), including in the stop-the-world remark drain, where it protects nothing.** Confirmed. Fix: thread an `in_pause` flag and skip the lock there.

**C6. LOW: smaller cleanups.** Confirmed.
- `finish_sweep_as` clears a bitmap that is about to be dropped (~6508).
- `sweep_free_epoch` is dead and its doc is wrong; `HeapRealm::address_epoch` moves on every sweep slice, including empty ones (~101, ~6222).
- The compact layout is resolved twice per scanned object (~7103/~6687).
- A fresh local mark stack is allocated per slice (~5141, ~5374).
- The retained-layout census does a hash probe per survivor (~3958).
- `remark_is_marked` takes the `sweep_eligible` mutex per query (~5504).
- The multi-worker drain machinery is test-only, but every push pays `idle_hint.load` (~3249, 3394-3504).

**C7. LOW (suspicion): dead finalizables are retained and re-traced on every concurrent cycle until a stop-the-world collection runs, and `retained` accumulates duplicates (~5944-5998).** Fix: dedupe `retained`, and drop retained candidates from the list.

**C8. LOW: comments that contradict the code.** Confirmed.
- "verified by `old_gen.contains()`" (~6575, ~6762): admission is `markable_old_object`.
- `contains(obj_ptr + total_size - 1)` (~6774): the code uses `contains_range`.
- The sweep TAMS bullet (~6176) omits the "never walks back below `p.next`" argument.
- "Mark bitmap covering the old generation" (~3748).
- "three times per cycle" (~7172).
- **Shared with G1:** `is_marking_active`'s doc (~1199). The barrier gates on `SatbQueue::is_active()`. Fix the doc only.

## Old generation and its helpers

**O1. MEDIUM (parity): shrink damping is all-or-nothing, not HotSpot's `_shrink_factor` 0/10/40/100 %.** Confirmed.
- Where: `OldGen::resize_after_collection`.
- The doc claims parity. The code also DAMPS the case HotSpot takes back (capacity committed by promotions since the prologue).
- Fix: a `shrink_factor` field (0 → 10 → ×4, capped at 100; reset when no shrink is due). Release `excess * f / 100` per collection. Or correct the doc.

**O2. MEDIUM (parity): `-XX:MinHeapFreeRatio` acts only after a refused allocation unless `CRATONVM_GC_OLD_FREE_RATIO_GROWTH=1`.**
- This is the flip the j1/og rows decide (block G of the report).
- Also: `grow_after_refusal` at `MinHeapFreeRatio=100` computes `used * 100 / 1`, while the new `min_free_ratio_capacity` maps 100 to the limit. Align them.

**O3. MEDIUM (perf, default-ON pauses): `gen_heap_y2o_live.rs` `seed` binary-searches the young grid for every old→old slot.** Confirmed (~632/647/728/300). Fix: range-screen `addr` against `[base, end)` before `mark_grid_object`. `is_owner`'s linear `values().any` (~612) becomes a set.

**O4. LOW: `close_live_set_from_seeds` scans a seed twice when an earlier seed promoted it** (`old_gen.rs` ~4749-4777; the opt-in `CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL` only). This is double work, not a wrong answer. Fix: snapshot the seeds' marks before Phase A.

**O5. LOW-MEDIUM (perf): the free list is sorted twice per in-place sweep.** `coalesce_free_blocks` re-sorts itself, then invalidates the cache, and `with_sorted_free_blocks` uses a stable sort that allocates. Fix: reuse or refresh the cache; use `sort_unstable_by_key`.

**O6. LOW (perf): `compact_around_pins_watching`.**
- `live` grows without reserve.
- `watch.binary_search` runs per dead object although the walk ascends.
- Phase 0b builds its own start bitmap; thread the shared start set through, as `compact_with_walked_objects_shared` now does for the full slide.

**O7. LOW (release hazard): `release_unused_tail`'s alignment / minimum-size check is a `debug_assert!` only; `free` has no alignment check.** Fix: refuse those cases in release too.

**O8. LOW: dead code and stale docs.**
- `bot.rs` `object_at`/`bot_object_at`/`bot_compress` and the `oracle_*` stats are test-only since the O(live) sweep's deletion.
- `gen_heap_live_sweep.rs` holds only unrelated tests.
- Stale comments:
  - `close_live_set_starts` "binary search (the default)";
  - the `mark_bitmap.rs` test doc on `any_marked` ordering (~596);
  - `gen_heap_y2o_live.rs:85` "only when the flag is on";
  - old-gen docs hard-coding 40/70 where the code reads `min_free_percent` / `max_free_percent`.

**O9. LOW (perf): `MarkBitmap::is_marked` locates twice (~266).** The per-compaction / per-closure `runtime_flag_default_on` reads (`CLOSE_BITMAP_FLAG`, `FORWARD_INDEX_FLAG`) should be read once per generation in `OldGen::new_on`.

**O10. LOW: `give_back_candidates` counts interior holes that are already decommitted** (`CRATONVM_GC_OLD_INTERIOR_DECOMMIT` only), so its A/B sample is inflated.

## How to verify

Each fix lands with its own unit test in the crate's style. Run the perf items (C1, C4, C5, O3, O5) on the concurrent rows `GenR5W5ConcUnloadProbe` and `GenR4W4SteadyPromotionProbe 4 -Xmx512m`, plus `Gcj1OgFullGcCompactProbe`, as interleaved CPU pairs.
