# References, metaspace, class unloading, NUMA

Audit + fixes, 2026-07-26. Scope: `gc/src/reference.rs`, `gc/src/class_unloading.rs`,
`gc/src/metaspace.rs`, `gc/src/vm_heap.rs`, `gc/src/numa.rs`.

Base: `arch/wave1-integration-20260726` merged at `7e12b8772` (`classloading/src/type_maps.rs`
present, `gc/src/g1.rs` contains `retain_sources`).

## Summary of findings

| # | Finding | Status |
|---|---|---|
| 1 | Reference strengths are ordered correctly, but phases 3-4 used raw marking, so a `Cleaner` could free native memory for a still-soft-reachable object | **FIXED** in `reference.rs` |
| 2 | The `SoftReference` LRU policy could never clear anything on either VM path | **FIXED** in `reference.rs`; pressure input still constant, cross-owner request below |
| 3 | Reference processing is O(all registered refs) per collection, under a global L7 mutex, inside STW | Characterised, not changed |
| 4 | `gc/src/class_unloading.rs` is not the production class unloader — it has no caller at all | Documented; two unbounded tables inside it fixed |
| 5 | `gc/src/metaspace.rs` is a model with no caller; real metaspace is unbounded and only estimated | Documented; registry leak fixed |
| 6 | `numa.rs` performs detection only — no NUMA placement happens anywhere | Documented plainly |

---

## 1. Reference processing correctness

### Ordering and inter-phase reachability — was mostly right

`ReferenceProcessor::process_references_with_finalizer_trace` runs
soft → weak → final → phantom, and it already recomputed reachability between
phases 1 and 2:

* **Phase 0** builds `finalizer_live`, the closure from the referents of
  finalizer references about to be enqueued. Phase 1 (soft) honours it, so a
  soft ref to an object a finalizer can still reach is not cleared out from
  under that finalizer.
* Between phases 1 and 2 it builds `soft_live`, the closure from the soft
  referents Phase 1 chose to retain, and folds it into weak clearing. That is
  the JLS `strong > soft > weak` ordering, correctly implemented.
* With a caller-supplied `trace_from` these are full transitive closures; with
  `None` (both VM call sites) they degrade to the direct referents — a
  documented residual gap, pinned by the existing test
  `v18_residual_gap_transitive_without_tracer`.

### The bug: phases 3 and 4 used the raw mark snapshot

`process_final_refs` and `process_phantom_refs` were called with the unmodified
`is_marked`, justified by a comment claiming phantom reachability should be
"unaffected by the resurrection closure". That has it backwards — the reason
phantom is processed *last* is that `java.lang.ref` defines an object as
phantom-reachable only once it is neither strongly, softly nor weakly reachable
**and it has already been finalized**. Consequences of the old behaviour:

* A `Cleaner` fired for an object Phase 1 had just decided to **retain** through
  a surviving `SoftReference`. The JDK's most important cleaner is
  `DirectByteBuffer`'s: firing it frees the native allocation backing a buffer
  the application can still obtain from `SoftReference.get()`. That is a
  use-after-free of native memory, not a spec nit.
* `finalize()` was scheduled for an object that was still softly reachable.
* A phantom was enqueued for an object whose `finalize()` had not run yet, so a
  resurrecting finalizer raced an already-delivered phantom notification.

**Fix** (`reference.rs`, in `process_references_with_finalizer_trace`): both
phases now fold in the closures phases 1-2 already computed, with a deliberate
asymmetry:

* **finalizer** entries fold in `soft_live` **only**. Soft reachability blocks
  finalization; finalizer reachability must not, or two mutually-reachable
  finalizable objects would block each other forever. HotSpot enqueues both in
  the same cycle. Pinned by `mutually_reachable_finalizers_both_enqueued`.
* **cleaner** and **phantom** entries fold in **both**. `Cleaner` is a
  `PhantomReference` subclass, so it takes the phantom rule, and "has been
  finalized" is part of that rule.

The change is strictly *less* clearing/enqueueing, so it can only over-retain,
never free early. Over-retention from the finalizer closure is bounded at one
collection: once phase 3 flags an entry `enqueued` it stops contributing a root,
and the next cycle delivers the phantom. Pinned by
`phantom_waits_one_cycle_for_finalization`.

New tests: `cleaner_not_fired_while_referent_softly_reachable`,
`finalizer_not_enqueued_while_referent_softly_reachable`,
`phantom_waits_one_cycle_for_finalization`,
`mutually_reachable_finalizers_both_enqueued`,
`unrelated_cleaner_and_phantom_still_fire_with_surviving_soft_ref`.

### `ReferenceQueue` enqueueing — correct, with one deliberate deviation

Enqueueing is once-only per entry (`pending_queues` is drained, not read;
`clear_emitted` / `action_emitted` flags gate the referent-null and cleaner
emissions). That is load-bearing: the header comments record that re-emitting on
every GC corrupted innocent objects through recycled Reference addresses.

The deviation is `ReferenceQueue::enqueue`'s **evict-oldest-on-overflow** policy
at `max_capacity`. Real `ReferenceQueue` is unbounded; silently dropping the
oldest pending reference means an application polling the queue can miss a
notification. It is logged at `debug` and counted in `overflow_count()`, and no
production code constructs a `ReferenceQueue` with a finite capacity today
(the VM tracks queues by address through `pending_queues`, not through this
type), so this is latent rather than live. Left alone; noted here so it is not
mistaken for spec behaviour.

`remove_timeout` / `remove_blocking` spin-wait with `yield_now` and cap at 60 s.
Since nothing in the VM calls them, this is not a live pause-time issue, but they
must not be wired to `ReferenceQueue.remove()` as-is — a Java thread blocked in
`remove()` would burn a core.

### Cost

* **Where:** inside the STW pause on all three drivers — `process_references_after_gc`
  and `g1_remark_process_references` in `vm/src/runtime/interpreter.rs`, and
  `ZgcRealHeap::process_references` in `gc/src/zgc.rs`.
* **Lock:** the single global `ref_processor` mutex, L7 in
  `vm/src/runtime/lock_order.rs`, held across the whole of
  `process_references_after_gc`. Because the world is stopped, it serialises
  mutator-side `discover_reference` / `touch_soft_reference` against each other,
  not against the GC — so the L7 position is not itself a pause-time cost.
* **Complexity: O(registered references), not O(live references).** Phases 2-4
  are flat scans; an already-cleared or already-enqueued entry is skipped but
  still visited. Only phase 1 is sublinear, via the `soft_ref_lru_index` BTreeMap
  range over entries idle enough to be candidates.

  The lists *are* bounded — `remove_collected` (called from
  `process_references_after_gc`) drops entries whose `Reference` **object** died
  — so the scans are proportional to live `Reference` objects, not to every
  reference ever created. But a `WeakHashMap`/`ThreadLocal` population with N
  live entries costs an N-entry scan on every collection, forever, even after all
  N referents have been cleared.

  **Improvement sketch (not done):** segregate settled entries. A cleared weak
  ref and an enqueued phantom can never change state again; move them to a `cold`
  vector that phases 2-4 skip entirely and that only `remove_collected` /
  `update_after_gc` walk. That makes the phases O(unsettled) while keeping
  relocation correct. It touches the `soft_ref_addr_index` / `soft_ref_lru_index`
  position invariants, so it needs its own change with its own tests.

  Note the G1 final-remark driver does **not** call `remove_collected` (nothing
  moves in that pause), so on a G1-only workload the lists are pruned solely by
  the post-GC path.

---

## 2. Soft-reference policy

### What it did: nothing, ever

The policy in `process_soft_refs` is HotSpot's `LRUMaxHeapPolicy` —
clear when `idle_ms > SoftRefLRUPolicyMSPerMB * free_heap_MB` — and both of its
inputs were broken at the call site.

The two numbers being compared came from different clocks:

* `last_access_time_ms` on each soft entry is stamped by
  `NativeContext::touch_soft_reference` (`vm/src/vm/vm_exec.rs`) using
  `SystemTime::now()`, i.e. Unix-epoch milliseconds (~1.7e12). It fires from
  `SoftReference.<init>` **and** every `SoftReference.get()`
  (`native-builtins/src/reference.rs`). That wiring is correct and complete.
* `current_time_ms` was a hardcoded **`0`** at both production call sites in
  `vm/src/runtime/interpreter.rs`.

With `current_time_ms == 0` the cutoff is `0.saturating_sub(threshold) == 0`, so
the BTreeMap range selects only entries still at timestamp `0`, and for those
`idle_ms` is also `0` — never greater than the threshold. **No `SoftReference`
could ever be cleared on either VM path.** Soft references behaved exactly like
strong ones, and `OutOfMemoryError` was reached with a heap full of reclaimable
soft-reachable objects. This was observed independently and recorded in
`docs/internal/fixed-suite-bugs/springboot/core39-clusterD-lifecycle-ssl-validation-FIXED.md`
("`process_soft_refs` never ran even once during the whole failing run") but was
read there as a symptom rather than as a dead policy.

Only `gc/src/zgc.rs` passes a coherent `(free_mb, now_ms)` pair, and its own
processor is not the one the VM populates.

### Fix (self-contained, in `reference.rs`)

Rather than depend on a caller-side change in a file this slug does not own, the
processor now tracks the mutator clock itself:

* new field `last_observed_clock_ms`, updated monotonically by
  `touch_soft_reference` **before** any early return, so it learns the real time
  base even from a touch that resolves no entry;
* `process_soft_refs` is driven by `max(current_time_ms, last_observed_clock_ms)`.
  A caller with a coherent clock (ZGC's backend, every unit and integration test)
  already dominates and is bit-for-bit unaffected; a caller passing `0` gets the
  real clock instead of a dead policy;
* `discover_reference` stamps a **soft** entry with `last_observed_clock_ms` at
  creation, mirroring `SoftReference`'s constructor (`this.timestamp = clock`).
  Without this, a soft ref discovered through a path that does not also touch
  would sit at timestamp `0`, look infinitely idle against a wall-clock "now",
  and be cleared on the first collection after the process had been up longer
  than the threshold. It is a no-op for every caller that never touches, so no
  existing test changes.

Tests: `soft_policy_uses_mutator_clock_when_caller_passes_zero`,
`soft_policy_creation_stamp_protects_fresh_ref`,
`soft_policy_caller_clock_dominates_observed_clock`.

### Is it sound under a fragmenting heap? Not yet — one dimension is still constant

The time dimension now works. The **pressure** dimension does not: both call
sites pass a literal `64` for `free_heap_mb`, so the threshold is a fixed
64 seconds of idleness regardless of how full the heap is. The policy cannot
respond to memory pressure at all, and an actively-used cache (touched more often
than every 64 s) still holds its referents right up to `OutOfMemoryError`.

Getting that number right is *harder* here than on HotSpot, for the reason the
brief flags. The default collector does not compact: `moving_young` defaults
false and `gen_heap` fail-closes to a non-moving mark-sweep whenever any thread
holds a live JIT frame — the steady state at a 500-invocation JIT threshold
(compaction's correctness blocker closed 2026-07-26,
`docs/internal/fixed-suite-bugs/app-jvm-bugs/moving-young-gen-drops-jit-held-oops-FIXED.md`,
but moving-young remains opt-in on throughput grounds). Under a
non-moving fragmenting heap, "unused bytes" and "bytes an allocation can actually
obtain" diverge without bound. A policy keyed on unused bytes refuses to clear
soft references precisely while allocation is failing.

`VmHeap::soft_ref_policy_free_mb()` (new, in `gc/src/vm_heap.rs`) computes the
allocatable-biased figure: the smaller of young/eden headroom and old-gen
promotion headroom, capped by whole-heap headroom, rounded **down** so a
sub-megabyte remainder reads as 0 MB (maximum pressure) rather than 1 MB. It is
deliberately pessimistic — under-reporting free space clears soft refs sooner and
costs cache hit rate; over-reporting clears them never and costs the process.

It has no caller yet. See the cross-owner request below.

### Still missing: the last-ditch clear before `OutOfMemoryError`

HotSpot guarantees `OutOfMemoryError` is only thrown after a full GC that clears
**all** soft references (`clear_all_soft_refs`). CratonVM has no such path —
grep for `clear_all_soft`, `always_clear`, `last_ditch` finds nothing. Even with a
correct pressure input, an actively-touched cache can still push the VM to OOM.
Adding the API without a caller would be a default-off landing, so it is filed as
a cross-owner request rather than landed here.

---

## 3. Class unloading and bounded metadata

### The invariant holds on the GC side — but by vacuity, not by discipline

`gc/src/class_unloading.rs` **is not the production class unloader.** A
repo-wide search for `ClassUnloader`, `ClassLoaderHierarchy` and
`class_unloading::` matches only that file and the `pub mod` line in
`gc/src/lib.rs`. Nothing calls `register_loader` / `register_class` /
`register_jit_code`, so every table in it is permanently empty in a running VM.

The real transaction is:

1. `vm/src/runtime/interpreter.rs` — `process_references_after_gc` (post-GC) and
   `g1_remark_process_references` (G1 final remark), both under STW;
2. `native-builtins/src/classloader.rs` — `gc_reconcile_defining_loaders`,
   which retains the `defining_loader_store`, records dead class ids, and
   re-syncs `cratonvm_types::loader_pin`;
3. `vm/src/memory/gc.rs` — `unload_dead_class_metadata`, which prunes statics,
   class locks, field descriptors, class-init waiters, lambda proxies, mirrors
   (both directions), the initiating-resolution cache, vtables, JIT alloc cache,
   profiles, tier state, deopt log, JIT code cache and skip set;
4. `classloading/src/class_manager.rs` — `unload_user_loader`.

**That transaction touches no `gc`-crate table.** Its only effects inside `gc`
are indirect: `ClassStore::remove` calls `unregister_class_layout`, which bumps
`LAYOUT_GENERATION` and thereby invalidates the layout cache in `gc/src/heap.rs`;
and the `loader_pin` / `mirror_pin` / `metadata_pin` registries in `types/` are
re-synced, which `g1.rs`, `gen_heap.rs` and `zgc.rs` read as roots.

So the answer to "after a loader is unloaded, is every per-class side table entry
released?" is: **on the GC side, yes — because the GC owns no live per-class side
table.** That is a weaker guarantee than it sounds, and the audit turned up three
latent violations that would bite the moment anything is wired up.

`ClassIdSlots` / `remove_each` are **not on this branch**. They live on the
sibling worktree branch (`classloading/src/class_manager.rs` here still uses
`FxHashMap<ClassId, Arc<AtomicU8>>` for `init_states`). Their contract — a
per-slot `RwLock<Option<T>>` rather than `OnceLock<T>` specifically so a slot can
be *un*-published, with evicted values dropped after all guards are released — is
the right model, and its own doc warns that clearing the manager's slot does not
invalidate `Arc` clones already handed to downstream caches. Nothing in `gc`
holds such a clone (see below), so the GC side needs no matching `remove_each`.

### Latent violations found and fixed

**(a) `ClassUnloader::code_cache` was append-only.** `unload_classes` phase 3
only flipped `entry.valid = false`; `register_jit_code` only ever pushed. Every
`CodeCacheEntry` ever created — including its owned `method_name: String` —
stayed in the vector forever, and every later unload rescanned all of them.
Fixed: retirement is now a removal (`retain_mut`), so the vector tracks *live*
compiled methods. Two existing tests were updated to assert release rather than
flag-flip.

**(b) `ClassLoaderHierarchy::parents` had no unload coupling.** The type is
unlinked from `ClassUnloader`, and `unload_classes` returned only loader
*names*, which are not unique — so a caller holding both had no way to keep them
in step and `parents` grew one entry per loader forever. Each leaked entry also
lengthens the `is_ancestor` iteration bound. Fixed: `ClassUnloadingResult` now
carries `unloaded_loader_addrs`, and `ClassLoaderHierarchy` gained
`remove_all(&[usize])` plus `len`/`is_empty` so the shrink is assertable.

**(c) Accounting could underflow.** `total_metadata_bytes -= …` and
`total_classes -= …` were plain subtractions that panic in debug on any
accounting drift (e.g. `register_class` against an unknown loader silently drops
the registration without bumping the totals). Both are now `saturating_sub`.

New tests: `repeated_unload_cycles_leave_no_residue` (64 define-heat-drop cycles
must reach a steady state in *every* table — the shape of the checked-in
`class_loader_unload` probe and of any CGLIB/ByteBuddy workload),
`unload_result_reports_loader_addresses`,
`retired_entries_do_not_linger_or_recount`,
`accounting_does_not_underflow_on_unknown_loader`.

A module-level doc now states prominently that this is not the production
unloader and points at the real chain, so nobody wires it in by mistake.

### Other unpruned per-class tables in `gc/`

* `gc/src/compact_header.rs` — `NarrowKlassTable` (`to_narrow` and
  `to_class_id`, both `FxHashMap<u32, u32>` keyed on raw class id, plus a
  monotonic `next_id`) has **no removal path of any kind**: no `remove`,
  `retain`, `prune` or `clear`. Its owner `CompactAllocator` has no caller
  outside its own file, so it is latent, not live. **Not my file** — cross-owner
  note below.
* `gc/src/heap.rs` — the thread-local `OOP_CACHE` holds one
  `Arc<CompactLayout>` per GC-scanning thread. It is generation-guarded
  (`unregister_class_layout` bumps `LAYOUT_GENERATION`), so it can never serve a
  stale layout, and it holds no back-reference to `Class` or `ClassManager` —
  it cannot keep a class alive. The only residue is that the drop is *lazy*: one
  layout allocation per scanning thread survives until that thread next scans a
  different class. Bounded and harmless; noted for completeness. **Not my file.**
* The debug rings (`a2dbg.rs` `LOG`, `gen_heap.rs` `SWEPT_RING`) record class
  ids but are env-gated, fixed-size and cycle-cleared. No obligation.

No other `Arc` in `gc/src` holds per-class metadata; every remaining one is GC
infrastructure (`Arc<SatbQueue>`, `Arc<G1Collector>`, and friends).

---

## 4. Metaspace accounting

**Metaspace is not tracked, not bounded, and does not shrink on unload —
because `gc/src/metaspace.rs` is not wired to anything.**

Nothing constructs a `Metaspace`, `MetaspaceRegistry` or `CompressedClassSpace`.
Outside that file, `Metaspace` matches only JFR event descriptors, accepted-and-
ignored `-XX:` flags, unified-log tag names and the serviceability text report.
The chunk allocator hands out `(chunk_id, offset)` pairs, not pointers; it
reserves no memory and cannot be made to.

What a Java program actually observes as metaspace is an approximation in
`vm/src/vm/vm_init.rs` (search "Metaspace approximation"): live class count times
an estimated per-class overhead. `native-builtins/src/jmx.rs` documents the same
from the `MemoryMXBean` side — there is no per-pool non-heap accounting, so
`-XX:MaxMetaspaceSize` bounds nothing and there is no
`OutOfMemoryError: Metaspace`.

Real class metadata lives in ordinary Rust allocations owned by
`classloading::ClassStore`. It **does** shrink on unload, via
`unload_dead_class_metadata` → `unload_user_loader` (chain in §3), but with no
cap. A proxy-heavy workload is bounded only by loader unloading keeping up.

Fixed inside the model, so it is correct if ever wired:

* `MetaspaceRegistry` had `mark_dead` (flip `is_alive`) as its only unload
  operation and no removal at all, so `loaders` was append-only — one
  `ClassLoaderMetaspace` row with an owned `String` per loader ever created.
  Added `remove_loader` and `prune_dead`, and documented that `mark_dead` alone
  is not a release. Tests: `registry_prune_dead_releases_rows`,
  `registry_remove_loader_returns_row`,
  `registry_bounded_across_repeated_unload_cycles`.
* Added `metaspace_capacity_shrinks_on_loader_unload` to pin that
  `free_loader_metaspace` + `trigger_gc` actually returns capacity (freeing alone
  only flags chunks; the GC returns the bytes).

Note `CompressedClassSpace` still has no free path at all — `used_size` only
grows. Left as-is: it is a model of a reservation, and adding a free path without
a caller would be inventing a contract.

---

## 5. `numa.rs` — detection only, no placement

**Live on the default path** — three entry points, all consumed by
`gc/src/gen_heap.rs`: `global_topology()`,
`NumaTopology::current_thread_node()`, and `node_of_cpu()`. Detection itself is
real: Linux parses `/sys/devices/system/node/node*/cpulist` and `meminfo`;
Windows and macOS fall back to a single node.

What `gen_heap` does with the answer is store it in a `numa_node_hint` field and,
on a multi-node host, emit a `tracing::trace!` when the calling thread's node
differs from the heap's primary node. Its own comments call this a "NUMA stub".
**No allocation is ever steered to a node.** The heap has one young arena and one
old generation, memory comes from the process allocator, and nothing calls
`mbind`, `set_mempolicy`, `numa_alloc_onnode` or `VirtualAllocExNuma`. On a
single-node host — every current CI and dev target — even the trace is dead.

**Inert, no caller anywhere:** `NumaAllocator`, `NumaArena`, `NumaAllocation`,
`NumaStats`, `NumaPolicy`, and `StringDeduplicator` with its config/stats types.
`NumaAllocator::allocate` is a *simulation*: it bumps a `next_address` counter
starting at `0x1000` and returns that integer. It touches no memory and its
"addresses" are not pointers. It also unconditionally increments
`local_allocations`, so `local_allocation_ratio` is always `1.0` unless a caller
manually calls `record_cross_node_access` — a metric that cannot report a
problem.

Stated plainly, since the module and type names imply otherwise: **any claim that
CratonVM does NUMA-aware placement is wrong.** The topology probe is real; the
placement it would inform does not exist. A module doc now says so.

Wiring it would need per-node young arenas in `gen_heap.rs` plus an OS binding
call at arena reservation — neither of which is in this slug's files, and neither
of which is worth doing before the topology is ever non-trivial on a target host.

---

## Cross-owner requests

Precise, line-referenced. **Not made here** — these files belong to siblings.

### R1 — feed the soft-reference policy a real pressure input (owner: `vm/`)

**File:** `vm/src/runtime/interpreter.rs`
**Functions:** `process_references_after_gc` and `g1_remark_process_references`
**Change:** replace the hardcoded `64` in both

```rust
let result = ref_proc.process_references(&is_marked, 64, 0);   // process_references_after_gc
let result = ref_proc.process_references(is_marked, 64, 0);    // g1_remark_process_references
```

with `shared.mem.heap.soft_ref_policy_free_mb()`:

```rust
let free_mb = shared.mem.heap.soft_ref_policy_free_mb();
let result = ref_proc.process_references(&is_marked, free_mb, 0);
```

**Rationale:** the accessor is new in `gc/src/vm_heap.rs` and documents why it is
keyed on allocatable rather than unused bytes (the non-moving fragmenting default
collector — see §2). The third argument can stay `0`; `gc::reference` now
substitutes the mutator clock it observes through `touch_soft_reference`. Until
this lands the policy runs on a constant 64 MB of assumed headroom and cannot
respond to memory pressure. This is a two-line change and the accessor is already
tested.

### R2 — last-ditch soft-reference clear before `OutOfMemoryError` (owners: `gc/src/reference.rs` is mine, the driver is `vm/`)

**Rationale:** HotSpot never throws `OutOfMemoryError` without first running a
full GC that clears **all** soft references. CratonVM has no equivalent, so an
actively-touched soft cache can force OOM no matter how good the LRU policy is.

**Shape:** add `ReferenceProcessor::request_clear_all_soft_refs()` setting a
one-shot flag that makes the next `process_soft_refs` ignore the LRU predicate,
then call it from the allocation-failure retry loop in
`vm/src/runtime/interpreter.rs` (the "Try to allocate an object, running GC and
retrying on failure" path) before the final attempt, and only then raise
`OutOfMemoryError`.

**Deliberately not landed here:** the API without the driver would be a
capability landing default-off, which this repo has three confirmed cases of. It
should land as one change with both halves. Happy to do the `reference.rs` half
in the same PR as the `vm/` half.

### R3 — `NarrowKlassTable` has no removal path (owner: `gc/src/compact_header.rs`)

**File:** `gc/src/compact_header.rs`
**Type:** `NarrowKlassTable` — fields `to_narrow: RwLock<FxHashMap<u32, u32>>`,
`to_class_id: RwLock<FxHashMap<u32, u32>>`, `next_id: AtomicU32`.
**Change:** add a `remove_classes(&self, ids: &[u32])` that drops both directions
for each id, and call it from whatever wires `CompactAllocator` up.

**Rationale:** both maps are class-id keyed and grow monotonically with no
`remove`, `retain`, `prune` or `clear` anywhere in the impl. `next_id` never
recycles. This satisfies neither arm of the "unload invalidation or hard bound"
rule in `docs/internal/class-loader-unloading-and-bounded-metadata.md`. It is
latent today only because `CompactAllocator` has no caller outside its own file —
which is exactly the condition under which such a table gets wired up without
anyone rechecking the invariant.

### R4 — `docs/internal/class-loader-unloading-and-bounded-metadata.md` (owner: whoever owns that doc)

The bounded-caches list (§"Bounded process-lifetime caches") should either add
the `gc`-crate tables or say explicitly that the GC crate owns no live per-class
side table. As written, a reader reasonably concludes the list is exhaustive
across the whole VM, and §3 above shows three `gc`-crate tables that satisfy
neither arm of the rule.

---

## What was not done

* **Reference-processing pause cost was characterised, not reduced.** The
  cold-list segregation sketched in §1 touches the `soft_ref_addr_index` /
  `soft_ref_lru_index` position invariants and deserves its own change.
* **`ReferenceQueue`'s evict-oldest overflow and spinning `remove_blocking`**
  were left alone — no production caller, and changing either without one would
  be guessing at the contract.
* **Nothing was built or tested** (nine concurrent cargo builds OOM the host).
  All five files parse and are `rustfmt --check` clean; CRLF line endings were
  verified byte-for-byte after every edit. Every new test was hand-traced against
  the code path, and every existing test in `gc/src/reference.rs`,
  `gc/src/class_unloading.rs`, `gc/src/metaspace.rs`, `gc/tests/wp1_10_reference.rs`
  and `gc/tests/phase_h_integration.rs` was re-checked against the changed
  predicates. Two tests in `class_unloading.rs` needed updating (they asserted
  `valid == false` on entries that are now released); both are in-file and were
  updated to assert the stronger property. **This still needs a real
  `cargo test -p cratonvm-gc` before it is trusted.**
