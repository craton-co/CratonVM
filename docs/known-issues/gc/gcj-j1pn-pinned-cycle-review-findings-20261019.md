# Pinned in-place young cycle: review findings left open by gcj j1/pn

> **STATUS (2026-10-19, gcj j2/pn2): ITEMS 2-6 FIXED IN CODE, PENDING BUILD/PROBE; item 1 re-read and narrowed (not changed); item 7 open; one new residual (item 8).** Item 2: `InPlaceEvac::note_refused_forward` -- the serial tag / suspect-header exits of `forward_object_impl` and every parallel refusal (`absorb_parallel_in_place` reads `EvacShard::refusals`; `ParEvac::claim_in_place`'s lost self-forward is now a refusal) mark a pinned object `visited` (kept, not scanned), `CRATONVM_GEN_PINNED_REFUSED_FORWARD_KEPT` (default ON, `=0` kills), counted as `prk_kept_sum=`. Item 3: `pxr_*` count only cycles whose seeds `_EXACT_ROOTS` / `_NAMED_ROOTS` named (`PinnedYoungPlan::exact_rule`); the watched / finalizable filters' drop cycles are `pfd_cycles` / `pfd_dropped_sum` / `pfd_dropped_bytes_sum` on the same `[GC] pinned_exact_roots:` line. Item 4: the comments named. Item 5: `moving_young_dangling_verify` skips `pinned_droppable` objects. Item 6: `pinned_young_census` slots 6-7 (`pycopy_objects_sum` / `pycopy_bytes_sum`) exclude what the cycle reclaims. Item 1, re-read: the i35/wy argument is "the marking side already treats the word as no reference"; it covers a register / layout-free word exactly when the pause's root vector does not hold its object either, and a word the root vector holds already passes the test (`in_root`). The residual is a word the LEDGER records that no root channel publishes -- a Rust frame's raw address, which the VM's own discipline forbids across a safepoint (raw refs must be pinned). So item 1 is defense in depth, not a reachable reclaim of a live object, unless a VM frame breaks that discipline; tagging ledger words by source would cost a parallel bit array in `YoungPinSlot` and a second read API. Not built; deciding row: `GenR4W6JitOomRootProbe` under `CRATONVM_DBG_STALE_OBJREF=1` with `CRATONVM_GEN_PINNED_WATCHED_START_NEEDS_ROOT=0` vs default (a stale-objref line only on the default arm would refute this reading). Item 8 (new): a shard keeps at most `REFUSALS_PER_SHARD` (64) refusals, so a pinned object refused past that cap on the parallel arm is not seen by `note_refused_forward` (still dropped). Rows: `docs/internal/gc-round-gcj-20261019/j2-pn2-report.md` block PC. Tests: `gcj2pn2_a_refused_pinned_object_is_kept`, `gcj2pn2_filter_drop_cycles_are_not_counted_as_exact_roots`, `g1y_the_exact_roots_census_counts_seeds_and_dropped_neighbours` (updated).**

> **STATUS (2026-10-19, gcj j1/pn): OPEN -- found by reading (no build, no run); one finding fixed in code this round (below), the rest open.** Generational only. Line numbers are today's tree on branch `gcj1-pn` (they move; grep the names).

The drop mode (`InPlaceEvac::drop_unvisited_pinned`, `gc/src/gen_heap.rs`) is not an
`CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`-only path: `drop_unvisited_pinned = plan.roots.is_some()`,
and the default-ON watched filter (`CRATONVM_GEN_PINNED_WATCHED_UNNAMED_NOT_ROOTS`) and finalizable
filter (`pinned_finalizable_unnamed_not_roots_enabled`) both set `plan.roots`
(`collect_garbage_inner_with_pins`, the `pinned_plan` block). So every finding here is on the
default path whenever one of those filters drops a seed.

## Fixed this round

0. **An unvisited SEED was reclaimed.** A seed whose forward exits before the pinned arm -- the
   kind/element-tag screen and the suspect-header screen of `forward_object_impl`, the suspect
   screen of `gen_evac::ParEvac::evacuate`, and `claim_in_place`'s `Err(_)` arm ("self-forward lost
   to a non-forwarded word") -- stays unvisited, and the rebuild zeroed and freed it while a pin
   word named it. Fixed: `InPlaceEvac::pinned_droppable` never drops a seed
   (`CRATONVM_GEN_PINNED_SEEDS_NEVER_DROPPED`, default ON, `=0` kills). Test
   `gen_heap::gcf_f5y_tests::gcj1pn_an_unvisited_seed_is_never_dropped`.

## Open

1. **Soundness (medium confidence): the i35/wy start-word rule also applies to REGISTER and
   native-band words.** `pin_name_counts` (`gen_heap.rs` ~1414) counts a word exactly at a watched
   object's start only if the pause's root vector holds the object. Its argument (doc of
   `pinned_plan_seeds_without_unnamed_watched_rooted`, gci i39/rv correction 2) is about STACK words
   in compiled bands. But `pause_young_pin_words` also returns the deposits' callee-saved register
   words and the layout-free (Rust) band words, which are exactly the unrewritable VM-frame holders
   the plan exists for. A watched object (a referent, but also a `Reference` or a queue, which are
   not weakly held -- the doc's "weakly held by definition" is wrong for those) whose only holder
   is such a word is dropped and reclaimed under it. Fix: tag ledger words by source
   (`YoungPinDeposit` already knows: register image, compiled band, layout-free band) and apply the
   start-word root test to compiled-band words only; or count register and layout-free words as
   rooted, as i39 does for card walk-gap words (`pinned_watched_start_rooted_set`). Decide with
   `WeakClearYoungProbe` (must still clear) and `GenR4W6JitOomRootProbe` under
   `CRATONVM_DBG_STALE_OBJREF=1`.
2. **Non-seed objects reached through a refused forward are dropped.** Same exits as item 0 for an
   object that is not a seed: before the drop mode it stayed (in `live`, untouched); now it is
   reclaimed although something reached it. Rare (a corrupted header), but the fail direction
   changed from over-retain to free. Fix: record a refusal on a pinned object as `visited`
   (`note_forward_refusal` sees the address).
3. **Census mislabels.** `[GC] pinned_exact_roots:` (`pinned_exact_roots_census_line`) says "all
   zero with the switch off", but `note_pinned_young_cycle` counts every drop-mode cycle, watched
   and finalizable drops included; `exact_seeds` is then the post-filter seed count. Fix: count a
   cycle in `pxr_*` only when `_EXACT_ROOTS` / `_NAMED_ROOTS` set the roots, and add a separate
   `pwd_*` (watched-drop) row.
4. **Stale comments.** `InPlaceEvac::drop_unvisited_pinned`'s doc, `arm_in_place_young_cycle`'s and
   `finish_in_place_young_cycle`'s "under `CRATONVM_GEN_PINNED_COPY_EXACT_ROOTS`" and the two
   "every object on a pinned page is a ROOT" comments at the seed loops of the serial and parallel
   arms describe the pre-i30 state.
5. **Verifier false misses.** `moving_young_dangling_verify`'s in-place scan walks `ip.live`, which
   still holds the unvisited (to-be-dropped) pinned objects; their slots were never rewritten, so
   it can report `MISSED-HEAP-REWRITE YOUNG-INPLACE` for dead objects. Fix: skip
   `pinned_droppable(i)` objects.
6. **Pinned-object counters include dropped objects.** `pinned_young_census` slots 6-7 add
   `ip.pinned.len()` and bytes, including objects the same cycle reclaims.
7. **Unconfirmed: a tail gap may be free-listed uncommitted.** If the last pinned page reaches past
   `used_at_start` and copies land above it, `[used_at_start, page_end)` becomes a free block below
   the new cursor without `commit_for_relocation`. Harmless if free-list hand-out commits lazily
   (`decommit_free_blocks` suggests it does); confirm in the arena's hand-out path.

## Checked and found correct (so nobody re-reads them)

Parallel claims and `absorb_parallel_in_place`; the loader rescue, side-table rows, Phase 2.5 /
2.5b, `redrain_serial_closure` (all through the pinned arm); weak / JNI-weak / String-table
survival (`pointer_map` then a non-zero header, after the rebuild); destination spans vs pinned
pages (`subtract_spans`, tail window); `pinned_young_pages` slack bounds.
