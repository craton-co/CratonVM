# GC core files review (safepoint, collector, gc, heap, satb, shadow_stack, compact_header, region, heap_geometry, heap_reservation): residuals

> **STATUS (2026-10-19, gcj j2/cm2): OPEN, one item fixed.** Item 12 FIXED IN CODE (comment only: `deactivate_and_drain`'s double-stop note now says what the code does; `9a0fea2ed3`). Item 14 (the per-thread lock walk in the remark / cleanup pause) has its Generational fix built behind opt-in `CRATONVM_GEN_SATB_DIRTY_THREAD_WALK` (`ca7e3c668b`, proposal `gcj-j1cm-proposal-dirty-thread-satb-walk`); G1's queue is unchanged. Re-checked on today's tree and still open as described: 1-11 (legacy semi-space `Heap` / `gc.rs`, no production backend), 13 (orphan buffers walked twice: unchanged by the dirty walk, which keeps the orphan pass), 15-22 (`SatbBuffer` test-only, `shadow_stack.rs`, the feature-gated `compact_header.rs`), 23 (deprecated `RegionHeap`), 24 and 26 (G1, out of the gcj scope), 25, 27. None of them is reachable from the Generational default path; the memory-safety ones (18-21, 23) stay MEDIUM for whoever next enables those modules.

> **STATUS (2026-10-19, gcj j1/cm): OPEN -- filed by reading on base `953a317758` (no build, no run); fixed items are listed at the end.** Nothing HIGH on a production path. The memory-safety items are in code no production backend reaches: the legacy semi-space `Heap` (`gc/src/heap.rs` / `gc/src/gc.rs`; no `VmHeap` arm, tests only), the feature-gated `compact_header.rs` (`cfg(any(test, doc, feature = "lilliput-prototype"))`, `gc/src/lib.rs:139`) and the deprecated `RegionHeap` in `region.rs`. G1-only items are recorded for a G1 round (out of the gcj scope) and are marked so. Line numbers are on the base; `heap.rs`, `gc.rs`, `shadow_stack.rs` moved by this lane's edits -- search by name.

## Legacy `Heap` / `gc.rs` (tests and embedders of the legacy heap only)

1. **(MEDIUM) `Heap::is_valid_heap_object` uses `try_lock`** (`heap.rs` ~1605-1629): any concurrent allocation
   (`alloc_zeroed` holds `from_space` briefly) makes it answer `false`, so `get_field`'s compact-reference arm
   (`autobox::unbox_reference_field`) and `get_array_element_unboxing` return the `AUTOBOX_CLASS_ID` wrapper
   instead of the boxed value -- a nondeterministic wrong value. The deadlock rationale does not hold (the
   collector holds the arena locks only inside its STW pause). Fix: capture both semi-spaces' `(base, len)` at
   `with_capacity` (the buffers never move; `swap` exchanges them) and test membership without a lock.
2. **(MEDIUM) The GPU token is check-then-act** (`heap.rs` ~2095-2124; `safepoint.rs` documents it now, invariant
   4): a collection that read the count as zero is not excluded by a later `enter_gpu_critical`, and a
   `mem::forget`-ed token hangs the legacy wait forever. Fix: a `COLLECTING` high bit taken by
   `compare_exchange(0, COLLECTING)` in the GC wait, which `enter_gpu_critical` backs off from.
3. **(MEDIUM, opt-in) `drop_roots_inside_unallocated_spans_judged` is O(roots x spans) and O(roots x verdicts)**
   (`heap.rs` ~468-505, two `iter().find` per root) inside the take-over pause under
   `CRATONVM_XT_TAKEOVER_DROP_TAIL_WORDS=1` -- the cost `skip_spans_hold_no_root_judged` removed (~346-379).
   `find` takes only the first containing span. Fix: the sorted-starts + prefix-max-end search of ~357-379 above
   `SKIP_SPAN_LINEAR_MAX`; drop a root only if every containing span is `Unallocated`.
4. **(LOW) GPU pin-set rewrite can lose a concurrent `pin_ref` / `unpin_ref`** (`heap.rs` ~1757-1791,
   ~1828-1861; UNSURE whether a pin can happen during the pause). Hold `gpu_pinned_refs` from snapshot to rewrite.
5. **(LOW) Compact `set_field` boxing uses the aborting `alloc_object`** (`heap.rs` ~1348-1352) where
   `set_array_element` uses `try_alloc_object` and reports `ARRAY_STORE_OUT_OF_MEMORY`.
6. **(LOW) `let _ = with_class_layout(..)` silently leaves a compact object's references un-forwarded on a
   layout miss** (`gc.rs` ~324, ~795, ~965; practically unreachable). Use `with_compact_oop_scan`, which counts
   `COMPACT_OOP_MAP_MISSING`.
7. **(LOW) `collect_with_finalizers`' fixpoint comment describes a case that cannot occur** (`gc.rs` ~850-905):
   phase 3 forwards every finalizer before any phase-3b scan, so the second iteration is always a no-op.
8. **(LOW) Corrupt-cell coordinates are four independent `Relaxed` stores** (`heap.rs` ~3212-3254): two hits
   at once can mix one cell's slot with another's raw words and tid. Diagnostic only.
9. **(LOW) `OnceLock` latches defeat the per-thread test overrides their comments promise** (`heap.rs`
   ~2426-2459).
10. **(INFO) Stale or wrong comments**: the `UNATTRIBUTED` coercion claim (`heap.rs` ~1478, ~2273, ~2519, ~2573,
    ~2637: the trait defaults pass `FieldCoercionSite` since G45); the `Send/Sync` SAFETY note (~772-782: volatile
    access now takes the stripe lock); the misplaced `read_value_cell_checked` doc on `static CORRUPT_CELL_SLOT`
    (~3002-3047) and its link to a nonexistent `CORRUPT_CELL_HITS`; heap.rs ~2118 hard-codes ">5s" instead of
    `GPU_CRITICAL_DEADLINE_SECS`; the "recomputed AFTER the boxing closure" note (~1356) protects nothing.
11. **(INFO) Process-global statics holding per-VM state**: `YOUNG_MARK_WATCH` / `DYNAMIC_WATCH`
    (`heap.rs` ~3665-3694), `GC_START_HOOK` / `GC_FINISH_HOOK` / `GC_HOOKS_ACTIVE` (`gc.rs` ~64-66: JVMTI GC
    events of every VM go to the first installer), `CLASS_INFO_HOOK`, `CLASS_NAMER`, `DISPLACED_HASH_RESOLVER`
    (UNSURE), and the counters `print_gc_summary` reports per VM (`COMPACT_OOP_MAP_MISSING`, `SKIP_SPAN_*`,
    `CORRUPT_CELL_*`, `ARRAY_RECEIVER_FIELD_ACCESSES`); `vm_heap::GPU_CRITICAL_COUNT` is process-global with a
    "one VmHeap per process" justification the multi-VM layout-domain work contradicts.

## `satb.rs`

12. **(LOW) `SatbQueue::stop`'s double-stop comment is false** (~1561-1569): the CAS result is discarded, so a
    stop of an INACTIVE queue runs the whole registry walk, three shard passes and the in-flight wait again.
    Over-retains nothing; fix the comment or return `drain()` on the INACTIVE arm.
13. **(LOW) An exited thread's orphaned buffer is walked twice per flush and its registry slot never pruned**
    (~184-186, ~399, ~738-749): the orphan list's strong `Arc` keeps the registry `Weak` upgrading. O(dead threads)
    extra lock acquisitions per remark / cleanup pause. Mark the buffer orphaned and skip it in the registry pass.
14. **(INFO, cost) `flush_all_thread_satb_buffers` / `discard_all_thread_satb_buffers` take one mutex per
    registered thread in the pause** (~727-910); `flush_thread_satb_buffer` (~600) registers a buffer for a
    thread that never logged. See the dirty-thread proposal
    `docs/internal/proposals/gc/gcj-j1cm-proposal-dirty-thread-satb-walk-20261019.md`.
15. **(INFO) `SatbBuffer` has no user outside its own tests** (`git grep 'SatbBuffer\b'`), and the module /
    `SatbQueue` docs describe callers that do not exist. Gate it `#[cfg(test)]`. Process-global census counters
    `BARRIER_LOGS`, `BARRIER_SPILLS`, `WALK_*` mix VMs (diagnostic).

## `shadow_stack.rs`

16. **(LOW, UNSURE) `remap_in` rewrites an indirect home once per entry** (~806-839): two indirect entries naming
    the same frame slot apply the map twice, which is wrong for a chained map (`old -> new` where `new` is also a
    key). Harmless for evacuating maps; confirm whether a slide / compaction map ever reaches `ShadowStack::remap`
    (callers `vm/src/memory/gc.rs` ~2207, `gc_and_alloc.rs` ~25471, `vm_exec.rs` ~8457). Fix: rewrite each distinct
    indirect home once (`indirect_slots_in` already dedups).
17. **(INFO) `unsafe impl Send for ShadowStack`** (~594) is redundant (every field is `Send`) and its SAFETY text
    describes a `Sync`-like property.

## `compact_header.rs` (feature-gated prototype; unreachable in production)

18. **(MEDIUM) `CompactAllocator`'s OOM rollback races** (~1041-1046, ~1087-1091): `fetch_add` then `fetch_sub`
    on failure lets two later allocations receive the same bytes. Use a CAS loop.
19. **(MEDIUM) Unchecked size arithmetic in safe `pub fn`s** (~1038-1059, ~1082-1098): a wrapped `start + aligned`
    passes the bound and `write_bytes` writes outside the buffer; `length as u32` truncates. `checked_*` throughout.
20. **(MEDIUM) Writes through `Vec::as_ptr()` behind `&self` to 1-aligned storage** (~1048-1098, reads ~1112-1117):
    UB by `as_ptr`'s contract and misaligned `u64` / `u32` access. Use `UnsafeCell<u64>` storage or unaligned access.
21. **(MEDIUM) Safe functions dereference arbitrary raw pointers** (`read_header`, `read_array_length`,
    `class_id_at`, `header_view`, ~1111-1132). Make them `unsafe fn` or bounds-check against `storage`.
22. **(LOW) A forwarded header still answers `is_array` / `has_hash` / `element_type` / `gc_age` from forwarding
    bits** (~392-402, ~703-717, ~924-934). Stale comments: the `Forwarded` layout (~213), "32 - 8 = 24 bytes"
    (~1063, ~1190). `FORWARDING_OVERFLOW` (~159) is per-collection state in a process static.

## `region.rs`

23. **(MEDIUM, deprecated `RegionHeap`, compiled and `pub`, no production user) `RegionHeap::evacuate` can copy
    into a collection-set region it then zeroes, and walks its own copies** (~1115-1282). **(MEDIUM, same)** the
    humongous filler header overwrites the object's payload at byte 0 of each continuation region (~910-927), and
    `is_humongous_filler` (~1468) has no caller. Delete `RegionHeap` or fix as the review notes say (skip CSet
    regions in `alloc_in_type`, snapshot `top`; skip `HumongousContinuation` in walkers instead of a filler).
24. **(LOW, G1 -- out of the gcj scope) `RememberedSet::clear` clears the map and the coarsened flag in two
    steps** (~466-470: the lock guard is a temporary), and `names_source` / `retain_and_collect_sources`
    (~571-632) check the flag outside the lock. A concurrent add between them is lost / a coarsening between them
    hands back an empty source list. Fix: one critical section; re-read the flag under the lock. For a G1 round.
25. **(INFO)** stale `evacuate` comments (40-byte header, `AtomicU64` mark word at offset 32; the real header is 16
    bytes with an `AtomicU32` mark word at offset 4); `Region::_remaining` dead; `rset_source_cap` and
    `REGION_TYPE_DECODE_FAILSAFE` are process statics.

## `heap_reservation.rs` / `heap_geometry.rs`

26. **(LOW, G1 -- out of the gcj scope) `decommit_to` returns `true` whatever `decommit_range` released**
    (`heap_reservation.rs` ~211-230), so `G1Collector::uncommit_to_bytes`'s "OS refused" branch is dead for a
    reserved heap and its released-bytes figure can overstate the shrink (reporting only; lowering `committed`
    first is the safe direction). Return the released byte count. For a G1 round.
27. **(INFO)** `HEAP_SPANS` (`heap_geometry.rs` ~91) is process-global per-heap geometry, documented and
    tolerated by its one consumer.

## Fixed in this lane

`ShadowStack::set_top` panicking in `Ord::clamp` on `end < base` and `push` writing through an out-of-range /
unaligned `top`; `gc.rs` `try_forward_object`'s fixed 64 MiB cap (aborted on a legitimate large live array; now
the arena bound); the compressed-oops width of `write_prim_element`'s cell watch; the stale comments in
`safepoint.rs`, `collector.rs` (the `Heap implements GarbageCollector` claims, line references,
`write_barrier_pre`'s "zero-cost default" users), `heap.rs` (`Heap::new` capacity, `read_prim_element`'s
overflow note) and `satb.rs` (`SatbBuffer::drain`). See the j1-cm report.
