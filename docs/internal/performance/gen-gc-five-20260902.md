# The generational minor pause, part two: five findings, and the one the first fix exposed

Slug: `gen-gc-five` · 2026-09-02
Follows `gen-gc-minor-pause-20260902.md`, which left the copy as the largest
phase and named parallel evacuation as "explicitly out of scope here". This
page takes the five ranked findings of the review that followed it, on the
MOVING young collection of `-XX:+UseGenerationalGC`.

---

## VERDICT

1. **The young trigger was never consulted on compiled code.** The refill-time
   gate demanded 65,536 slow-path entries since the last GC; healthy TLAB flow
   makes a few hundred per semi-space. Every collection ran at allocation
   failure and the pause-goal loop moved a number nothing read. Fixed with a
   second, bytes-based re-arm metric. `young_bytes_before` now sits at the
   trigger rather than at capacity.
2. **The from-space memset left the pause.** `cardclear+young_reset` was
   18–33 ms of a ~105 ms steady-state pause; it is 0–1 ms with the wipe on a
   helper thread. `CRATONVM_GC_SYNC_YOUNG_WIPE=1` brings it back.
3. **Evacuation is parallel** (`gc/src/gen_evac.rs`). On ONE worker the new
   engine copies an object in ~115 ns where the sequential drain took
   300–400 ns; with eight it was at first SLOWER than one, because a
   depth-first walk never reached the spill threshold and one worker did the
   whole closure while seven waited. Work is now donated whenever a peer is
   idle. `CRATONVM_GC_PAR_EVAC=0` restores the old drain.
4. **Promotion goes through per-worker buffers** carved unzeroed from old
   gen, 16 KiB doubling to 256 KiB, tails returned without stamping the
   reclaim epoch. Part of the per-object gain in 3.
5. **The pointer map is sharded and built in parallel.** With the copy spread
   over eight workers the single-threaded fold of their pair lists was the
   largest sequential term left: `map_merge` 15–25 ms of ~105 ms. Sixteen
   shards, each thread folding a disjoint slice.

And the residual the first fix exposed: with the trigger reachable, the
pause-goal loop halved the nursery 136 → 67 → 33 → 19 MB across consecutive
tenure cycles whose pause is the fixed live set, and a probe run took 88
collections where 14 would do. A halving is now a trial judged by the pause
it produces (`next_young_trigger`).

---

## Method, and what it is not

Host: the 32-core Windows box, shared with other sessions' builds and suites
for the whole period. Every number below is a ratio within one run, an exact
counter, or an interleaved same-binary A/B; absolute milliseconds are this
host's, not the collector's. Probes: `bench/OldGenRsetProbe 19 700 16` at
`-Xmx1g` (a large tenured tree, no old→young stores; the pause-shape probe)
and `bench/OldToYoungEdgeProbe 40000 200` at `-Xmx320m` (the store-heavy
companion, run under `CRATONVM_GC_VERIFY_RSET=1`).

The r1 binary (engine, wipe, trigger; before the feedback fix and the
donation fix) was measured first, because its numbers are what decided the
shape of the last two changes; the final binary is measured in §7.

---

## 1. The trigger nobody could reach

`tlab_alloc_object_inner` consulted `needs_gc_for_jit_allocation()` only
once `TLAB_SLOWPATH_ENTRIES_SINCE_GC >= 65_536`. That count was chosen for
the degraded modes the guard was written for — the crumb wedge enters the
slow path tens of thousands of times per second — and a bytes stamp was
rejected there because the per-object path never bumps one. Both were right
about their own mode. On healthy TLAB flow a 256 MiB semi-space is exhausted
in 300–1000 refills, so the gate never opened, and the 09-02 page's sweep of
`CRATONVM_GC_YOUNG_TRIGGER_PERCENT` at 50/75/90 % returned identical
collection counts with `young_bytes_before == capacity` on every line.

Two re-arm metrics, OR-ed: the entry count for the wedge, and
`TLAB_REFILL_BYTES_SINCE_GC >= 4 MiB` for healthy flow. `needs_gc` keeps its
own anti-livelock floor, so consulting it every few TLABs cannot storm a
young gen whose live set sits above the threshold. Engagement: the r1 A/B's
default arm collects at `young_bytes_before` 101–137 MB against a 268 MB
semi-space, i.e. at the 50 % trigger, where the 09-02 page saw 268 MB.

## 2. The wipe, off the pause

`Arena::reset` zeroed `[0, cursor)` and the high region inside the pause —
up to the semi-space capacity per cycle — and every one of those bytes is
zeroed again before an object lands on it (`refill_tlab` zeroes each TLAB,
`OldGen::alloc` zeroes each block). The documented blocker was that
`is_object_address` accepts any aligned address in either semi-space, so a
stale header in a reset arena could become a false root.

`Arena::reset_deferring_zero` does the metadata reset in the pause and hands
back the extents; at the end of the cycle — after a possible `grow`, which
can move the backing — `deferred_wipe_spans` turns them into committed
absolute spans and `spawn_evacuated_wipe` zeroes them on a named thread.
The arena is the next cycle's to-space, which no mutator allocates into;
the thread is joined at the top of the next collection and by `Drop`, and
`is_object_address` declines the inactive semi-space while
`wipe_in_flight` is set. `wipe_deferred_bytes` is the engagement counter.

r1 A/B, steady-state phase `cardclear+young_reset`: default 0–1 ms,
`CRATONVM_GC_SYNC_YOUNG_WIPE=1` 18–33 ms, on pauses of 100–140 ms.

## 3. Parallel evacuation, and why eight workers were slower than one

`gen_evac.rs` drains the transitive closure on `young_gc_threads()` workers,
each owning a to-space chunk (256 KiB, shrinking as the arena fills so
`threads` idle tails cannot exhaust a to-space sized exactly for its
survivors), an old-gen promotion buffer, its own `(from, to)` pair list,
deferred-card list and counters. A source object is claimed by a CAS on its
mark word after the copy — the ordering `ObjectHeader::make_forwarded`
documents — and a loser gives its copy back by retreating its buffer cursor
or stamping a dead filler over it. Chunk tails become the TLAB filler
object, so every linear walker still parses to-space end to end; chunk
starts are allocator anchors, so the next cycle's parallel object-start
walk has its grid.

Per object, r1 A/B (`evac_drain` / `objects_copied`, steady state):

| arm | ns per copied object |
|---|---:|
| `CRATONVM_GC_PAR_EVAC=0` (the old drain, incl. its map inserts) | 300–400 |
| new engine, `CRATONVM_GC_PAR_THREADS=1` | 115–120 |
| new engine, 8 workers, r1 | **170–185** |

The last row is the finding. A depth-first walk of a binary tree pops one
object and pushes two, so a worker's local stack sits at the tree's depth —
about twenty entries — and never reached `SPILL_HIGH = 1024`. Worker 0 did
the whole closure while seven waited on the condvar, and paid their
wake-ups. The fix is donation rather than spilling: whenever another worker
is idle (an atomic mirror of the idle count, one relaxed load per scanned
object) and this one holds more than one object, it hands over the OLDEST
half of its stack — in a depth-first walk those are the widest subtrees.
`evac_max_worker_pct` reports the largest worker's share of the copies; 100
is the r1 shape.

`evac_lost_races` is 0 on both probes, because a tree references each node
once. The race path is exercised by
`parallel_evacuation_survives_forwarding_races_on_shared_targets` (8191
references to 64 shared objects across 4 workers).

## 4. Promotion buffers

Every promotion used to be `OldGen::alloc`: a walk up the size buckets, a
best-fit scan inside one, the split remainder re-pushed, the sorted
free-list cache invalidated, and a `memset` of the block that the copy then
overwrote. A worker now carves a buffer with `OldGen::alloc_unzeroed`
(16 KiB, doubling to 256 KiB while the worker keeps promoting) and bumps
promoted objects out of it. A buffer never ends 8 bytes short — `Lab::alloc`
refuses the allocation that would leave that remainder, because 8 bytes is
below the free list's minimum block — and its tail goes back through
`OldGen::release_unused_tail`, which does NOT stamp `reclaim_epoch`: the
tail never held an object, so no concurrent-mark remark snapshot can name an
address in it, and a young cycle retiring its buffers therefore does not
invalidate an in-flight old-gen sweep. `evac_plabs` counts them (181 on the
tenure cycle of the debug probe run).

## 5. The map

`pointer_map` takes one entry per survivor and is read by ~160 `get` sites
after the pause, so the 09-02 page's rejection of a sorted vector stands.
What changed is that with the copy on eight workers, folding their pair
lists into one `FxHashMap` on the collector thread was the largest
sequential term left: `map_merge` 15–25 ms of a ~105 ms pause for 320–390k
survivors, ~60 ns per entry, cache misses on a table that does not fit L2.

`cratonvm_types::PointerMap` is now a struct of 16 `FxHashMap` shards
selected by `(addr >> 3) & 15` — adjacent objects land in different shards
— with the `HashMap` surface the 76 files naming it use, and
`par_extend_pairs(sources, threads)`: each thread owns a disjoint slice of
the shards and scans every source, inserting the pairs whose shard it owns,
so the build costs `N / threads` inserts of wall time. A lookup is the old
probe plus a shift and a mask. The five native helpers that were generic
over `&HashMap<usize, usize, S>` now take the map by name.

## 6. The residual the trigger exposed: a halving is a trial

With the trigger live, `adapt_young_trigger_to_pause` did what the 09-02
page predicted. On the debug probe run the tenure cycles copy 1.2–1.3M
objects and their pause is that copy; the loop halved the trigger 136 → 67 →
33 → 19 MB across consecutive cycles without the pause moving, and the run
took 88 collections where 14 would do. On the r1 release A/B the same loop
is why the `PAR_EVAC=0` arm ran 203–219 collections in two of three rounds
against the default arm's 29–40: its pauses were over the goal, so its
nursery was shrunk to 19–37 MB, and every extra collection cost a full
copy of the live set.

`next_young_trigger` is pure and tested: a halving opens a trial; the next
over-goal pause judges it, and one that did not fall by a quarter reverts
the halving and latches on that survivor volume, so the loop leaves the
trigger alone until the volume changes by a factor of two — the signal
that the live set, not the nursery, was the pause.

## 7. The final binary

<!-- filled in from the r3 interleaved A/B -->

## Correctness

* 1,762 gc unit tests, 8 new: the multi-worker tree, the shared-target race,
  the async and sync wipes, the tail release, the buffer tail rule, filler
  sizing, and the feedback sequence; 18 gc integration suites; 2,656 vm unit
  tests; 597 types tests including the sharded map's equivalence to the
  sequential fold at 1/2/3/8/16/64 threads.
* `bench/OldToYoungEdgeProbe 20000 200` at `-Xmx128m` under
  `CRATONVM_GC_VERIFY_RSET=1`: 57 collections, every one with edges reports
  `edges=20000 missing=0`, 16 of them through the new engine
  (`edges_verified=20000`).
* The regression suite on the release binary — see §7.

## Not established, and what is owed

* **`RMapGcStress` timed out at 900 s in the DEBUG-binary suite run.** It is
  not a hang: on the release binary it passes in 114 s (ZGC, 6 collections)
  and 122 s (Generational, 2 minors), and its cost is 378k native-collection
  checks, not collection. A debug binary is an order of magnitude slower on
  that shape. Read the suite on the release binary.
* Absolute pause numbers are this host's under load — see Method.
* `OldToYoungEdgeProbe` shows the item-7 shape the review named and this
  page does not touch: with dirty cards near the end of a large old gen,
  `scan_dirty_cards` is O(old objects) (137–161 ms spikes in the r1 A/B's
  `PAR_EVAC=0` arm, whose nursery shrink promoted more). A block-offset
  table is its own change.
* `evac_filler_bytes` — to-space wasted to chunk tails — is 1–2 MB per cycle
  with 8 workers on a 268 MB semi-space and counts toward `bytes_copied`.
  An adaptive chunk size would remove most of it; not done here.
