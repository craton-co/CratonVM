> **STATUS (2026-10-19, gcj j2/rf2): item 4 PARTLY FIXED (comments only, commit `26a6f77732`): the phantom comment in `cleared_ref_objects` and the module doc's "everything runs inside the pause" (the concurrent sweep's row prune runs outside one) are corrected; the `Finalizer.register` row comments and the test-only `FinalizerThread` machinery are left (a dead-code removal touches every backend's tests). Items 1 (G1), 2 (census counter, shared code: a lifetime counter changes `-Xlog:gc+ref` figures on every collector, so it needs a gate and HotSpot captures), 3 and 5 open, unchanged.**

> **STATUS (2026-10-19, gcj j1/rf): OPEN -- found by reading `gc/src/reference.rs` on `953a317758` (not run). Item 2 of the review was FIXED IN CODE this wave for Generational (`CRATONVM_GEN_SOFT_CLEAR_ALL_ADVANCES_CLOCK`, see the j1-rf report); the items below are what is left. Each says what would close it.**

# Reference processor: review residuals (gcj j1/rf)

The core of the processor checked out on reading: the phase order (soft, weak, final, phantom),
the HotSpot LRU arithmetic (`clock - ts > ms_per_mb * free_mb_at_last_gc`, the previous round's
clock, stamps capped at the GC clock), the last-ditch clear-all, the enqueue-once guards,
application `clear()` / `enqueue()` retirement, finalize-at-most-once, cleaners run once, and the
`FinalizerThread` / `CleanerThread` lock nesting. These are the residuals.

## 1. G1 full collection: an "undiscovered" row is retired for good (G1; out of the gcj round's scope)

`retire_entries_the_pre_gc_pass_found_cleared` (`reference.rs` ~2641) settles every row that is
`pre_gc_examined && !pre_gc_nulled` as cleared + enqueued + emitted with nothing queued. On a G1
full collection under `CRATONVM_G1_REMARK_UNDISCOVERED_REFS_STRONG` (default ON),
`weak_phantom_active_triples()` (~3178) and `condemn()` (~4507) mark the rows examined BEFORE
`take_armed_full_gc_undiscovered` (~2974) drops the held ones from the null list
(`vm/src/runtime/interpreter.rs` ~316-327). The held row is then retired although the round meant
to leave it alone: its referent is never cleared, its queue never notified, and it is strongly
retained while the `Reference` lives (visible when `finalize()` resurrects the `Reference`).
**Close:** skip `self.undiscovered_this_round.contains(&e.reference_obj)` in both loops of the
retirer (the set is still populated there). G1 owner's change; G1 probe row needed.

## 2. Late `stats` increments are lost (all collectors, census only)

`clear_refused_row` (~2757, ~2773) bumps `stats.weak_refs_cleared` / `soft_refs_cleared` from
`clear_after_refused_restore_of`, which the VM calls after the round's result was copied
(`gc_and_alloc.rs` ~19791 / 19884 / 19918, after ~18999); the next round zeroes `stats` (~1600).
The `-Xlog:gc+ref` census under-counts those clears. **Close:** a lifetime counter read by the
census, as `finalizers_enqueued_by_channel` (~4251) already does.

## 3. Address-table rebuilds keep unmapped keys verbatim (all collectors; reachability unsure)

The identity-stamp, referent-class-stamp and `soft_pre_nulled` rebuilds (~2445-2506) map a key
absent from the pointer map to itself, so a stale stamp of a dead `Reference` can coincide with a
live `Reference` moved onto that address (`FxHashMap` iteration order decides which survives) and
refuse its writes -- the collision those rebuilds exist to prevent. Ordering closes it on the main
path (`remove_collected` and the stamp prune run before `update_after_gc`), but
`retain_shaped_weak_phantom` (~2541) drops rows without `prune_identity_stamps()`, and a stamp
for an address whose discovery was a no-op has no row. **Close:** call `prune_identity_stamps()`
when `retain_shaped_weak_phantom` drops a row; in the rebuilds, drop a key no row names. Shared
code: needs a collector gate or a G1 / ZGC owner's agreement (not done here for that reason).

## 4. Stale comments and test-only code

- ~3857 "Phantom refs: Java 9+ does NOT clear the referent" contradicts ~2262 (correct: JDK 9+
  clears a phantom as it is enqueued); the enclosing `cleared_ref_objects` (~3845) has no
  production caller.
- ~1817-1822, ~3302-3305, ~3716-3721 still say `Finalizer.register` creates `cleaner_refs` rows
  with `reference_obj == referent`; since gc-common w5-d `native_finalizer_register` calls
  `register_if_finalizable`, which makes a `Finalizer` row.
- Module doc ~52-55 "Everything here runs inside the collector's stop-the-world pause": the
  concurrent sweep calls `remove_reference_objects_registered_before` / `flush_deferred_sweep`
  outside a pause (~3351-3376).
- `FinalizerThread::enqueue` (~5386), `was_finalized` (~5499) and the `already_finalized`
  machinery are test-only; `cleanup_collected` runs every pause on an always-empty set.

**Close:** a comment / dead-code pass (no behaviour change).

## 5. `SoftReference.get()`'s lock-free touch is keyed on the wall-clock millisecond (perf, unsure)

Under `CRATONVM_SOFTREF_STAMP_GC_CLOCK` (default ON) with the HotSpot LRU, a row's stamp only
changes at a collection, yet the fast path (`soft_touch_stamp` ~909, `vm_exec.rs`
`touch_soft_reference`) matches only within the same millisecond as the last locked touch, so a
hot soft cache read from many threads takes the processor mutex once per thread per millisecond.
**Close (to measure first):** key the stamp on `soft_stamp(now_ms)` in that mode (the epoch
already bumps when the clock moves); check that `last_observed_clock_ms` advancing less often is
neutral. Decide with an interleaved CPU A/B of a multi-threaded `SoftReference.get()` loop.
