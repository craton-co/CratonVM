# Proposal: prune the concurrent sweep's reference rows once per pause boundary, not once per slice

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 43
> of 54).** Not built (`gen_conc_sweep_drop_reference_rows` per slice).
> **Gate:** the page's 100k-`WeakReference` probe:
> `concdrv_sweep_reference_row_prunes` from about slices to about pauses, same
> rows dropped, sweep wall time lower. **Size:** S.

*Filed 2026-09-27 by the GC defects round, wave d3, lane n (`old+refs`). A
direction (performance, default path), not a defect. Unmeasured.*

## Where things stand

Since gcd d1/c the generational concurrent sweep drops the soft / weak /
phantom rows of the `Reference`s it frees
(`vm/src/runtime/interpreter/gc_and_alloc.rs::gen_conc_sweep_drop_reference_rows`,
called from `maybe_concurrent_gc_at`'s Phase 4). In the sliced arm it runs
after EVERY slice that freed anything: one `ref_processor` lock and one
`retain_reference_rows` walk of all three lists per slice
(`ReferenceProcessor::remove_reference_objects_registered_before`, a binary
search of the slice's spans per row). With `R` rows and `S` slices that free
something, the sweep does `O(R * S * log spans)` work and takes the
processor lock `S` times, contending with every `SoftReference.get()` touch
and every `Reference` constructor on the mutator threads. A server holding
100k `WeakReference`s (a `WeakHashMap`-heavy cache, a class-value table)
whose old generation needs a few hundred slices pays tens of millions of row
tests per cycle for a result that is empty on most slices.

## Why per slice is no longer necessary

d1/c pruned per slice so that no dead row could meet a NEW object on a freed
block. Two things now make that meeting harmless between pauses:

- the registration bound: a row registered after the sweep's snapshot is
  never dropped, whatever its address (d1/c);
- the tie-breaks: both address indexes a mutator reaches between pauses now
  resolve a shared address to the LATER registration -- the application
  retirement index since gcd d2/g (`rebuild_app_row_index`), the soft touch
  index since gcd d3/n (`claim_soft_addr`).

Every other reader of a row (the pre-collection null pass, the
post-collection restore / clear / enqueue loops, the skip-set candidates)
runs inside a pause. So the rows of a freed span must be gone before the
next PAUSE, not before the next slice -- exactly the contract the
address-keyed tables' `survived_in_place` sweep already follows in the same
loop (`unswept_frees` + `stw_requested` before the between-slice
`safepoint_check`, and once at the end).

## The proposal

In the sliced arm, accumulate each slice's freed spans (merged, sorted) and
call `gen_conc_sweep_drop_reference_rows` only (a) before a
`safepoint_check` that is about to join a requested pause
(`gc_barrier.stw_requested`), (b) when the accumulated list exceeds a bound
(e.g. 64 Ki spans, to cap memory), and (c) once after the last slice. A
pause requested after the check cannot complete without this counted
thread's next arrival, which happens at the next between-slice check, where
(a) runs first -- the argument the address-keyed sweep already rests on.

## Cost / risk

Memory for the accumulated spans (16 bytes per freed run; merged adjacent
runs make it far smaller than the freed-object count). Risk: a future
mutator-side reader of a row by address that is NOT one of the two indexes
above would see a dead row until the next pause boundary; any such reader
must use the later-registration rule too. Opt-in first
(`CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH`), census `concdrv_sweep_reference_row_prunes`
(prune calls per cycle) next to `concdrv_sweep_reference_rows_dropped`.

## How to verify

- `cargo test -j 5 -p cratonvm-vm --lib a_sweep_slice_drops_the_rows_of_the_references_it_freed`
  and the gcd d1/c / d2/g / d3/n `reference.rs` tests unchanged.
- `GenR5W5RemarkRefsProbe` and `GenR4DroppedReferenceLeakProbe` stdout as on
  the base, both arms.
- Perf: a probe holding 100k live `WeakReference`s in old gen while churning
  old garbage, `CRATONVM_DBG=gc-stats`: `concdrv_sweep_reference_row_prunes`
  falls from ~slices to ~pauses-during-sweep + 1, the same
  `concdrv_sweep_reference_rows_dropped`, and the sweep's wall time drops
  (interleaved medians, both arms in one binary).
