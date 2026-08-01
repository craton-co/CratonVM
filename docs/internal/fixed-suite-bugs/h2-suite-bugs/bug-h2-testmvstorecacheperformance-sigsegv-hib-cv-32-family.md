# `TestMVStoreCachePerformance` `SIGSEGV` after a burst of heap-integrity guard warnings — HIB-CV-32 family

> **✅ FIXED (2026-07-31, branch `fix/h2-mvstore-cache-segv-20260731`).**
> Two commits, one root cause in two places:
> * `c0d09e2451` — post-GC reference processing wrote through stale
>   old-generation addresses.
> * `b86945eafe` — the same old-gen blind spot in `is_stale_young`, the guard
>   the cleared / enqueue / finalize / cleaner loops use.
>
> `org.h2.test.store.TestMVStoreCachePerformance` now runs to completion —
> all six `testCache` rounds, up to 100 concurrent reader threads — exit 0,
> with zero `gen_heap::set_field`/`get_field` out-of-bounds guard hits and
> zero `read_slot: corrupt Value cell` reports, under **both** `--nojit` and
> the JIT.
>
> The sibling finding this doc cross-referenced (`TestGetGeneratedKeys`)
> turned out to be an unrelated defect and **not heap corruption at all** —
> a wrapper `equals` native reading field 0 of a `String[]`. It was
> root-caused and fixed independently, on another branch, while this one was
> out: `7656ad39`, retired as
> [bug-h2-testgetgeneratedkeys-wrapper-equals-missing-type-check-FIXED.md](bug-h2-testgetgeneratedkeys-wrapper-equals-missing-type-check-FIXED.md).
> This branch adds `967abc2546` on top of it, closing two residuals its
> class-id gate left open (`Foo` vs `Foo[]` compare equal by class id; an
> undecodable slot compared EQUAL).

## Original symptom

```
[WARN] gen_heap::set_field: out-of-bounds field write dropped (caller used
  slot index past receiver's layout ...) obj=0x200274cf3a8 index=0
  num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
  real_field_count=Some(0) value=Object(Some(ObjectRef { ptr: ... }))
  ... (repeats)

[ERROR] gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)
  ... slot=0x20010419010 raw0="0x0000003436363834" raw1="0x0102080100000000"

# SIGSEGV at pc=..., addr=0x0
```

## Root cause

`VmHeap::is_addr_live` answers `true` for **any** address inside the
old-generation arena:

```rust
VmHeap::Generational(h) => h.is_old_gen_addr(addr) || h.is_live_young_survivor(addr)
```

`OldGen::contains` is a pure bounds check over the arena's whole backing
store. That is correct for a *minor* cycle — old gen is not touched, so
nothing moved — and `gc_quiescence.rs` documents it as such. It stops being
true the moment the **same cycle** reclaims old-gen storage:

* the mark-compact `major_gc` (Phase 5, fires at 75% old-gen occupancy or on
  an explicit `System.gc()`) slides live objects down over the dead ones and
  **zeroes the freed tail**;
* the in-place `sweep_old_gen_non_moving` (the JIT-active path) returns dead
  blocks to the free list.

Post-GC reference processing used `is_addr_live` as its survival proof, in
three places (`is_marked` → `process_references`, the weak/phantom
referent-restore pass, and `remove_collected`). So a dead old-gen
`Reference` — or referent — was judged "survived":

1. `remove_collected` never pruned the entry, so it accumulated **forever**;
2. every subsequent collection's restore pass wrote a referent pointer
   *through the stale address*.

When the recycled memory was the zeroed compaction tail the write hit the
`gen_heap::set_field` out-of-bounds guard (`class_id=ClassId(0)`,
`num_slots=0` — exactly the reported shape) and was dropped. When it was a
live object slid onto that address, the write landed **silently** on a real
reference field. That is the `SIGSEGV` and the `corrupt Value cell` reports:
a live object's slot holding a pointer that belongs to something else.

`is_stale_young` — `VmHeap::pre_gc_addr_did_not_survive`, the guard the
cleared / enqueue / finalize / cleaner loops use — had the identical blind
spot: for the Generational heap it only rejects **young** addresses absent
from the pointer map, and answers "survived" for every old-gen address. The
finalize and cleaner loops hand that address to `run_finalizers` /
`run_cleaner_actions`, which **invoke Java methods on it**.

### Measured

Instrumenting the restore site (`CRATONVM_DBG_WEAKREF_STALE`, temporary)
printed, for every one of the first 40 stale restores:

```
[weakref-stale #0] ref_old=0x20010143658 ref_new=0x20010143658
  ref_in_map=false ref_oldgen=true ref_youngsurv=false ref_nf=0 |
  referent_old=0x20010143548 referent_new=0x20010143548
  referent_in_map=false referent_oldgen=true referent_youngsurv=false
  referent_nf=0 | map_len=234865 active_len=86266
```

Every one: an **old-gen** address, **absent from the pointer map**, admitted
solely by `is_old_gen_addr`, resolving to a receiver with **0 fields**.
`active_len=86266` — 86 k "active" weak/phantom entries, essentially all
stale, because none had ever been prunable. Per-run guard-caught corrupt
writes: **98,559** (`--nojit`) and **64,960** (JIT on).

## The fix

The collector now publishes a survival proof for exactly the addresses
reference processing writes through, and reference processing requires it.

| Change | File |
| --- | --- |
| `ReferenceProcessor::all_tracked_addrs` — publish **every** tracked address (soft/weak/phantom/cleaner/finalizer reference object, referent, queue) as watched before each collection, not just the weak/phantom pairs' | `gc/src/reference.rs` |
| `sweep_old_gen_non_moving` returns identity `pointer_map` entries for watched survivors (it returned an empty map), mirroring what `OldGen::compact` already did for watched *stationary* survivors | `gc/src/gen_heap.rs` |
| `gc_quiescence::OLD_GEN_RECLAIMED` — did the cycle in flight reclaim old-gen storage? | `gc/src/gc_quiescence.rs` |
| `VmHeap::watched_pre_gc_addr_survived` — exact verdict: map membership, or an old-gen address in a cycle that did **not** reclaim old gen | `gc/src/vm_heap.rs` |
| `is_marked`, both halves of the restore lookup, and `is_stale_young` use it; the restore write gains the `num_fields < 2` guard every other write site already had | `vm/src/runtime/interpreter.rs` |

`is_stale_young` **ORs** the two verdicts rather than replacing one with the
other, so the young rule stays exactly as strict as the `bc math-ec 0x4` fix
made it and the change can only skip more writes, never fewer.

`CRATONVM_NO_EXACT_REFPROC_SURVIVAL=1` restores the permissive predicate
(bisection escape hatch).

### Why the "watched" set makes it exact

`OldGen::compact` already emitted an identity `pointer_map` entry for a
*watched* object that happened not to move (the RandomizedContext
`WeakHashMap<Thread,…>` fix). Publishing every tracked address as watched,
and adding the same emission to the in-place sweep, makes "absent from the
pointer map" a **complete** death proof for precisely the address set
reference processing dereferences — and for nothing else, so no other
consumer's behaviour changes.

## Verification

`org.h2.test.store.TestMVStoreCachePerformance`, `--Xmx 1g`, JDK 25, Azure
box. "Anomalies" = `gen_heap::set_field`/`get_field` out-of-bounds guard
hits + `read_slot: corrupt Value cell` reports + fatal-error banners.

**Before** — 6 for 6 failures, across three separate builds and both arms:

| build | arm | outcome | corrupt writes |
| --- | --- | --- | --- |
| `origin/dev` fat-LTO | `--nojit` | `MVStoreException: chunk is null`, exit 1 @786 s | 98,559 |
| `origin/dev` fat-LTO | JIT | `ClassCastException: Integer→String`, exit 1 @1397 s | 0 (silent variant) |
| + diagnostics | `--nojit` | **SIGSEGV**, exit 139 @1038 s | 79,023 |
| + diagnostics | JIT | `ClassCastException`, exit 1 @1302 s | 64,960 |
| + `CRATONVM_DBG_OOBFIELD` | `--nojit` | exit 1 @951 s | 98,559 |
| + `CRATONVM_DBG_OOBFIELD` | JIT | exit 1 @1130 s | 0 (silent variant) |

Every run died in round 3 or 4 (`testCache(10, …)`); none ever reached the
100-thread rounds.

**After** — complete runs, all six rounds through `testCache(100, "cache:")`:

| build | arm | outcome | anomalies |
| --- | --- | --- | --- |
| both fixes | `--nojit` | **exit 0**, 2667 s | 0 |
| both fixes | JIT | **exit 0**, 2679 s | 0 |
| both fixes (repeat) | `--nojit` | **exit 0**, 3038 s | 0 |
| + merged `origin/dev` | `--nojit` | **exit 0**, 2871 s | 0 |
| + merged `origin/dev` | JIT | **exit 0**, 3160 s | 0 |
| + merged (repeat) | `--nojit` | **exit 0**, 3867 s | 0 |
| final integrated tree | `--nojit` | **exit 0**, 2687 s | 0 |

Six clean completions where there had been none, plus
`org.h2.test.jdbc.TestGetGeneratedKeys` exit 0 with 0 corrupt-cell reports
in both arms.

Intermediate stage, recorded honestly: with only the FIRST commit in, the
corrupt-write burst was already gone (98,559 → 0 and 64,960 → 0) but the
process still `SIGSEGV`'d — the second commit (`is_stale_young`) is what
made the workload complete. Both are needed.

**Three post-fix runs that did not reach exit 0, none showing this
defect's signature** (all three: **zero** guard hits, zero corrupt cells,
no SIGSEGV):

* A JIT repeat on the **pre-merge** build failed at 2269 s with
  `ClassCastException: java.lang.Object cannot be cast to
  org.h2.mvstore.Page` out of `FileStore.readPageFromCache`. Not reproduced
  on the merged build. If `TestMVStoreCachePerformance` regresses again,
  this is the shape to look for — and it is NOT what this doc describes.
* A JIT repeat was killed by the **host's** OOM killer (exit 137) at load
  average 379 with 0 GB free, log clean to the last line. Environment
  casualty, not a VM result.
* The JIT arm on the final integrated tree ran into a **catchable**
  `java.lang.OutOfMemoryError` ("native allocation could not be satisfied",
  535 elements) in round 4, after ~20 minutes of continuous
  `[moving-young] fallback: xt-helper-window-conservative-scan` — i.e. the
  young generation running the fragmenting non-moving sweep back-to-back
  under 10–100 threads at `--Xmx 1g`. That is a
  fragmentation/throughput problem in the moving-young coverage machinery,
  orthogonal to reference-processing correctness, and it degrades safely
  (a catchable Java exception, not corruption). See the observations
  section below.

Unit tests: 877 `cratonvm-gc`, 3153 `cratonvm-native-builtins`, 2326
`cratonvm-vm`, 453 `cratonvm-types` — all pass. New regressions:

* `gen_heap::tests::in_place_old_sweep_proves_watched_survivors_and_dead_ones`
* `gen_heap::tests::explicit_full_gc_marks_dead_old_gen_watched_addr_as_not_surviving`
* `reference::tests::all_tracked_addrs_covers_every_reference_kind`

## The three "suggested next steps" the original doc left open

1. **Is it JIT-only?** No. It reproduces under **both** `--nojit` and the
   JIT, with the same guard signature and comparable volumes (98,559 vs
   64,960 corrupt writes). The `--nojit` arm is where the hard `SIGSEGV`
   showed up most reliably; the JIT arm more often degraded to a
   `ClassCastException`. Which face you get is just which slot the stale
   write happened to clobber.
2. **The `hs_err_pid*.log`.** Captured, and it is *not* where the answer
   was. CratonVM's signal handler writes a deliberately minimal report
   (`# (truncated: full report requires allocator, unsafe in signal
   handler)`), and the faulting frame moved every run — `alloc_object`,
   `SynchronizedMethodGuard::drop`, `young_mark::drain_parallel` — which is
   the signature of heap corruption rather than a single bad call site. The
   diagnostic that actually located it was the existing `RESID-DIAG WRITE`
   backtrace on the `num_slots == 0` write guard, which named
   `process_references_after_gc` on the very first occurrence.
3. **Is it HIB-CV-32's `promotion_oom_risk` mechanism?** No. That fix
   (`dev@c9258e17`) is about which *collector* runs. This is a
   **consumer-side** defect: post-GC reference processing trusting an
   address-range test as a liveness proof. Same symptom family, same
   defensive guards firing, different root cause. Both of HIB-CV-32's halves
   remain correct and untouched.

## Observations noted during this work — NOT part of this defect

* The `--nojit` arm printed `STW cross-thread JIT takeover is still waiting
  for cooperative mutators rounds=64` under 10–100 mutator threads before one
  of the intermediate crashes. It did not recur once both halves of the fix
  were in.
* The JIT arm logs a steady stream of `[moving-young] fallback` warnings
  (`unregistered-jit-frame-on-stack`, `xt-helper-window-conservative-scan`,
  `missing-exact-rbp`, `active-safepoint-map-incomplete`,
  `cross-thread-jit-peer`) — i.e. the young generation is repeatedly NOT a
  copying collector under this workload. That is the documented, safe
  diversion, and it is orthogonal to this defect: the `--nojit` arm takes
  none of those fallbacks and reproduced the bug just as hard.

  It is not free, though. On the final integrated tree the JIT arm spent
  ~20 minutes in back-to-back `xt-helper-window-conservative-scan`
  fallbacks and then raised a catchable `OutOfMemoryError` on a 535-element
  native allocation at `--Xmx 1g` — the fragmenting free-list allocator
  never getting a compaction while 10–100 reader threads keep a helper
  window open. That is a **separate, open** throughput/fragmentation issue
  in the moving-young coverage machinery; it degrades safely and shows
  none of this defect's signatures (zero guard hits, zero corrupt cells).
  A good starting point is why `xt-helper-window` can stay latched for
  minutes at a time under this thread count.

## Follow-up: the fragmentation half is fixed (`7303483521`, 2026-08-01)

The `OutOfMemoryError` recorded above — "a catchable `OutOfMemoryError`
after ~20 minutes of back-to-back `xt-helper-window-conservative-scan`
fallbacks" — was root-caused to the old generation's free list never
coalescing.

`OldGen::free` defers coalescing to `compact`, which only runs on the
moving path's Phase 5. Under conservative JIT roots the old generation is
reclaimed IN PLACE by `old_gen_gc(compact = false)`, which never compacts,
so every reclaimed object became a permanently isolated free block: free
BYTES stayed high while the LARGEST block collapsed toward one object, and
the fallible native-side allocator (which deliberately cannot GC-and-retry)
reported a spurious OOM on a mostly-free generation. The young generation
has had exactly this coalescer since the bintrees18 allocation cliff; old
gen never got the counterpart.

The magnitude, now that `[GC] oldgen_coalesce: calls=N blocks_merged=M` is
reported in the GC summary:

| run | heap | result | blocks merged in ONE sweep |
| --- | --- | --- | --- |
| this workload, JIT on | `--Xmx 1g` | exit 0 | **40,645** |
| this workload, JIT on | `--Xmx 256m` | direct-buffer OOM (unrelated) | **374,573** |

So the free list really was reaching hundreds of thousands of isolated
blocks, and the fix demonstrably fires in the shipping configuration.
Note this also removes an O(n) best-fit scan per old-gen allocation, the
same throughput cliff the young-side coalescer was added for.

**Superseding this doc's earlier caveat:** the commit message for
`7303483521` says old gen "never reached the 75% occupancy that triggers
the in-place sweep at all" on a quiet host. That was true of the runs
available when it was written; the confirmation run on the integrated tree
(`major=2`, `calls=1`, 40,645 blocks merged, exit 0) shows the regime IS
reached at the default heap size. What remains unproven is only the
counterfactual — the original OOM itself never recurred, so "this specific
OOM is gone" is inference from the mechanism, not a before/after
observation.

## Repro (historical)

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.store.TestMVStoreCachePerformance
```

Reproduced 6/6 before the fix across both arms and three different builds;
5 clean completions after (see Verification for the one unexplained repeat
failure on the pre-merge build).

## Related

- [`bug-h2-testgetgeneratedkeys-wrapper-equals-missing-type-check-FIXED.md`](bug-h2-testgetgeneratedkeys-wrapper-equals-missing-type-check-FIXED.md)
  — the same *guard* firing for an unrelated *cause*. Read together, the two
  are the argument for never filing a `corrupt Value cell` report under a
  heap-corruption family before dumping the cell's HOLDER
  (`CRATONVM_DBG_CELLCORRUPT=1`): here the heap really was corrupt, there it
  never was, and the diagnostic text is identical.
- `bug-h2-testdiskfull-classid0-corruption-segv-cce.md` (retired into this directory 2026-08-01)
  — the `TestDiskFull` `AbstractMethodError` the original write-up flagged as
  "possibly related, not confirmed" now has its own doc. It is **not** closed
  by this fix and was not investigated here.
- `../hibernate/run-20260622/HIB-CV-31-abstractmethoderror-onflush-root-cause.md`
  and `../hibernate/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md`
  — the original family; their own root causes are unrelated to this one.
