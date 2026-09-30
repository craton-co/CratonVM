# Proposal: a JIT-warm TLAB refill takes the `young_from` lock three times and bumps about eight shared counters

- **Status:** PROPOSAL (perf, allocation slow path). Filed 2026-09-29 by gce e1/o, by reading `adb9178bc`. Not measured; nothing changed.
- **Backend:** Generational (the lock); every backend (the counters).
- **Code:** `vm/src/jit/helpers.rs::jit_new_object_body` (the helper's young probe), `vm/src/runtime/interpreter/gc_and_alloc.rs::tlab_alloc_shaped_inner` (the refill gate, the trigger consult, the refill), `gc/src/gen_heap.rs` `GenerationalHeap::try_alloc_young_probe`, `young_has_free_block`, `refill_tlab_at_least`.

## What is wasted

**The lock.** On a JIT-warm program every young cycle is the in-place sweep (term 4 of `divert_non_moving`), which never moves the young bump cursor back. After the first pass through the semi-space the cursor sits at the top, so `young_bump_headroom` (lock-free, the published triple) answers `false` on every call and each compiled `new` that misses its TLAB then takes `young_from` three times:

1. `jit_new_object_body`: `!heap.young_bump_headroom(total_size) && heap.try_alloc_young_probe(total_size).is_none()` -- the locked probe (`lock_young_from().has_free_block_at_least(size)`), once per helper entry;
2. `tlab_alloc_shaped_inner`'s gate: `!young_bump_headroom(bump_probe) && !young_has_free_block(free_block_floor)` -- locked again;
3. `refill_tlab_at_least` -- the carve itself.

`young_bump_headroom`'s own doc (gen r4/alloc) calls these "up to three acquisitions of that mutex on every guarded TLAB refill" and made only the first lock-free; on a swept heap the other two are paid on every refill. Every acquisition is an RMW on the line every allocating thread wants, and `YoungFromGuard::drop` republishes the triple (three stores) each time. The two probes ask almost the same question (a free block of at least the object / at least the fragmentation floor) under separate acquisitions.

**The counters.** Per granted refill, besides the lock: `tlab_wedge.slowpath_entries_since_gc` (`fetch_add`), `tlab_wedge.refill_bytes_since_gc` (load, then `fetch_add`), `tlab_wedge.gate_consecutive_fails` (store), `shared.mem.tlab_refill_count` (`fetch_add`), `gc_entry_census::note_refill` (two process-wide `fetch_add`s -- `REFILL_ATTEMPTS`, `REFILL_SUCCESSES`, which count what `tlab_refill_count` counts), `HeapStats::young_tlab_carved_bytes` and one of `young_zero_skipped_bytes` / `young_zeroed_bytes` (per heap, adjacent fields of one struct), and at the outgoing buffer's retire `bytes_allocated_total` and `thread_allocated_total`. About ten shared-line RMWs per refill. At the 256 KiB baseline that is noise; a thread the ladder shrank to 8 KiB refills 32x as often.

## Proposed change

1. **One locked question per refill.** Publish the young arena's free-block upper bound (`Arena::max_free_upper`, an over-estimate the arena already keeps) with the triple `young_from_published` carries. The helper's probe and the gate answer `false` lock-free when the bound is below the size; when it is not, skip the helper's locked probe (the refill path below decides) and let the gate pass to `refill_tlab_at_least`, whose own `None` is already handled (the wedge breaker counts it: "the gate can keep PASSING on a stale cached free-block bound"). The one behaviour change: a gate that passes on a stale bound now retires the outgoing buffer before the refill fails, where today the locked probe keeps it. Opt-in first (`CRATONVM_GC_REFILL_GATE_LOCKFREE`), kill switch after.
2. **Drop the duplicate refill count.** `gc_entry_census::note_refill` is a process-wide copy of `tlab_refill_count` (per VM) plus the attempts; keep the per-VM pair and print the `[GC] gen-alloc:` / ZGC lines from it. Separately, the `tlab_wedge` stamps could be per thread (summed at the trigger consult), since each is read only by its own trigger.

## How to verify

- Correctness: `CRATONVM_DBG_DEADREF_STORE=1` with the switch on, `GenR4W4EvacThroughputProbe 65536 20000000 -Xmx64m` (`PASS ... corrupt=0`), `BinT 14 -Xmx256m` (`sum=327670`), `[GC] tlab-guard: filler_over_object=0 refill_over_object=0`; the wedge census (`tlab_refill_retries=` on `[GC] gen-alloc:`) not above the default arm's.
- Effect: a multi-threaded compiled allocation loop (`GenR4W4SmallAllocProbe 8 20000000 -Xmx256m`), arms interleaved, medians of 5 (this host's in-VM timings swing about 3x between reps); `perf` on Linux should show `young_from`'s mutex and `refill_tlab_at_least` falling.
