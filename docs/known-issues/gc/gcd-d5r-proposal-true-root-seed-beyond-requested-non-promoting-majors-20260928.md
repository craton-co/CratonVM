# Proposal: carry the true-root young seed to the cases it falls back on, and to the concurrent remark

> **STATUS (2026-09-28, gcd d9/a): steps 1 and 2 BUILT (step 2 default on,
> step 1 opt-in); step 3 not built.** As the round's rank-1 item this page was
> the fix of three defect pages, and gcd d9/a built it in `gc/src/gen_heap.rs`
> (with a watch list added to `OldGen::compact_around_pins_watching`):
>
> - **Step 2 (promoting cycles), as designed below, plus the planned pinned
>   compaction:** `sweep_old_gen_non_moving_impl` hands `(source,
>   destination)` of every promotion to `old_gen_gc_inner` (`TrueRootAsk`,
>   items 1 and 5: the destinations stay in the slice but the root loop and
>   the census skip them, `promo_tail`); `TrueRootYoung::seed_word` resolves a
>   young address in a promoted source extent to its destination (item 2) for
>   the roots, the finalizer candidates and the watched set (item 4); `round`
>   follows the `mirror_pin` / `metadata_pin` rows keyed by a marked
>   destination's SOURCE (item 3; `external_roots` were remapped at the
>   commit). New, not in the design: a destination nothing reaches is freed,
>   so its source leaves the pointer map the collector returns
>   (`TRUE_ROOT_SWEEP_OUT`, `run_non_moving_young_cycle`), decided from the
>   free list after the in-place sweep and from the compaction's own report
>   after a pinned one. The pinned plan keeps the seed because `seed_stretches`
>   now pins what an unparseable young stretch names, as the legacy seed's
>   conservative fallback does. Default on; `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0`
>   restores the d4/n..d8 fallbacks. Every remaining fallback is counted by
>   reason (`crate::old_gen::TrueRootFallback`: walk gap, promotion off the
>   grid, and -- with `WIDE=0` -- promoted / pinned plan; plus a requested
>   major on the moving cycle).
> - **Step 1, option (B)** (not (A): a second young sweep would age the
>   survivors twice, judge the finalizers twice and need the caller's roots
>   remapped): `TrueRootYoung::reclaimable_runs` + `sweep_old_gen_non_moving_impl`
>   zero the excluded young survivors onto the young free list, only after an
>   in-place sweep that freed exactly the unmarked set. The verdicts the
>   design worried about read the memory state (`is_live_young_survivor`'s
>   header words, `reclaimed_hole_at`'s free list), and none of the reclaimed
>   objects is watched, a finalizer candidate or in the pointer map (the seed
>   started from all three). Opt-in: `CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG=1`;
>   flip gate in `gcd-d9a-proposal-true-root-seed-for-every-stw-major-20260928.md`.
> - **Step 3:** not started.
> - **Gates (unchanged):** step 1: `CRATONVM_GC_OVERHEAD_PROGRESS=0
>   CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG=1 ... -Xmx64m
>   GenR4W4NativeStringOomProbe` prints the four lines 5/5 with the thrash
>   and ThreadsOom rows no worse; step 2: `oldsz_true_root_fallbacks=0` on the
>   pcallee and texit rows, which print HotSpot's lines 3/3 (the runs are on
>   those pages). **Retire this page** when step 2's gate passes and step 1 is
>   flipped or rejected; step 3 then moves to its own page.

*Filed 2026-09-28 by gcd d5/r (conc5), from the review of gcd d4/n's
`TrueRootYoung` and the orchestrator's `GenR4W4NativeStringOomProbe` runs on
d916d1c40. Proposal: no code landed here beyond d5/r's fixes to the existing
seed (see `gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-20260928.md`).*

## Where the true-root seed stops today

`gc/src/gen_heap.rs::TrueRootYoung` (d4/n) makes a REQUESTED major's old mark
seed young from the true roots instead of from every young survivor, so dead
old data a dead old holder's young child names is freed in one major. It
covers exactly one shape:

| Case | Today | Consequence |
|---|---|---|
| requested major, non-moving cycle, nothing promoted, gap-free old walk, no pinned plan | true roots | old half freed in one major |
| the young phase promoted anything (`promo_seeds > 0`) | legacy seed | a dead young `elementData` promoted this pause keeps its chain one more major |
| old walk gap / pinned compaction | legacy seed | same |
| occupancy-triggered major (moving Phase 5 or non-moving) | legacy seed | floating garbage one major longer |
| the excluded young survivors themselves | left ALLOCATED until the next young collection | a young allocation right after the major still fails (the likely reading of the NativeString run: see the d4j page) |
| the concurrent cycle | `collect_young_to_old_roots` (every young object), or `CRATONVM_GEN_Y2O_LIVE_SEED` (live young, but every OLD object is live to it) | dead old data a dead old holder's young child names survives the cycle |

## Step 1 — free the excluded young survivors in the same pause (the NativeString gap)

After `old_gen_gc_inner` returns with a `TrueRootYoung`, every grid object
not in `visited` is garbage by the seed's own argument (no root, no
finalizable, no watched weak-table entry, no stretch and no MARKED old object
reaches it). Two ways to reclaim them before the mutators resume:

- **(A) second young sweep in the same call** (`run_non_moving_young_cycle`,
  after the old sweep, only when `true_root_young_excluded > 0`): run
  `sweep_young_non_moving` again over the same `roots` (still valid: a
  true-root cycle promoted nothing, so no root moved). Its card scan no
  longer finds the freed old holders. Compose its `pointer_map` with the
  first's (the second may promote; the old sweep already ran, so its
  destinations need no old mark this pause). Cost: one more young mark on the
  requested majors that excluded something, i.e. near-OOM and `System.gc()`.
  Risk: every post-collection consumer sees one merged map, as the moving
  path's Phase 5 already arranges.
- **(B) free them directly** from the true-root trace's complement. Cheaper,
  but every post-collection verdict keyed on the young sweep's survivor set
  (`GenerationalHeap::is_live_young_survivor`, the watched identity entries,
  the `addr_keyed` weak-row sweeps' `in_place` verdict, the age table, the
  free-list accounting) would have to learn the second verdict in the same
  pause. Not recommended.

Owner: the young-copy lane (`sweep_young_non_moving`) with the old-gen lane
(`run_non_moving_young_cycle`). Gate: `CRATONVM_GC_OVERHEAD_PROGRESS=0 ...
GenR4W4NativeStringOomProbe -Xmx64m` prints HotSpot's four lines 5/5, and the
OOME battery (`GenR4W4HeapFullThrashProbe`, `GenR4W5ThreadsOomProbe`) is no
worse. Then lane j's second major can be reconsidered.

## Step 2 — promoting cycles

The fallback exists because, during the pause, the side tables and the
finalizer list still name the PRE-promotion young addresses (the VM remaps
them after the collector returns), and the promotion destinations are seeded
as unconditional roots (they were proved live by the young phase, which
treats every old object as live). To keep the true-root verdict on such a
cycle:

1. build a sorted `(source, destination, size)` table from the sweep's
   `evac_map` (sizes from the destination headers);
2. in `TrueRootYoung::seed_word` / `follow`, resolve a young address that
   lies in a promoted source extent to the DESTINATION (base plus the same
   offset) and push that to the old mark instead of seeding a young object;
3. for a promoted object reached this way, also run `side_table_targets`
   with the SOURCE address as the owner key (`mirror_pin`, `metadata_pin`
   rows are keyed by it until the remap; `external_roots` were already
   remapped at the young-phase commit);
4. seed the finalizer candidates and the watched set through the same
   resolution (they are pre-promotion addresses);
5. stop appending the destinations to `root_shadow` on such a cycle.

`[promo-seed]` (`CRATONVM_DBG_PROMO_SEED=1`) says how often this arm is hit;
measure first.

## Step 3 — the concurrent cycle (the question the d5/r brief asked)

Can the concurrent cycle reuse the true-root seed? **Only at the remark, and
not as-is.**

- At the initial mark no old object is marked yet, so "the young targets of
  MARKED old objects" is empty: seeding young from the true roots alone
  there would drop every snapshot edge old -> young -> old whose old holder
  is marked LATER. The SATB snapshot needs them, so the initial mark has to
  keep a superset (today's all-young walk, or `CRATONVM_GEN_Y2O_LIVE_SEED`'s
  "every old object is live").
- Young objects move between the pauses (every young collection copies or
  promotes), so no young address survives from the initial mark to the
  remark: the fixed point cannot be carried across the cycle, only computed
  inside the remark pause.
- **What the remark could do:** replace `CRATONVM_GEN_Y2O_LIVE_SEED`'s
  "every old object's reference slots" old -> young seed by a fixed point
  over the MARKED old objects, iterated with the remark's final drain: drain;
  collect the young targets of the old objects marked since the last round;
  trace them with `scan_young_object`'s edges; push their old referents;
  repeat until both sides are dry. That is `TrueRootYoung::round` with the
  concurrent marker's mark bits instead of `GC_FLAG_MARKED`.
- **The hazard that blocks it today:** objects PROMOTED between the pauses
  are "ineligible = live" to the sweep but are never TRACED by the concurrent
  marker. The all-young initial-mark seed is what keeps their old referents
  (anything an old object reached through young at the snapshot was seeded).
  With a remark-only fixed point, a young object Y reached at the snapshot
  only through an old holder, promoted before the remark, is live to the
  sweep but never traced, and the old object only Y names is found unmarked
  and swept under it: a use-after-free. So the remark fixed point
  must also scan every object promoted since the initial mark (the promotion
  log the young collections already produce, `evac_map` destinations inside
  the cycle's window), which is the same cost the live seed pays today.

Recommended order: Step 1 (it is what the OOME probes need), then measure how
often Step 2's arm is hit, and only then Step 3 behind its own opt-in,
gated on `GenR5W5ConcUnloadProbe` / `GenR5W5RemarkRefsProbe` with the four
conc-unload switches and the SATB gc-stress set.

## Merged from `gengc-r5w6-conc10-proposal-integrated-young-trace-for-old-collections` (d8/y, 2026-09-28)

Retired: gcd d4/n built its design for requested majors (`TrueRootYoung`), and
its remaining cases are this page's steps. One idea of it is not above and is
kept here: the concurrent cycle's live-set seed
(`collect_young_to_old_seeds`' live path under `CRATONVM_GEN_Y2O_LIVE_SEED`)
scans every old object's reference slots, O(old) per concurrent pause. A
non-consuming read of the dirty cards (`CardTable::dirty_card_indices` plus
the thread-local card buffers) would make it O(dirty), with the same trust
the young collector already places in the card table. Worth doing with the
four-switch flip; measure the initial-mark and remark pause lines
(`door=gen-initial-mark`, `door=gen-remark`) before and after.
