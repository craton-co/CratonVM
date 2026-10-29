# G1: young evacuation copies about 3x slower per byte than HotSpot, and `KeepLong` runs about 7x slower

> **STATUS (2026-10-29, g1ep round 7, branch `g1ep7` merged with dev `36caad6ecc`): OPEN.**
> The ZipFs row's finalizer half and its OOM are fixed; the KeepRounds close bar (at most 2x HotSpot's
> wall) is met only at the lower loads this shared 16-core host gave. Host 64.177.43.150, frame-pointer
> release builds, `G1epKeepRounds 10 -Xmx1g`.
>
> **Landed** (each behind a default-ON switch, `=0` restores the old path):
>
> - **Root regions** (`CRATONVM_G1_ROOT_REGIONS`, `gc/src/g1_root_regions.rs`). Round 6's lead. At the
>   initial mark the young regions get TAMS at bottom and their objects are scanned once, on the
>   evacuation pool, as HotSpot's `G1CMRootMemRegions`; the cycle then never marks, traces or
>   keeps alive a young object (SATB drains, the marker's pop, `scan_object_refs`, remark's push all
>   filter young). Declines (falls back to the old cycle start) on an unparsable young region or a
>   reserved TLAB tail. User CPU 37-41 s -> 26-29 s.
> - **The in-place full marks by bitmap** (`CRATONVM_G1_IN_PLACE_MARK_BITMAP`,
>   `gc/src/g1_compact_in_place.rs`): movable regions are no longer parsed before the mark; the mark
>   claims 8-byte granule bits and the live table is built from them afterwards
>   (`live_table_from_marks`, refusing an interior mark). Parse per full 139 ms -> 33 ms.
> - **Parallel forward-map compose** (`PointerMap::compose_after_par`), **parallel remark drain**
>   (`CRATONVM_G1_PARALLEL_REMARK_DRAIN`, `drain_marking_to_fixed_point`), **parallel Phase 3.5
>   resurrection** (`CRATONVM_G1_PARALLEL_RESURRECT`, `resurrect_dead_finalizers_parallel`: a second
>   `parallel_evacuate` seeded by the dead finalizable objects).
> - **Take-over helper-window pass pre-signals its blocked peers** (`CRATONVM_XT_HELPER_PRESIGNAL`,
>   `vm/src/jit/xt_root_scan.rs`) so a pause waits for the slowest peer instead of the sum, and the
>   word probe screens words by the heap envelope first.
> - **The mixed phase is bounded at `ceil(candidates / min_old)` pauses** (`CRATONVM_G1_MIXED_PHASE_BOUND`,
>   `mixed_choice::mixed_phase_pause_bound`), HotSpot's at most `G1MixedGCCountTarget`. The budget was
>   one pause per candidate on the premise that each pause frees one; a candidate an evacuation
>   failure retains is never freed, so the phase ran ~100 pauses on stale mark data while no new mark
>   could start. That was KeepRounds' slow mode (100+ "mixed" pauses that freed no old region) and the
>   ZipFs OOM below. Also: **a mixed pause whose every candidate left is pinned ends the phase**
>   (`end_mixed_phase_if_only_pinned_left`; `[GC] g1 mixed-live: ... ended_pinned=`).
>
> **ZipFs row** (`ZipFsFinalizerChurn <300 jars> 60 -Xmx512m`): dev 366 s with 544 young pauses and
> 128 s of pauses; the branch 93-134 s, 15-25 s of pauses, stdout identical to HotSpot on 4 of 4 runs.
> Before the phase bound, rr6/rr9/m1 threw `OutOfMemoryError` at round ~20 in 7 of 9 runs (one
> concurrent cycle, then the endless mixed phase). HotSpot runs it in 15.6-23 s; the remainder is
> mutator/JIT time, which is
> [`javac-zipfs-typecheck-sites-and-instance-clone-leave-compiled-code-20261027`](../jit/javac-zipfs-typecheck-sites-and-instance-clone-leave-compiled-code-20261027.md) territory, plus the Phase-4 fix-up re-walking the
> remembered-set sources the seed phase walked.
>
> **Measured, KeepRounds** (interleaved, `fact chains_ok=true` on every run):
>
> | load | CratonVM wall | HotSpot wall | ratio | CratonVM user | HotSpot user |
> |---|---|---|---|---|---|
> | 16-20 (rr9) | 9.0-9.8 s | 4.8-4.9 s | 1.9-2.0x | 26-28 s | 18-19 s |
> | 16-17 (m1, merged) | 10.5-11.1 s | 4.3-5.2 s | 2.1-2.4x | 29-32 s | 19 s |
> | 25 (m5) | 10.8-17.2 s | 6.0-7.5 s | 1.6-2.3x | 28-29 s | 18-19 s |
> | 18-19 (m6, merged tip) | 9.2-10.6 s | 4.4-5.0 s | 2.08-2.13x | 29-31 s | 18-19 s |
> | 30-40 (rr9) | 20-26 s | 8.4-9.7 s | 2.3-2.7x | 26-30 s | 18-19 s |
>
> CPU is now 1.5x HotSpot's (was 1.9x). The wall ratio grows with load because the main thread still
> carries ~3.5x HotSpot's serial work (take-over stack bands, the remembered-set source walk, the
> scrub); at load 30+ on 16 cores every serial millisecond is stretched. The deciding row at load
> under 8 had not come up when this was written (see below).
>
> **Tried and kept off:** `CRATONVM_G1_PARALLEL_SEED=1` (no change on KeepRounds; ZipFs inconclusive),
> `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE=1` with and without `CRATONVM_G1_OOME_RETRY_BEFORE_FRESH_CYCLE=1`
> (22.0/21.6 s against 22.4 s, noise), `CRATONVM_G1_CLEANUP_PARALLEL_SCRUB=1` (inconclusive at load 45-60).
>
> **Checked** (merged with dev `36caad6ecc`, m6): `cargo test -p cratonvm-gc --lib` 4345 passed, 0 failed;
> `cargo test -p cratonvm-types` all pass; vm `xt_root_scan` 78 passed; ZipFs 2/2 identical; OOM gate clean.
> Earlier, merged with `0e7a9a4b46`: gc integration
> tests: `g1_w7d_mark_cycle_lifecycle` fails only when run in parallel with its binary's other tests
> (process-global counter; 3 of 3 single-threaded passes), and three failures in code this branch does
> not touch (`g1_doc_blocks_are_not_stranded` on a dev doc block at `drop_post_block_peer_stack_words`,
> `gengc_r4w2_obs` maxMemory, `vm_infallible_allocations` JIT/FFM audit list); `cargo test -p cratonvm-vm
> --lib` 7122 passed, 4 failed in frame layout / lambda / fast-boot / mirror tests outside this
> branch; `cargo test -p cratonvm-types`: dev's three (`the_whole_surface_is_twenty_variables` and
> `the_surface_users_have_to_learn_is_twenty_names` count 22 scalars, `every_declared_flag_still_has_a_reader`
> on `CRATONVM_COMPAT_EAGER_*`). G1 OOM gate (`R22FlipGcRows -Xmx64m`) 0/12 OOM and 12/12 equal to
> HotSpot with the JIT and with `--nojit`.
>
> **Next:** the deciding row at load under 8; then the serial main-thread work: cache a parked peer's
> classified band across pauses while it stays blocked, and drop the Phase-4 re-walk of seed sources.

> **STATUS (2026-10-28, g1ep round 6, branch `gc-ki-g1evac-j1yg` off dev `3de9225a6c`): OPEN.**
> Round 5's lead found and fixed; the close bar (at most 2x HotSpot's wall) is still not met.
> Host 64.177.43.150, frame-pointer release builds, `G1epKeepRounds 10 -Xmx1g`.
>
> **Landed:**
>
> - **The JIT-side root behind the all-Mixed mode** (`gc/src/g1_compact_in_place.rs`). A held region
>   (one with a JIT pin) keeps in place only the objects its pins *name*, and a conservative word
>   names the object it points into *or ends at* (the `j-1` end rule: a derived pointer one past an
>   array). The root loops applied the end rule to shadow-stack precise roots too. A precise root at
>   the start of an object therefore also named the object just below it, and in the failing runs
>   that was round 0's `Node[64]` ring, ending exactly where a live object began. The first starved
>   full then kept all of round 0 (`freed=0`) and the run went all-Mixed. Now the end rule applies
>   only to words in the JIT pin snapshot (`pinned_jit_roots_snapshot`); a precise root names only
>   the object it starts. Test `a_precise_root_in_a_held_region_does_not_keep_the_dead_object_below_it`
>   (fails before the fix: the dead object stays). Dev was in the mode in 3 of 9 interleaved runs,
>   the fix in 0 of 9.
> - **The survivor cap follows HotSpot's young target** (`gc/src/g1.rs`,
>   `g1_young_target_within_free`). HotSpot's `_max_survivor_regions` is
>   `ceil(T / SurvivorRatio)` with `T` bounded by the young regions already allocated plus the free
>   regions outside the `G1ReservePercent` reserve. Ours divided the unbounded desired target, so
>   on a full heap it armed a survivor cap that free space could not hold, and the pause promoted
>   instead. Test `g1ep6_the_young_target_is_bounded_by_free_regions_outside_the_reserve`. About
>   12 % less user CPU.
>
> **Where the rest of the gap is** (one run at low load, `-Xlog:gc` timestamps on both VMs):
> CratonVM 7.94 s wall = 5.02 s in pauses + 2.60 s between them; HotSpot 3.33 s = 1.89 + 1.31.
>
> - Young pauses: closure 2.36 s for 8.1 GB copied; `other_us` 0.66 s, and a scratch split shows
>   it is nearly all concurrent-mark bookkeeping: keep-alive roots 0.18 s, re-graying their copies
>   0.17-0.21 s, the gray-set remap 0.24 s. Every allocation here overwrites a ring slot, so the
>   SATB log carries about one pre-snapshot young object per allocation, and each pause of the
>   cycle carries, re-grays and remaps them. HotSpot drops every young SATB entry (young regions'
>   TAMS is bottom) because it scans the concurrent start's survivor regions as root regions; this
>   collector does not (see `marking_keepalive_roots_split`'s "ONLY IN EDEN" note). **Next lead:
>   root-region scanning at concurrent start, then the full young SATB filter.**
> - In-place fulls: 1.38 s (parse 215, mark 538, plan 130, map 58, copy 226, adjust 207 ms) against
>   HotSpot's 0.4-0.8 s.
> - Mutator: about 0.78 s CPU of TLAB-carve zeroing; the main thread waits 2.2 s runnable behind
>   the evacuation workers inside pauses.
>
> **Measured** (6 interleaved triples, load 13-17; no run of either CratonVM binary was in the
> mode): the branch 7.2-8.8 s wall / 31.3-33.9 s user, dev 6.3-8.8 s / 33.1-37.8 s, HotSpot
> 2.6-3.6 s / 17.4-18.1 s, `fact chains_ok=true` on every run. So about 2.3-2.6x HotSpot's wall
> and 1.8x its CPU; **the 2x bar is not met.** The mode fix shows up as the absence of 22-44 s runs
> (dev 3 of 9 in an earlier batch, the branch 0 of 9), not in this row's medians.
>
> **Checked:** `cargo test -p cratonvm-gc --lib` 4325 passed with all three commits;
> `g1::tests::a_live_compiled_frame_with_an_empty_publication_still_evacuates_by_default` and
> `an_unregistered_compiled_frame_also_makes_an_empty_publication_unsafe` fail their *precondition*
> (`pinned_jit_root_count() == 0`) in some full runs, because other g1 tests publish into the
> process-wide pin map at the same time; each passes alone. Dev `3de9225a6c`'s own test binary fails the same two in 4 of 4 full runs (the branch: 3 of 5), so this is a pre-existing test-isolation race, not this branch. The G1 OOM gate
> (`R22FlipGcRows -Xmx64m`) 0/12 OOM and 12/12 equal to HotSpot with the JIT and with `--nojit`.
> `cargo test -p cratonvm-types`: one failure, `every_cratonvm_literal_is_declared_or_explicitly_exempt`,
> on eight `CRATONVM_COMPAT_EAGER_*` / `CRATONVM_COMPAT_FULL_PHASE1` / `CRATONVM_JIMAGE_INDEX_CACHE`
> names this branch does not touch (dev's).

> **STATUS (2026-10-27, g1ep round 5, branch `g1ep5` off dev `12c7637fa2`): OPEN.** Round 4's lead
> taken: the runs that take 48-109 Mixed pauses. Host 64.177.43.150, frame-pointer release builds,
> `G1epKeepRounds 10 -Xmx1g`.
>
> **What the mode is.** Dev's binary entered it in 3 of 10 interleaved runs (plus 1 of 6 and 2 of 10
> in earlier batches): 109 Mixed pauses and 18 young, 44 s wall, 36-37 s of it in pauses (41-50
> Mixed: 22-29 s). The chain:
>
> 1. With the JIT, about one first starved full in four reclaims **nothing**: `[g1][IN-PLACE] ...
>    moved=35-45 MB freed=0`, old 938 MB in 943 regions before and after. Round 1 has promoted about
>    300 MB by then, so round 0's 634 MB is dead to the program but still reached. In good runs the
>    same full at the same point frees 652 MB (old 936 -> 315 MB). Under `--nojit` 9 of 9 runs
>    freed round 0 there (652.8 MB each time), so a JIT-side root keeps the previous round's `ring`
>    alive into the next round. **Not found yet; the next round's lead.**
> 2. The heap is left full (1024/1024 Old); young pauses copy almost nothing until the next marking
>    cycle's cleanup frees round 0 (508 regions). Cleanup opens a mixed phase with one pause per
>    candidate (128), each later cycle's cleanup reopens it before it closes, and no starved full
>    comes again, so every pause from there on is Mixed.
> 3. Mixed pauses were always serial (`CRATONVM_G1_PARALLEL_MIXED`, opt-in), and each one carries
>    the whole young generation: about 270 young regions per pause. One copied 279 MB in 1.66 s of
>    closure on the driver thread alone; a young pause copies 322 MB in 64 ms.
>
> **Landed:** mixed pauses take the parallel evacuator by default (`CRATONVM_G1_PARALLEL_MIXED`
> default ON, `=0` is the serial route; a `System.gc()` or allocation-failure compaction plan stays
> serial). The opt-in rested on a measurement that the old half of a mixed pause is copied in Phase 2
> by the driver alone; that holds, but here the young half is the pause. Also fixed on the way: the
> parallel mixed body returned early on an empty CSet without spending the phase's pause count (the
> serial body has no early return), so a phase of empty pauses never closed; surfaced by
> `mixed_gc_remaining_decrements` once the route flipped.
>
> **Measured** (interleaved on one binary first, 10 pairs, load 12-27): the parallel route's one run
> in the mode spent 3.5 s in pauses and 9.5 s of wall, against 36-37 s and 44 s serial; runs outside
> the mode matched. Then dev's binary against the fix, 10 pairs, load 11-22: dev in the mode 3
> times (44.0, 22.5, 28.6 s), the fix in none; user CPU 35.3-41.4 s on the fix against 35.7-49.6 s.
> Final binary at load 12-16: 7.2-11.5 s wall / 36.0-40.2 s user, pauses 4.3-6.8 s;
> HotSpot 3.0-4.9 s / 17.0-18.9 s. So still about 2.4x HotSpot outside the mode; the mode is now
> about as cheap as a normal run.
>
> **Checked:** `cargo test -p cratonvm-gc --lib` 4309 passed (new:
> `mixed_gc_remaining_decrements_on_the_parallel_body_too`); `cargo test -p cratonvm-gc --tests`
> all G1 tests pass, and the two failures (`gengc_r4w2_obs` `maxMemory`,
> `vm_infallible_allocations`'s audit list naming `ffm_downcall_direct_call`) are gen-GC and VM
> source audits this change does not touch; `cargo test -p cratonvm-types` 777 passed; the G1 OOM
> gate (`R22FlipGcRows -Xmx64m`) 0/12 OOM and 12/12 equal to HotSpot with the JIT and with
> `--nojit`; `SteadyChurnRecreation -Xmx16m --nojit` (the parallel race note's shape) at
> `CRATONVM_G1_WORKERS` 1/2/8/16 and `G1OldBurstProbe -Xmx128m`, checksums equal to HotSpot in
> every run; `fact chains_ok=true` on every row run; the route census reads `serial=0 parallel=9`.
>
> Merged with `dev` `ab54dabd75` and re-checked: gc lib 4313 passed, types 781 passed, the gate 0/12
> OOM and 12/12 equal to HotSpot in both modes, four row runs 7.5-8.1 s wall / 35.9-37.3 s user.

> **STATUS (2026-10-27, g1ep round 4, branch `g1ep4` off dev `e8021d716c`): OPEN.** Round 3's lead
> taken: the scratch buffers that faulted on every pause. Host 64.177.43.150, frame-pointer release
> builds, `G1epKeepRounds 10 -Xmx1g`, interleaved, six clean pairs at load 4-11 (a seventh had a load
> spike): CratonVM 6.7-8.6 s wall / 33.5-38.8 s user / 2.3-3.7 s system against dev's 7.4-8.9 s /
> 36.4-41.1 s / 4.1-4.7 s; pauses 3.8-4.7 s against 4.0-4.8 s; HotSpot 2.7-3.1 s / 17.6-18.4 s /
> 0.6 s. About 0.8 s of wall less, still about 2.4x HotSpot. Page faults per run (`/usr/bin/time %R`)
> 0.71-0.72 M against 1.15-1.38 M (HotSpot: 0.28 M, its heap's first touch); peak RSS 1.50-1.51 GB
> against 1.48-1.49 GB (HotSpot 1.13 GB).
>
> **Landed:**
>
> - `PointerMap` has an optional `SortedBufferPool` (`types/src/pointer_map.rs`): a sorted map built
>   from one hands its pair vector and bucket index back when it is dropped or thawed, and
>   `compose_after` takes its three vectors from it. G1 keeps one per collector, at most a sixteenth
>   of the reserved heap of each kind (`CRATONVM_G1_SORTED_BUFFER_POOL`, default ON, `=0` kills);
>   the young pause's bucket sort scatters into a kept vector, and so does the in-place compaction's
>   map. The VM drops a pause's map when the next pause publishes its own, so two buffers alternate.
>   The scatter's faults 168 k -> 25 k per run, `compose_after`'s and the index's about 93 k -> 0.
> - The in-place parse's per-region tables and each plan chunk's pair list are allocated at their
>   upper bound (a region's used bytes over the header size; a chunk's live objects) instead of grown
>   from empty, so they never grow by copy. Past the 64 KiB arena cap the unused tail of such a
>   mapping is never touched, so never faulted in. Small: the parse's faults 134 k -> 116 k.
> - The JIT's helper-window pass keeps its 8 MiB copy band per thread between passes
>   (`xt_root_scan::imp::take_pass_band`); every pause used to map a fresh one and fault in the
>   pages its deepest peer copy wrote (0-208 k faults per run, varying with where peers park). Every
>   reader of the band already reads only the words a copy wrote.
>
> Tried and not kept: pooling the parse tables and the plan's pair lists as well (in the forward-list
> pool). Faults went to 0.59-0.78 M but peak RSS to 1.71-1.74 GB, a quarter of a gigabyte for about
> another 0.3 s of system CPU.
>
> Left (faults per run, `perf record -e page-faults`): the in-place parse tables (116 k) and plan
> lists (91 k) are first touches of tables about 50 MB per full collection, which only holding them
> would avoid; the rest is the heap's own first touch (TLAB carves 112 k, evacuation copies 134 k +
> 55 k), which HotSpot pays too.
>
> Checked: `cargo test -p cratonvm-gc --lib` 4295 passed; `cargo test -p cratonvm-vm --lib
> xt_root_scan` 76 passed; `cargo test -p cratonvm-types` (new: a
> pooled map reuses its buffers, stale contents and all, and answers as the unpooled one, composed and
> thawed; the bucket sort's test builds through a pool) -- the lib 730 passed, and the four failing
> integration tests are dev's own (undeclared `CRATONVM_BUILD_*` and `COMPAT_FAST_BOOT_CLASSES`,
> `DEFINE_BOOT_MAPS_AT_LINK`'s off word, `late_bind_osr_optimizing_enabled`'s doc, three jr21 records
> without a verification note); the G1 OOM gate (`R22FlipGcRows -Xmx64m`) 0/12 OOM and 12/12 equal to
> HotSpot with the JIT and with `--nojit`; `fact chains_ok=true` on every row run.
>
> Merged with `dev` `486c275b17` and re-checked: gc lib 4305 passed, `pointer_map` 13, `xt_root_scan`
> 76, the gate 0/12 OOM and 12/12 equal to HotSpot in both modes. Alternated with dev's own binary
> (load 7-12): faults 0.71-0.74 M against 1.15-1.40 M, system 2.7-3.2 s against 3.9-5.0 s. **New on
> dev, not from this round:** some runs now take 48-51 Mixed pauses instead of 18 (17-21 s wall,
> 1.66 GB peak RSS, `chains_ok=true`) -- dev's binary in 1 of 4 runs, the merged one in 2 of 7. The
> other runs are as before. Not investigated here; the next round should start from it.

> **STATUS (2026-10-27, g1ep round 3, branch `g1ep3` off dev `6bb160f1b9`): OPEN.** The merged tree
> (g1sz + g1ep) measured again on host 64.177.43.150 at load 14-44, frame-pointer release builds,
> interleaved: `G1epKeepRounds 10 -Xmx1g` CratonVM 9.3-17.8 s wall / 34.6-41.2 s user CPU, HotSpot
> 3.3-7.6 s / 18.1-19.5 s, so about 2.8-3x wall and 2x CPU; at this load a single run's wall moves
> 30 %. HotSpot's `-Xlog:gc` in the same minutes: 67 young pauses in 1.69 s and 9 `Pause Full` in
> 0.38 s (42 ms each); CratonVM: 74-75 young pauses in 4.2-7.8 s and 9 in-place fulls in 1.6-2.6 s.
>
> **Landed** (no switch: each builds the same map and the same plan; unit tests compare them):
>
> - `sorted_pointer_map_by_region` reads several source slices where they lie. The young pause no
>   longer appends every worker's forward list to the driver's (79-149 ms per run, serial), and the
>   in-place compaction no longer flattens its plan chunks into one vector to test their order
>   (194-219 ms per run, serial; all nine fulls of this row lift objects, so the test always failed).
> - The sorted map's bucket index is filled on the pool (`PointerMap::from_sorted_unique_par`); the
>   serial fill was 215-228 ms over the run's 84 map builds.
> - The in-place plan walks a region's live objects through the mark bits (`RegionObjs::live_indices`)
>   instead of asking every parsed object, and the lift's trial copies only the hole lists of the
>   regions that have holes, not one list per compaction region for each of the ~620 empty regions it
>   tries per pass.
>
> Over the nine fulls of a run: the map phase 405-628 ms -> 136-280 ms, the plan 444-581 ms ->
> 297-403 ms (four interleaved pairs, load 24-26). The young pauses' `pointer_map_us` did not move
> visibly (1.12-1.37 s against 1.00-1.42 s); its remaining cost is the scatter and the per-region sorts
> (diagnostic timers: 146-169 and 255-314 ms per run), both already parallel.
>
> Checked: `cargo test -p cratonvm-gc --lib` 4212 passed (new: the multi-source bucket sort against
> the concatenation's hashed fold), `cargo test -p cratonvm-types --lib pointer_map` 11 passed (new:
> the parallel index against the serial one, every job count), `fact chains_ok=true` on every row run.
> The G1 OOM gate (`R22FlipGcRows -Xmx64m`) at load 43-63: `--nojit` 0/12 OOM and 12/12 equal to
> HotSpot; JIT 1/12 OOM in the first gate (the `aastore-humongous` row) and 0/16 in sixteen more runs
> alternated with dev's binary, which read 0/12 and 0/16 -- 1/28 against 0/28, the same plans and maps.
> The tree merged with `dev` `97fd6da1c8` re-checked: gc lib 4266 passed, `pointer_map` 11 passed,
> the gate 0/12 OOM and 12/12 equal to HotSpot in both modes, the row `chains_ok=true` at 10.8 s wall
> against HotSpot's 4.5 s (load about 20), the full collections' map phase 71-75 ms per run. Merged
> again over `a3cd1ca886` (the fragmented-humongous Free-destination slide, which adds empty Free
> regions to the in-place set): gc lib 4268 passed, the gate 0/12 and 12/12 in both modes, the row
> `chains_ok=true`. At load 33-53 the row takes 82-84 young pauses instead of 74-75 on BOTH merges'
> binaries (27-32 s wall against HotSpot's 9.6 s): the pause count follows the host's timing.
>
> Tried and not kept (same binary, opt-in arms, three interleaved rounds, user CPU): HotSpot's
> destination prefetch (`prefetcht0` 576 bytes ahead of each copy) 36.3-40.8 s against 34.6-41.2 s
> off; holding a holder's collection-set slots back and evacuating them after the walk, last met first
> (so a chain node's `next` miss runs while its payload is copied) 37.6-39.5 s. Neither shows above
> the host's noise.
>
> **Found: the collector's large scratch buffers fault on every pause.** The launcher caps mimalloc's
> arena objects at 64 KiB (boot RSS, `CRATONVM_MIMALLOC_ARENA_MAX_OBJECT_KB`), so every larger buffer
> is its own `mmap`, faulted and zeroed on first write, and `munmap`ed when dropped. System CPU is
> 2.8-3.1 s with the cap and 1.7-2.0 s with `CRATONVM_MIMALLOC_ARENA_MAX_OBJECT_KB=0` (three
> interleaved pairs). Page-fault samples by the first VM frame: the map build's scatter into its fresh
> output (993), the in-place parse's per-region object tables (about 1200), the index and
> `compose_after`'s output (about 450), the JIT root scan's parked-slot snapshot (359); the rest are
> first touches of heap pages, which HotSpot pays too. Keeping those buffers mapped between pauses is
> an RSS trade (tens of MB on this heap), so it is left as the next step rather than taken here.
>
> What the profile still shows (`perf`, the merged tree): `evacuate_screened` 16 % (the claim CAS
> 12 %), the candidate screen 11 %, the concurrent marker's `try_mark` 4.7 % (its own thread), the
> layout `ScanCache` closures about 5.5 %, the map's sorts 3.4 %. The kernel is 9.5 % of samples.
>
> **STATUS (2026-10-27, g1sz): OPEN, narrowed.** `G1epKeepRounds 10 -Xmx1g` went from 7x HotSpot's wall
> (2026-10-24) to **2.4-3.0x** (CPU 1.8-2.0x), measured interleaved with HotSpot on the shared 16-core
> host; the spread is the host's load (section "Round g1sz"). The close bar below, 2x HotSpot's wall,
> is not met. The narrower milestone's second half holds: young pauses on a low Free pool run parallel
> (`serial=0` on every run) and their unattributed time fell instead of growing (`other_us` 1.3 s per
> run, against 4-6.7 s on the 2026-10-24 latch arm). This page no longer blocks
> `CRATONVM_G1_HEAP_RESIZE`: its soak passed and it is default ON
> ([`g1-default-sizing-never-collects-a-short-allocation-run`](../../internal/fixed-bugs/g1-default-sizing-never-collects-a-short-allocation-run-FIXED-20261026.md)).
> The g1ep round 2 below ran at the same time and landed much of the same parallel-path work (the
> latch flip, sorted maps on the full path, the pool's keep-alive seed, the start bitmap); the merged
> tree keeps one copy of each, g1ep's where both had one, plus g1sz's own items (the short-free switch,
> the inlined screens, batched provenance, the presorted in-place map).
>
> **STATUS (2026-10-27, g1ep round 2, branch `g1-evac-perf` off dev `ab197af426`): OPEN, narrowed.**
> `G1epKeepRounds 10 -XX:+UseG1GC -Xmx1g` user CPU went from 66-76 s (dev) to 37.8-39.8 s (HotSpot
> 17.9-19.6 s, the same host and minutes), wall from 78-90 s to 11.1-20.3 s (HotSpot 3.6-7.1 s at load
> 20-30). It is still about 3x HotSpot's wall, so the page stays open. Host 64.177.43.150, release
> builds without LTO, arms interleaved. What landed, each with its own kill switch:
>
> - **The latch flip** (`CRATONVM_G1_PARALLEL_UNDER_LOW_FREE_LATCH`, now default ON, `=0` kills). The
>   parallel driver no longer loses to the serial one (the sorted map of g1sz removed its hash fold), so
>   young pauses on a low Free pool run parallel: `parallel=70-76 serial=3-6` against `74/50` on dev.
>   `R23W6KeepLong 20` and `BinTreesClassic 18` are equal or better on it.
> - **Sorted maps on the starved-full path** (`CRATONVM_G1_FULL_POINTER_MAP_SORTED`, default ON). The
>   in-place compaction builds its map by the young pause's region bucket sort, and
>   `compose_forward_maps` merges two passes' maps as sorted lists (`PointerMap::compose_after`). The
>   old compose thawed the young pause's sorted map into hash shards and hashed every compaction forward
>   into it, on one thread: 8% of the run's CPU (item 1 below, on this path).
> - **The keep-alive seed runs on the pool** (`CRATONVM_G1_PARALLEL_KEEPALIVE_SEED`, default ON). Phase 1b
>   evacuated the SATB log's CSet residents on the driver alone; on this row that is about one object per
>   allocation. `roots_us` over the run fell from 1.26-1.72 s to 0.55-0.60 s.
> - Per-worker counters for the cap / overflow promotion census (a shared `fetch_add` per promoted copy,
>   28% of `evacuate_screened`'s samples). `compact_shape`'s memo hit is inlined (3.6% of CPU was call
>   overhead). The compact reference walk reads the oop-map (`ref_disps`) instead of zipping every field
>   with its `is_ref` (3.3%). The in-place mark finds an object by a start bitmap and ranks instead of a
>   binary search (4.9%). Forward lists are recycled between pauses (kernel page clearing was 1.8%).
>
> Checked: `cargo test -p cratonvm-types` (the five red integration tests are red on dev too: jit doc
> citations, compat fast-boot flags, `DEFINE_BOOT_MAPS_AT_LINK`, a `jit_bridge` doc, unverified jr21
> records), `cargo test -p cratonvm-gc --lib` 4208 passed, and the G1 OOM gate (`R22FlipGcRows -Xmx64m`,
> 12 runs JIT and 12 `--nojit`) 0/12 OOM and 12/12 output equal to HotSpot in both modes.
>
> **What is left** (`perf`, frame-pointer build of the round's last binary, `G1epKeepRounds 10`): the copy
> is a flat profile. `evacuate_screened` 8.9% (60% of it the forwarding CAS, the first touch of the
> object), `evacuation_candidate_screen_view` 8.0% (item 3), the provenance record of each new copy 3.0%
> (a `lock or` per first granule; bounded by the heap's granule count), `tlab_alloc` 2.8%, the concurrent
> marker about 7%. The 9 starved fulls take 1.7-2.4 s against HotSpot's 9 x ~95 ms. The young map build
> (`pointer_map_us`) is 1.3-1.5 s, about 15% of young pause time. On this shared host wall time follows
> total CPU, so the closing criterion needs about half the remaining CPU.
>
> Landed on dev as `1d033e22a1` (round commit `2e2aefddda`). The merged tree re-checked: gc lib 4210
> passed, the G1 gate 0/12 OOM and 12/12 equal in both modes, the row `chains_ok=true` at 11.6 s / 41 s CPU.
>
> Tried and NOT kept: prefetching every referent of a two-reference object before the compact walk
> visits it (HotSpot's queue-time prefetch, done at walk time). `G1epKeepRounds 10`, three interleaved
> reps against its own kill arm: user CPU 38.6-41.5 s on, 38.6-42.7 s off. No effect above this host's
> noise, so no switch was added.
>
> What would close the rest is structural, not another per-object trim (single changes under 5% do not
> show on this host): (a) the compaction's fix-up resolves every reference into the compaction set
> through the sorted map; a per-region "new address by object index" array beside the start bitmap
> would make it two array reads, and HotSpot adjusts from the header instead; (b) item 1's VM side --
> a pause could hand the VM a map it needs only for roots if the VM's remaps read forwards from
> from-space headers, which stay intact until the free phase; (c) item 3, the candidate screen (8%),
> once the defects it guards against are known closed.
>
> **STATUS (2026-10-24, g1ep): OPEN.** The residual of
> [`r23-w6-g1-evacuation-copy-throughput`](../../internal/gc/r23-w6-g1-evacuation-copy-throughput-SUPERSEDED-20261024.md).
> It is a throughput gap only: the output is identical (`fact chains_ok=true` on both VMs).

## Round g1sz (2026-10-26/27)

Host 64.177.43.150 (16 cores, shared; load average per row below). Fat-LTO builds of branch g1sz; HotSpot
25.0.4 with `-XX:+UseG1GC -Xmx1g`. Interleaved runs, median of 3 per arm (`kabn.sh`):

| binary | wall CratonVM / HotSpot | CPU CratonVM / HotSpot | young pause sum | full pause sum | serial young pauses |
|---|---|---|---|---|---|
| s5 (round start) | 3.11x | 1.98x | 17.2 s (HotSpot 5.7) | 5.0 s (0.9) | 4 |
| s6 (the final defaults, before the presorted in-place map) | 2.42x | 1.83x | 12.3 s (5.7) | 3.8 s (0.9) | 0 |
| s7 (the final branch, `169c240672`), host load 3.5 | **2.44x** (7.0 s against 2.9 s) | 2.05x | 3.0 s (1.1) | 0.97 s (0.22) | 0 |
| s9 (the branch merged with `dev`, `00832538c5`), load 2.4 | 2.59x; **2.39x** with `CRATONVM_G1_PINS_PEER_STACK_WORDS=0` (s7 alongside: 2.39x) | 1.98x | 2.8 s (1.1) | 1.27 s (0.22); 1.06 s with the pins off | 0 |
| g1pin p6 (object pins, `3772f3d40e`), load 16-25 | 2.67x; 2.52x with the pins off | 1.86x; 1.76x | 7.6 s (3.3); 7.4 s | 3.3 s (0.6); 2.6 s | 0 |

The s9 row's extra 0.2x is `dev`'s peer-stack region pins (`c4ecb17ec2`), filed as
[`g1-peer-stack-word-pins-hold-eden-regions-and-double-young-copies-20261027`](../../internal/fixed-bugs/g1-peer-stack-word-pins-hold-eden-regions-and-double-young-copies-FIXED-20261027.md)
and fixed by g1pin (2026-10-27): a blocked peer's word now pins its object, not its region. The g1pin
row, at a much higher load, keeps 1.06x of the no-pin wall (against s9's 1.08x); what is left of it is
in the full collections, whose in-place compaction keeps the region pin.
The s5 and s6 rows ran at load 45-55; repeats of the s6 arm against HotSpot at other times read
2.44x / 1.88x, 2.83x / 1.81x and 3.04x / 1.96x. The s7 row ran on a nearly idle host and still reads
2.44x: without contention the gap is the collector's own (young 2.7x, full 4.4x HotSpot's), and under
load it widens because CratonVM asks for twice the CPU (`schedstat`: CratonVM's threads ran 39 s and waited 161 s on the run queue in one run; HotSpot's ran
21 s and waited 89 s).

**What landed** (each with its switch; details on the sizing page's "What changed"): Phase 4 skips the
regions the closure settled; the forwarding map is built sorted by a parallel bucket sort; long keep-alive
lists are seeded on the pool; young pauses stay parallel under the allocation latch and on a Free pool
short of the young set (`CRATONVM_G1_PARALLEL_UNDER_SHORT_FREE`: the last young pause of each round ran
serial at 480-535 ms of closure, against 52-99 ms parallel); one candidate classification instead of two,
the implausible-header screen inlined; batched provenance records; a lock-free metadata-pin key filter;
the in-place full compaction carries sizes per pair, resolves marks through a start bitmap, and takes its
plan's pairs as the sorted map.

**Where a `G1epKeepRounds 10` run goes now** (s6, two runs at 26.8 / 25.9 s against HotSpot's 9.8 / 10.2):

| | CratonVM | HotSpot |
|---|---|---|
| young pauses | 79-81, 11.0-11.8 s, 8.6 GB copied | 68, 4.7-4.8 s, about 7.3 GB copied (region transitions) |
| full collections | 9, 3.4-3.5 s | 9, 0.71-0.79 s |
| in-place phases, 9 fulls: parse / mark / plan / map / copy / fix-up | 0.74-0.89 / 0.81-0.84 / 0.47-0.54 / 0.33-0.42 / 0.35-0.36 / 0.52-0.56 s | (mark, prepare, adjust, compact) |
| take-over post-quota (`xt_post_quota_us`, outside `collect_us`) | 1.5-2.5 s | - |
| mutator with no collection (`-Xmx12g`, run ms) | 5.3-6.0 s | 3.5-3.7 s |
| boot and exit | about 1.9 s | about 1.1 s |

**What is left, by size:**

1. **The young copy's CPU per object**, about 1.7x HotSpot's. A `perf` profile puts it in the copy and
   the claim (`evacuate_screened` 19%), the candidate's first header miss (`classify_candidate_header_view`
   + `evacuation_candidate_screen_view` 10%), and the forwarding map's build (sort, scatter, index: about
   6%). Measured and rejected: the screens off (`CRATONVM_G1_PARALLEL_EVAC_SCREEN=0`, CPU 39.5 against
   40.0 s: the miss moves into the copy), prefetching referents (`CRATONVM_G1_EVAC_PREFETCH`, now opt-in),
   fewer workers (8 and 5: slower at the same CPU), `CRATONVM_G1_EVAC_LEAF_NO_PUSH`. HotSpot keeps
   forwards in headers only; the forwarding map exists because the VM's root remap reads
   `GcResult::pointer_map`.
2. **The full collection**, 4.5x HotSpot's. It makes six passes where HotSpot makes four: a parse of
   every object, dead ones included (0.8 s), before the mark, and a separate fix-up walk after the
   copy. A mark into a bitmap that needs no parse, and forwarding resolved from the plan, are the two
   shapes.
3. **The concurrent mark traces through objects above TAMS.** The gray set sits at its ~1.05 M cap
   with 0.05-0.5 M young entries, and every young pause during a cycle seeds them as keep-alive roots,
   re-grays their copies and remaps the whole set: 1.2 s of serial driver time over 41 pauses in one run
   (the split 0.39 s, the re-gray and remap tail 0.82 s; `CRATONVM_G1_KEEPALIVE_REGRAY_ONCE=1` halves
   the tail but lengthens the cycle, neutral overall). HotSpot
   never marks above TAMS. Not marking there would make the mark's completeness rest on every store path's
   SATB pre-barrier, so it is a correctness change, not a tuning one.
4. **Post-quota take-over time** (`vm/src/runtime/interpreter/gc_and_alloc.rs` `stw_take_over_and_wait`):
   1-25 ms per pause in the helper-window block, which finds no window on this row
   (`helper_windows=0` on all 109 pauses of one run). `CRATONVM_XT_HELPER_UNPARKED_READ=1` does not move
   it. Owned by the JIT root scan, not the collector.
5. **The mutator**: the main thread's CPU outside collections is about 4 s against HotSpot's 1.75 s, and
   boot plus exit 1.9 s against 1.1 s. Not a collector cost.

## The rows (2026-10-24, g1ep)

Host 64.177.43.150 (16 cores, shared, load average about 7). Branch `g1-evac-proxy-ki`, binary `ld20`
(the content of `c8a6828262`). HotSpot 25.0.4 with `-XX:+UseG1GC -Xmx1g`.

| | CratonVM G1 | HotSpot G1 |
|---|---|---|
| `R23W6KeepLong 20` (rounds of 2.4 M nodes in 20 s) | 12, 12 | 82, 82 |
| `G1epKeepRounds 10` wall (s) | 18.6, 19.0 | 2.6, 3.0 |
| `G1epKeepRounds 10` user CPU (s) | 50.1, 51.5 | 17.4, 17.8 |
| `G1epKeepRounds 10` pauses / `Pause Full` (`-Xlog:gc`) | 90-92 / 0 (9 starved fulls) | 77-79 / 9 |

`tools/probes/G1epKeepRounds.java` is the fixed-work twin of `R23W6KeepLong`: it runs N rounds
instead of N seconds, so a loaded host can compare CPU and wall per round.

**First young pause, 481 MB copied** (`scale.sh`, `CRATONVM_G1_WORKERS=w`, `R23W6KeepLong 1`):

| workers | 1 | 2 | 4 | 8 | 13 |
|---|---|---|---|---|---|
| `closure_us` | 1 469 884 | 725 660 | 386 015 | 298 047 | 268 789 |
| ns per byte | 3.05 | 1.51 | 0.80 | 0.62 | 0.56 |

The copy scales with workers: 5.5x at 13. HotSpot's first pause on the same row copied about
300 MB in 52 ms (0.17 ns/B) in the old page's Windows measurement, which was not re-taken on this
host. The gap at full width is therefore about 3.3x per byte. HotSpot's later young pauses here, read
from `-Xlog:gc` sizes and times, run at roughly 0.35-0.45 ns/B under this host's load.

Over a whole run (`G1epKeepRounds 10`, `phase.sh` sums of `[GC-STAT]`):
- Young pauses take 14.6-20 s of the run. Mixed and full pauses take 2-5 s.
- The young closure copies 8-9 GB at 1.3-1.9 ns/B. Most young pauses still run serial: in
  `young evacuation: parallel=N serial=M`, `serial` is about 85%.
- With `CRATONVM_G1_PARALLEL_UNDER_LOW_FREE_LATCH=1`, the closure halves (0.75-0.87 ns/B), but the
  parallel driver's unattributed time (`other_us`) grows from about 1.3 s to 4-6.7 s. Young time
  went from 22 s to 17.5-21.7 s, which is not a clear win.

## Where the copy's time goes (`perf record -e cpu-clock -g`, `G1epKeepRounds 4`, latch arm)

| symbol | share of all samples |
|---|---|
| `PointerMap` (`hashbrown` insert, `Equivalent::equivalent`, `rustc_entry`, `get`, rehash, `par_extend`) | about 19% |
| `SharedEvac::evacuate` (mostly the forwarding-claim CAS, the first touch of each object) | 8.3% |
| `classify_candidate_header_view` (the per-reference header screen) | 5.9% |
| `num_slots`, `ScanCache` TLS, `object_total_size`, `object_instance_size` (layout lookups) | about 10% |
| `concurrent_mark_step_worker`, `metadata_pin::roots_for_loader` | 8% |

JIT code is 2.3% of the main thread's samples. The gap is in the collector.

## Closes when

`G1epKeepRounds 10` takes at most 2x HotSpot's wall on the same host at the same time, with
`fact chains_ok=true`. A narrower milestone: the first-pause closure at 13 workers is at most
0.34 ns/B (2x HotSpot), and young pauses on a low Free pool run parallel without the `other_us`
growth.

**No longer blocks** the default-ON flip of `CRATONVM_G1_HEAP_RESIZE` (refused 2026-10-25 on
`BinTreesClassic 18` at 1.155x CPU; the g1sz soak read 0.983x and the flip landed):
[`g1-default-sizing-never-collects-a-short-allocation-run`](../../internal/fixed-bugs/g1-default-sizing-never-collects-a-short-allocation-run-FIXED-20261026.md).

## What would have to exist

1. **A forwarding side table that is not a hash map of every copy.** The pause writes each forward
   into the object header (`publish_claimed_forwarding`) and also into the `PointerMap`. The map's
   inserts and probes are the largest single cost. The fix-up walk could read forwards from
   from-space headers, which stay intact until the free phase. The VM's root remap
   (`result.pointer_map`, with many consumers in `vm/`) is what requires the map. A per-pause
   address-keyed structure built lazily, or the VM consulting headers before the free phase, are
   the two shapes.
2. **The parallel driver's fixed cost on small pauses.** `other_us` is 24 ms per pause on the latch
   arm. The candidates are the shard merge into the `PointerMap` (`par_extend_pairs_with`) and the
   per-pause worker setup.
3. **The header screen.** `classify_candidate_header_view` runs once per CSet-bound reference.
   Before removing it, measure whether its cost is only the object's first cache miss moving
   elsewhere (its loads are the first touch of the header that `evacuate` reads next).
4. **Layout lookups.** `num_slots` / `ScanCache` resolve the class layout per object. A per-pause
   cache keyed by class id, or the size carried in the header for instances, would remove the TLS
   lookup.

Rejected or neutral this round (2026-10-24): `-XX:MaxTenuringThreshold=1` (copied bytes unchanged,
so survivor re-copying is not the volume), `CRATONVM_G1_EVAC_WORKERS_BY_POOL=0` together with the
latch (no gain beyond the latch alone), `CRATONVM_G1_IHOP_BACKOFF=1` (not relevant to this row).

## Row from the in-process `javac` retirement (2026-10-27)

`tools/probes/spring/ZipFsFinalizerChurn.java <300 jars> 60`, `-XX:+UseG1GC -Xmx512m`, host 64.177.43.150:
HotSpot 25 runs it in 15.6 s with 77 young pauses totalling 2.2 s; CratonVM (dev `6bb160f1b9`) takes 225 s with
280 young pauses totalling 61 s (median 210 ms), stdout identical. Each round drops 300 unclosed
`jdk.nio.zipfs.ZipFileSystem` objects, which have a `finalize()`: a young pause that finds them dead keeps their
entry indexes alive for finalization, and `G1Collector::resurrect_dead_finalizers` evacuates that graph on the
driver alone (`scan_and_evacuate_refs_tuned` -> `evacuate_object_tuned`), ~35 % of the allocation-path samples and
~12 % of the main thread. HotSpot keeps FinalReference referents alive in its parallel reference-processing phase.
The remark half of the same graph was fixed
(`docs/internal/fixed-suite-bugs/spring/in-process-javac-throughput-aot-test-classes-RETIRED-20261027.md`).
