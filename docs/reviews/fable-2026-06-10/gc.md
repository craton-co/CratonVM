# CratonVM `gc` crate — code & test review (fable-2026-06-10)

Scope: `gc/src` (27 files, ~36.5k LOC) + `gc/tests` (5 files). Reviewer focus per
brief: root scanning, remap/fixup completeness, free-list / mark-bit ordering
invariants — the historic sources of checksum corruption under JIT.

Static review only. No builds/tests run. No source modified.

## Summary

The production generational heap (`GenerationalHeap` in `gen_heap.rs`, dispatched
through `VmHeap`/`vm_heap.rs`) is remarkably hardened. Every untrusted-length and
header-deref path I sampled (`alloc_object`/`alloc_array`, `get_field`/`set_field`,
`array_length`, `set_array_element`, `is_object_address`, all four GC walkers) uses
checked arithmetic, bounds checks, and corrupt-header re-sync rather than trusting
header fields. The two collectors — moving Cheney (`collect_garbage_inner`) and the
JIT-active non-moving sweep + selective promotion (`sweep_young_non_moving`) — are
internally consistent: forwarding-pointer install, dirty-card deferral across
compaction, hole-aware linear walks, and atomic `mark_word` replication are all
correct and well-commented. The free-list/mark-bit ordering invariant that caused
the bintrees18 "inconsistent header" false positives (publish dead spans to the
free list *before* the mark-clear re-walk) is correctly enforced (`gen_heap.rs:3819`
must precede `:3866`).

I found **no critical or high-severity correctness/soundness bug** in the
production path. The findings below are medium-and-below robustness/consistency
items plus performance notes. The single most important gap is in **tests**: the
selective-promotion *evacuation* path — the most fragile, historically
corruption-prone code in the crate — has no direct unit or property-test coverage
(all promotion tests and the proptest exercise only the moving Cheney path).

Files **fully read**: `lib.rs`, `arena.rs`, `gen_heap.rs` (production region
1–5436 + test region structure), `old_gen.rs`, `card_table.rs` (core + signatures),
`reference.rs` (signatures + relocation/clearing), `vm_heap.rs` (signatures + GPU
drain + load_and_forward). Files **sampled** (structure + spot-reads):
`heap.rs` (legacy semi-space `Heap`, not the prod backend), `metaspace.rs`,
`tlab.rs`, `satb.rs`, `region.rs`, `g1.rs`, `zgc*.rs`, `concurrent_mark.rs`,
`compact_header.rs`, `numa.rs`, `class_unloading.rs`. Tests: `phase_h_integration.rs`,
`proptest_graph.rs`, `non_moving_sweep_when_jit_active` + S29/S26 suites read fully.

## Bugs

### B1 (low) — `OldGen::scan_region` panics on a corrupt array header instead of re-syncing
`gc/src/old_gen.rs:484-485` (and the identical site in `scan_region_filtered`,
`gc/src/old_gen.rs:443-444`):
```rust
HEADER_SIZE + array_data_size(header.array_length as usize, header.element_type)
    .expect("array_data_size overflow in old_gen scan")
```
Every *young*-side linear walker routes array sizing through
`gen_object_total_size` (`gen_heap.rs:5178`), which returns `0` on a malformed
`array_length` so the caller gracefully breaks/re-syncs. The old-gen `scan_region`
path instead `.expect()`s — a malformed `array_length` in an old-gen header (e.g.
from a JIT miscompile, or a stale word mis-parsed mid-scan during major GC) would
**panic the whole VM** rather than stopping the walk. The very next two lines
(`total_size < HEADER_SIZE || offset + total_size > end_offset → break`) are the
intended graceful handler, but the panic fires first and they never run.

Practical reachability is **low on 64-bit**: `array_data_size` only returns `Err`
on `usize` overflow, which needs ≈ `usize::MAX/8` elements; a `u32` `array_length`
caps at ~4.3 B elements (~34 GB) and cannot overflow `usize`. So on the 64-bit
target this is effectively unreachable today and is a latent inconsistency (reachable
on 32-bit). Fix: mirror `gen_object_total_size` — `match … { Ok(d) => …, Err(_) =>
break }` — so old-gen scanning is as corruption-tolerant as young-gen scanning.

## Vulnerabilities

No memory-safety vulnerability found in the reviewed production path. The crate's
threat surface (classfile/JIT-produced object headers) is consistently validated
before any header deref:

- `is_object_address`/`is_heap_addr` (`gen_heap.rs:812`, `:886`) reject
  non-aligned, out-of-region, and implausible-kind/slot/length headers before
  constructing an `ObjectRef`.
- All four GC walkers (`collect_garbage_inner`, `sweep_young_non_moving`,
  `mark_young_to_old_refs`, `fixup_young_old_refs`, `OldGen::scan_region`) cap
  `num_slots`/`array_length`, check `total_size < HEADER_SIZE`, and check
  `cursor + total_size <= used` before indexing.
- `forward_object_impl` reads the header field-by-field (avoiding a non-atomic
  read of the embedded `AtomicU64`), validates the forwarding pointer is non-null
  and 8-aligned, and verifies the computed extent `fits_in_arena` before copying.
- Allocation size math is `checked_mul`/`checked_add` with hard-abort on overflow
  (`alloc_object` `:445`, `alloc_array` `:567`), so an attacker-controlled field
  count / array length cannot wrap a size.

### V1 (low / informational) — non-atomic bulk copy of an `AtomicU64` header field in the selective-promotion evacuator
`gc/src/gen_heap.rs:3177` copies the whole object including the header via
`std::ptr::copy_nonoverlapping(src, dst, total_size)`, then re-replicates
`mark_word` atomically at `:3181-3188`. The moving path (`forward_object_impl`,
`:4350-4356`) was deliberately rewritten to *avoid* exactly this — a bulk
read of a struct containing `AtomicU64` is a non-atomic read of an atomic location
(technically UB) even though the value is immediately overwritten by the atomic
store. Under STW with no concurrent mutator this is benign, but it is an
inconsistency with the documented fix on the moving path and should copy the body
excluding `mark_word` (or read fields individually) for the same reason. Not
exploitable; flagged for soundness hygiene before open-sourcing.

## Stubs and Unimplemented

No `unimplemented!`/`todo!`/`NotImplemented`/synthetic-fake-value stubs exist in the
crate (grep returned zero). The items below are *honestly-labelled design stubs*
that degrade to a correct single-path behavior — none fake Java-visible behavior, so
none violate the no-synthetic-stubs policy. Reporting per brief.

- **NUMA single-arena fallback** — `gen_heap.rs:4162-4175` (`try_alloc_young`
  numa hint), `heap.rs:60-86` (`Heap` num_numa_nodes field), `numa.rs`. The NUMA
  topology is probed and logged but allocation always funnels through one shared
  `young_from`/`from_space`; multi-arena partitioning is a documented TODO
  (`heap.rs:71-81`). Correct, just not yet a perf win on multi-node hosts.
- **`Arena::reset_no_zero`** — `arena.rs:245-249`: `#[allow(dead_code)]`, retained
  unused because the conservative root scanner can deref any aligned in-capacity
  address. Honestly gated; safe.
- **ZGC backend** — `zgc.rs` / `zgc_concurrent.rs` are `#[cfg(feature = "zgc")]`
  and per `lib.rs:50-60` have no in-workspace consumer (page-storage simulation).
  Not in the default build; not a production stub.
- **Selective-promotion completeness** — `sweep_young_non_moving` stops evacuating
  when old gen fills (`gen_heap.rs:3205` `old_full = true`), leaving the remainder
  in young ("correctness over completeness"). Correct degradation, not a stub.

## Performance

### P1 (medium) — young free-list `alloc` is an O(n) linear best-fit scan
`gc/src/arena.rs:72-121`: `Arena::alloc` scans the entire `free_list` Vec on every
allocation when the list is non-empty. After a non-moving sweep the list can hold
hundreds of thousands of Node-sized holes (bintrees18). This is *mitigated* by the
post-sweep coalescing pass (`gen_heap.rs:3835-3853`) that collapses adjacent holes,
and by the in-place front-shrink fast path (`arena.rs:95-101`) for the hot
8-aligned/front-of-span case. But the general case is still O(blocks). A segregated
free list (as `OldGen` already uses, `old_gen.rs:202`) would make this O(1)
worst-case and remove the dependence on coalescing always firing.

### P2 (low) — `is_object_address` / `is_heap_addr` acquire three arena mutexes per call
`gen_heap.rs:828-830`, `:891-893` lock `young_from`, `young_to`, and `old_gen`
sequentially for a single containment test. Called per ambiguous slot during
conservative root scanning, this is 3 lock acquire/release per candidate word.
Containment is a pure pointer-range test; caching the three `(base,end)` ranges
(as the card table already caches old-gen bounds for the write barrier,
`gen_heap.rs:1528-1539`) would make this lock-free.

### P3 (low) — `is_humongous` locks `young_from` on every allocation
`gen_heap.rs:705-711`: each `alloc_array`/`try_alloc_array` re-reads
`self.young_from.lock().capacity()` to recompute the humongous threshold. Capacity
changes only at GC-time expansion; caching it in an `AtomicUsize` updated on
`grow()` would remove a lock from the array-alloc fast path.

### P4 (low) — `walk_objects()` allocates a full Vec of every old-gen object
`old_gen.rs:337-362` rebuilds and sorts the whole free list and materializes a
`Vec<(ptr,size)>` of *all* live old-gen objects. The minor-GC dirty-card path was
already optimized to `walk_objects_in_card_ranges` (`:384`), but the
diagnostic/seedhunt paths in `collect_garbage_inner` (`:1921`, `:2639`, `:2779`)
and `CRATONVM_SP_VERIFY` (`:3371`) still call the full `walk_objects`. Those are
behind debug env gates, so production is unaffected, but the major-GC mark
(`major_gc` → `compact` → `walk_objects` at `old_gen.rs:524`) does pay it every
major cycle (inherent to mark-compact; noting for completeness).

### P5 (low) — diagnostic env-var lookups on near-hot paths
Several `std::env::var_os(...)` calls sit just inside collector entry points
(`collect_garbage_inner` reads `CRATONVM_DBG_FORCE_MOVING` + `CRATONVM_SHADOW_STACK`
at `:1882`/`:1897` every cycle; `sweep_young_non_moving` reads
`CRATONVM_NO_SELECTIVE_PROMOTE` at `:3119` every cycle). These run once per GC (not
per object) so the cost is small, but the codebase already established the
`OnceLock`-cached pattern (`gcw_enabled` at `:5059`, `seedhunt_enabled`); applying
it to the per-cycle gates would remove the repeated allocation/syscall.

## Tests

**Best-estimate coverage of the `gc` crate: ~62%.** Does **not** plausibly reach 85%.

Basis (by area):

- **Well covered (any tests → good):** `arena.rs` (9 inline tests incl. overflow,
  alignment, reset), `card_table.rs` (~40 inline tests incl. cross-thread table
  scoping, out-of-range, drain dedup), `old_gen.rs` (alloc/free/compact/walk +
  fragmentation), `reference.rs` (soft/weak/phantom/cleaner/finalizer discovery,
  clearing, `update_after_gc` relocation, LRU rebuild), and the **moving Cheney
  path** in `gen_heap.rs`: the S29 suite (cross-gen old↔young chains, card-table
  multiple-dirty / overwrite, 1000-object random graph, promotion+barrier) and S26
  compaction suite (fragmentation, internal-ref update, pointer-map, forwarding
  clear) are thorough. `proptest_graph.rs` model-checks reachability preservation
  across random op sequences. `phase_h_integration.rs` covers promotion stats,
  reference semantics, CAS stress, region selection.

- **Under/Un-covered (the risk):**
  1. **Selective-promotion evacuation (`sweep_young_non_moving` lines 3079–3413)
     has NO direct test.** `non_moving_sweep_when_jit_active` (`:5814`) enters
     quiescence but uses fresh `gc_age=0` objects, so the `aged` gate (`:3172`)
     is never satisfied and **zero objects evacuate** — the `evac_map` build,
     forwarding install, 3-stage fixup (3a young-survivor refs, 3b evacuated-copy
     refs/new old→young cards, 3c dirty-card-seed rewrite), and the
     `is_forwarded`→zero reclaim branch (`:3766`) are all untested. Every
     historical checksum-corruption bug lived in this exact code.
  2. **All promotion tests exercise the moving path, not the non-moving one.**
     `promote_to_old` (`:6709`) and the ~20 `for _ in 0..PROMOTION_AGE` loops run
     `collect_garbage` *without* `quiescence::enter`, so they tenure via Cheney.
     The proptest (`proptest_graph.rs:174`) also never enters quiescence.
  3. **`major_gc` cross-gen fixup after compaction** is only indirectly hit; no
     test forces a young→old reference, a major GC that relocates the old target,
     and then asserts the young referrer was fixed up via
     `fixup_young_old_refs` + the `compact_map` chaining at `:2731`.
  4. **Corrupt-header re-sync paths** (`gen_heap.rs:3719` sweep re-sync;
     `gen_object_total_size` corruption returns) have no test that injects a
     bad header and asserts recovery rather than crash.
  5. **No coverage of `OldGen::scan_region` corrupt-array panic (B1).**

**Most important missing tests (priority order):**

1. JIT-active selective-promotion round-trip: enter quiescence, allocate a deep
   tree, run ≥`PROMOTION_AGE` non-moving sweeps so interior nodes age and
   evacuate to old gen, then assert (a) every parent→child edge still resolves to
   the correct child, (b) `evac_map` is non-empty and addresses are remapped,
   (c) new old→young cards were dirtied, (d) a subsequent sweep doesn't corrupt.
   This is the regression test that would have caught the entire bt18 saga.
2. Property test variant that randomly enters quiescence so the proptest model
   checker also covers the non-moving + selective path (not just Cheney).
3. Major-GC-after-promotion cross-gen fixup test (gap #3 above).
4. Corrupt-header injection tests for the sweep re-sync and old-gen scan
   (gaps #4/#5), asserting graceful recovery, not panic — this would surface B1.
5. Old→young ref-array (not just field) selective-promotion fixup, matching the
   ServiceLoader/ArrayList regression class called out in `gen_heap.rs:2578`.

## Feature Suggestions

1. **Segregated young free list.** Port `OldGen`'s bucketed best-fit allocator
   (`old_gen.rs:192-269`) to `Arena` to make post-sweep allocation O(1) worst-case
   and remove the bintrees18 dependence on the coalescing pass always running (P1).
2. **Cached arena-range table for conservative scanning.** A lock-free
   `[(base,end); 3]` snapshot refreshed at GC boundaries, used by
   `is_object_address`/`is_heap_addr`/`is_humongous`, eliminating per-slot mutex
   traffic during root scanning (P2/P3).
3. **Unify all linear heap walkers behind one corruption-tolerant iterator.**
   There are ~7 near-identical hole-skipping `while cursor < used { … }` walks
   across `gen_heap.rs` and `old_gen.rs`, each re-implementing the size/sanity
   checks (and `OldGen::scan_region` diverges into a panic, B1). A single
   `HeapWalker` that yields validated `(ptr, header, size)` and centralizes the
   corrupt-header policy would remove the divergence and the duplication.
4. **Promote selective-promotion correctness to a runtime invariant check.**
   Wire the existing `CRATONVM_SP_VERIFY` aliasing/missed-fixup detector
   (`gen_heap.rs:3357`) into a `debug_assert!`-gated post-sweep check so any future
   regression in the evacuation fixup fails CI instead of producing a wrong
   checksum silently.
5. **Bounded GPU-critical drain.** `wait_for_gpu_critical_drain`
   (`vm_heap.rs:102-127`) spins/yields forever on a leaked token (only *warns*
   after the deadline). Add a hard ceiling that aborts with a diagnostic (or forces
   the collection) so a leaked `SafepointToken` can't wedge the collector
   indefinitely. (gpu-offload feature only.)
6. **Stats for non-moving sweeps.** `sweep_young_non_moving` bumps
   `minor_gc_count` but not `bytes_promoted`/`objects_promoted` even when selective
   promotion evacuates (the moving path does, `:2862-2874`). Plumbing the
   `evac_map` counts into `HeapStats` would make JFR/observability accurate under
   JIT-active workloads (the common production case).

## Files sampled vs fully read

- **Fully read (production logic):** `lib.rs`, `arena.rs`, `gen_heap.rs`
  (allocation + both collectors + all fixup/walk helpers, lines 442–5300; test
  region inventoried), `old_gen.rs`, `card_table.rs` (mark/clear/drain core),
  `reference.rs` (relocation + clearing + signatures), `vm_heap.rs` (dispatch +
  GPU drain + forwarding), plus the gc-crate test files
  `phase_h_integration.rs`, `proptest_graph.rs`, and the gen_heap inline test
  suite (S29/S26/non-moving).
- **Sampled (structure + targeted spot-reads):** `heap.rs` (legacy `Heap`
  semi-space — alloc-overflow `expect`s confirmed deliberate; not the prod
  backend), `metaspace.rs`, `tlab.rs`, `satb.rs`, `region.rs`, `g1.rs`,
  `g1_concurrent.rs`, `concurrent_mark.rs`, `compact_header.rs`, `numa.rs`,
  `class_unloading.rs`, `mark_bitmap.rs`, `compressed_oops.rs`, `zgc.rs`,
  `zgc_concurrent.rs`, `gc.rs`, `gc_quiescence.rs`, `shadow_stack.rs`,
  `collector.rs`, and the remaining test files `leak_soak.rs`, `loom_satb.rs`,
  `wp1_10_reference.rs`.
