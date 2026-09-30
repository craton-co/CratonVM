# Proposal: the O(live) old-gen collection resurrects finalizables itself, keeps its set in a bitmap, and stays exact across concurrent sweeps

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 29
> of 54).** Not built. The one page for what the O(live) collection still
> costs; absorbs
> `../../internal/gc/gengc-r5w5-old9-proposal-exact-anchors-for-promotion-buffers-REJECTED-20260928.md`
> as item 4. Worth doing only together with the flip of
> `CRATONVM_GC_OLD_LIVE_SWEEP`, which stays NOT YET: no d7 row ran it, and its
> open gate is the heap-full thrash rows (2 of 3 failed with it on the d3
> build). **Gate:** the page's per-item tests, then `GenR5W5OldLiveSweepProbe`
> with `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY=1` `mismatches=0` and a lower
> `steady-major` median. **Size:** M (items 1, 2), S (item 3), S (item 4).

*Filed 2026-09-27 by gen round 5, wave 6, lane `old10`. A design, not a
defect; nothing here was built or measured. Owner: the old-generation lane
(`gc/src/gen_heap_live_sweep.rs`, `gc/src/old_gen/**`), plus the concurrent
lane for item 3. Follows the review and cost model on
`../../internal/gc/gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep-DONE-20260928.md`
(STATUS).*

Three costs the O(live) collection (`CRATONVM_GC_OLD_LIVE_SWEEP`) still pays,
in the order they are worth removing.

## 1. A dead old-gen finalizable throws the whole mark away

Since gen r5w6/old10 the live collection runs with finalizer candidates in
old gen, and DECLINES when the strong closure left one of them unreached: the
walked collection then marks again from scratch and resurrects it. On a
workload whose finalizables die in old gen, every such major pays two marks.

**Design.** Do the resurrection in `old_gen_gc_live`, as the walked BFS does
it (`OldGenFinalizerPass::seed`, `resurrection_draining`):

* pass `Option<&mut OldGenFinalizerPass<'_>>` instead of the candidate slice;
* turn the drain into the walked path's `loop`: when the worklist first runs
  dry and an old-gen candidate is unreached (`object_at` = `Base`, mark bit
  clear), first close the marked set (`close_live_set_by_oracle` over
  `(base, size)` of the marked set — `seed`'s `close_live_set`), record each
  object that closure promoted with `note_marked`, then mark, `note_marked`
  and push every candidate still unreached, push it to `pass.resurrected`,
  and drain again recording every popped object in `pass.closure`;
* on ANY later fallback, `old_gen_gc_inner` truncates `pass.resurrected` and
  `pass.closure` back to their lengths at entry (the walked collection then
  refills them), and `OLD_FINALIZERS_RESURRECTED` is bumped only when the
  live collection completes.

**Verify.** `gen_heap::live_sweep::tests::a_reached_old_finalizable_keeps_the_live_sweep_and_an_unreached_one_declines_it`
changes its unreached arm to `live_sweeps == 1`, `live_sweep_fallbacks == 0`,
`resurrected == [fin]`; a new test checks `pass.closure` equals the walked
path's for a candidate with a two-object subgraph;
`tools/probes/OldGenFinalizeProbe` with the flag prints `64/64 PROBE-OK`.

## 2. The kept set is a hash set, touched once per edge

`LiveOracle::marked` is an `FxHashSet<usize>`: one insert per newly marked
object, and — through `retrace_stale_mark` — one more (failing) insert for
EVERY edge that reaches an already-marked object, i.e. one hash operation per
reference on a well-connected graph, which the walked mark does not pay. Then
the set is copied to a `Vec`, each kept object is sized by a SECOND
`object_at`, and the list is sorted (L log L).

**Design.** A per-pause `crate::heap_bitmap::HeapBitmap` over the
generation's storage (one bit per 8 bytes; `claim` is the atomic
test-and-set; allocated zeroed, so its cost is the touched pages):
`note_marked` = `insert`; `retrace_stale_mark(a)` = `claim(a)` (true only the
first time); the kept list = the bitmap's set bits in ascending order (no
sort), sized by one decode of each kept header (it is a proven base), not a
second oracle query. The bitmap is also the natural `Sync` kept set for a
parallel mark (`gengc-r5w5-old9-proposal-parallel-old-gen-phases-on-the-evac-pool-20260927.md`,
step 4).

**Verify.** The live-sweep unit tests unchanged; `GenR5W5OldLiveSweepProbe`
with `CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY=1`: `mismatches=0`; its
`[probe] steady-major ms=` median lower than before (the steady major is the
edge-heavy one).

## 3. A concurrent sweep leaves the next live collection one linear walk

Every `OldGen::free` lowers the block-offset table's trusted prefix to the
freed block's card (`note_free`). The concurrent sweep frees through `free`,
from low addresses up, so after it the next stop-the-world live collection's
first query above that card re-derives the table with a linear walk
(`bot_extend`) — the one A-proportional pass the live collection still pays,
and on a concurrent-first run (where most majors follow a concurrent sweep)
the common case.

**Design.** The concurrent sweep knows its kept set (the cycle's mark
bitmap). At its end — inside the slice that finishes, under the old-gen lock
— call `OldGen::rebuild_block_offsets_from_live` over the objects it kept
(walked in ascending order from the bitmap), exactly as the live sweep does
after `sweep_dead_runs_around`. Precondition to check: that slice holds the
lock for the whole rebuild (O(kept + their cards)) and nothing allocated in
old gen between the sweep's last free and the rebuild (a promotion between
slices writes `note_alloc` anchors, which the rebuild would keep only for
kept objects — so the rebuild must run in the same lock hold as the last
free, or re-derive from the allocation's anchors).

**Verify.** `BlockOffsetStats::rederived_objects` stays 0 across a
concurrent cycle followed by a stop-the-world live collection (unit test in
`gen_heap`); `GenR5W5OldLiveSweepProbe` under
`CRATONVM_GC_CONC_START_PERCENT=40`: `sparse-major` median no higher than
without concurrent cycles.

## 4. Merged from `gengc-r5w5-old9-proposal-exact-anchors-for-promotion-buffers` (d8/y, 2026-09-28): anchor a retired promotion buffer exactly

Retired as a duplicate of this page. A promotion buffer (16-256 KiB) is one
old-gen allocation, so `note_alloc` anchors all its cards at the buffer's
start and the first `object_at` into its last card strides over every object
carved before it; right after heavy promotion that makes the first live
collection O(promoted objects). Design: at retirement, under the old-gen lock,
`OldGen::anchor_carved_span(start, cursor)` walks the carved objects and
anchors each card at the object covering its first byte, committing only if
the walk lands exactly on `cursor`. Call sites `PromotionQueue::retire_plab`
(`gen_heap.rs`) and `ParEvac::retire_old_plab` (`gen_evac.rs`), which need the
buffer start kept beside its cursor. Same flag as the live sweep. Verify: a
400-object buffer retired through the call, then `object_at` on its last
object costs at most one card's strides (`BlockOffsetStats::oracle_strides`);
a `GenR5W5OldLiveSweepProbe` variant timing the FIRST major after tenuring.
