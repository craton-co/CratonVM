# Proposal: run the old generation's mark and compaction phases on the persistent evacuation pool

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 30
> of 54).** Not built (no `CRATONVM_GC_OLD_PARALLEL`, no phase timer).
> **Gate:** step 1 first: an old-gen phase timer on `OldGenRsetProbe 19 700
> 16` and `GenR5W5OldLiveSweepProbe`; build only the dominant phase, 1 vs 4
> workers interleaved, program output identical. **Size:** S (timer), M (Phase
> 2), L (mark).

> **STATUS (2026-09-27, gen r5w6/old10): REFINED, not implemented.** The
> reachability question, answered by reading:
>
> * `sweep_old_gen_non_moving_impl` has `&self`, so `self.evac_pool(n)`
>   CAN reach `old_gen_gc_inner` on the non-moving path — the path every
>   JIT-warm old-gen collection takes. But what it would parallelise there is
>   (a) the MARK (step 4, the biggest design: work-stealing deques, a CAS on
>   the mark word, a `Sync` oracle without path compression) and (b) Phase 2
>   of `compact_around_pins`, which runs only on a fragmentation request
>   under the opt-in pinned compaction or the default humongous compaction —
>   rare, and small next to the mark it follows.
> * `compact_walked` (the moving path's compaction, where Phase 2 is worth
>   parallelising: every live object's slots, the whole generation's live
>   set) is reached only through `major_gc_finalizing`, whose callers are
>   `collect_garbage_inner`'s Phase 5 — lane `pin10`'s region this wave.
>
> So the half worth having needs a signature change in another lane's
> region, and the half this lane can reach is either the riskiest step
> (the mark) or not worth a flag. Not landed in an unbuilt wave. The order
> to land it in, refined:
>
> 1. **Measure first.** No per-phase timer exists for the old-gen
>    collection (`CRATONVM_DBG=gcpause` times the young phases). Add
>    `report_phase`-style marks to `old_gen_gc_inner` (mark / closure /
>    compact Phase 1-4 / free loop) under the existing `dbg_gcphase` flag,
>    and run `OldGenRsetProbe 19 700 16` (`-Xmx1g`,
>    `CRATONVM_OLDGEN_COMPACT=1`) and `GenR5W5OldLiveSweepProbe`: the split
>    says whether Phase 2 or the mark dominates. Only the dominant one is
>    worth the risk.
> 2. **Plumbing, one cross-lane diff** (for the owner of `collect_garbage_inner`
>    Phase 5; the rest is this lane's): add `pool: Option<&crate::evac_pool::EvacPool>`
>    as the last parameter of `major_gc_finalizing` and `old_gen_gc_inner`
>    (every other caller passes `None`), and at both Phase-5 calls
>    ```diff
>     Self::major_gc_finalizing(
>         roots,                       // (`&mut major_roots` at the second)
>         &young_from,
>         &mut old_gen,
>         &young_skips,
>         Some(&mut old_fin_pass),
>    +    gc_flags().old_parallel.then(|| {
>    +        let p = self.evac_pool(crate::young_mark::young_gc_threads(usize::MAX));
>    +        p.ensure_helpers(crate::young_mark::young_gc_threads(usize::MAX).saturating_sub(1));
>    +        p
>    +    }),
>     )
>    ```
>    (`old_parallel` a new opt-in `CRATONVM_GC_OLD_PARALLEL`; the pool is idle
>    by Phase 5 — the young evacuation's `scope` has returned — and `scope`
>    must not be entered from inside another `scope`, which Phase 5 is not.)
> 3. **Phase 2 of `compact_walked` on the pool**, as designed below; the
>    body is `OldGen::update_refs_in_object`, an associated function over a
>    `(data_start, data_end)` pair and a read-only `FxHashMap` — nothing of
>    `&OldGen` crosses the dispatch. Chunk `live` by index; convert raw
>    pointers to `usize` before the closure (a `*mut u8` is not `Sync`).
>    Before landing, confirm `forward_ref_slots`' compact-object layout lookup
>    is safe from pool threads (the parallel young evacuator already calls
>    the same lookup from them, which is the evidence to cite).
> 4. The mark, last, as designed below.
>
> Verification unchanged (below), plus the phase timer's split before and
> after.

*Filed 2026-09-27 by gen round 5, wave 5, lane `old9`. A design, not a
defect; nothing here was built or measured. Successor of step 1 and step 3 of
`../../internal/gc/gengc-r4w4-oldgen4-proposal-parallel-compaction-and-an-o-live-sweep-RETIRED-20260927.md`
(whose O(live) half landed opt-in this wave as `CRATONVM_GC_OLD_LIVE_SWEEP`).*

## Why it did not land this wave

Every old-gen phase that could run in parallel lives in associated functions
with no `&self`: `GenerationalHeap::old_gen_gc_inner`, `major_gc`,
`major_gc_finalizing`, `OldGen::compact_walked`, `OldGen::compact_around_pins`.
The pool (`GenerationalHeap::evac_pool(workers)`, an `OnceLock<EvacPool>`) is
reachable only through `&self`. `sweep_old_gen_non_moving_impl` has `&self`,
but `major_gc` is called from the moving path's Phase 5 in
`collect_garbage_inner` (lane `pin9`'s region this wave), so threading a pool
through both needs a signature change in two lanes' regions at once. And
`OldGen` is not `Sync` (its sorted-free-list cache and block-offset table sit
in `RefCell`s), so a worker cannot hold `&OldGen`: every parallel phase has to
be written against plain slices and integers captured before the dispatch.

## Design, in the order it is safe to land

1. **Plumbing (no behaviour change).** Add `pool: Option<&EvacPool>` to
   `old_gen_gc_inner` (and the two `major_gc*`), `None` from every caller but
   the two that have `&self` (`sweep_old_gen_non_moving_impl`, and
   `collect_garbage_inner`'s Phase 5 through `self.evac_pool(policy_workers)`).
   Gate everything below on `CRATONVM_GC_OLD_PARALLEL=1` (opt-in).
2. **Parallel Phase 2 of both compactors** (reference forwarding). In both
   (`compact_walked`, `compact_around_pins`) every live object's slots are
   rewritten in place from a READ-ONLY `destinations` map built by Phase 1
   (neither reads a referent's header, which is what makes this safe: a
   header forward would sit in a compact instance's first field, which
   another worker may be rewriting), and each worker writes only into the
   objects of its chunk: chunk `live` by index into `pool.helpers() + 1`
   ranges, each worker runs `OldGen::update_refs_noting_young` /
   `update_refs_in_object` over its range and returns its own
   `young_ref_holders` `Vec`, merged after the barrier. Raw pointers cross
   the boundary as `usize`; `FxHashMap<usize, usize>` is `Sync`. The
   copy-fields-first discipline of `for_each_old_gen_ref` is per object.
3. **Segment-parallel Phase 3 of `compact_around_pins`** (the slide). Every
   pin ends a segment: objects between two consecutive pins slide into
   `[previous pin end, next pin start)` and never outside it, so segments are
   independent; each is slid serially low to high by one worker. Useless with
   no pin (`compact_walked`), where the slide needs HotSpot's region
   dependency scheme instead (a region may be written only after every region
   its destination overlaps was read).
4. **Parallel mark** (the phase every old-gen collection pays). Work-stealing
   deques on the pool with the young `drain_parallel` termination protocol;
   the mark bit set by a compare-and-swap on the mark word (today
   `gc_flags() & MARKED == 0` then `add_gc_flags`, a read-then-OR that two
   workers can both pass — benign for correctness, a double scan). The
   admission oracle must be `Sync`: the walked grid is (a `&[(usize, usize)]`
   copy), the block-offset oracle is not (its path compression writes the
   table) — read-only queries against a table rebuilt at the previous
   collection would be, with compression skipped in parallel mode.

## How to verify

`OldGenRsetProbe 19 700 16` at `-Xmx1g` with `CRATONVM_OLDGEN_COMPACT=1`
(every major compacts), 1 vs 4 workers, interleaved binaries, medians:
program output identical, `COMPACT_ESCAPE_HITS == COMPACT_WALK_GAP_HITS ==
COMPACT_STALE_GRID_REWALKS == SCAN_REGION_BREAK_HITS == 0`;
`GenR4W4HumongousFragProbe` and `GenR4W5OldPinnedCompactProbe` (`-Xmx128m`)
unchanged; the unit tests of both compactors
(`compact_records_identity_map_for_watched_stationary_survivor`,
`a_pinned_object_keeps_its_address_and_the_rest_slides_around_it`,
`with_no_pins_the_pinned_compaction_matches_compact`) run with the pool at
width 4.
