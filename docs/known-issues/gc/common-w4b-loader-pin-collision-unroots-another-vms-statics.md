# A class-id collision in the process-wide `loader_pin` table unroots another VM's loader-conditional roots

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7): OPEN for G1 and ZGC only; the Generational marker side and the root side are fixed and unchanged.** No code on this page's path changed in the round; `loader_pin::tests` and `a_colliding_class_ids_loader_pin_is_used_only_by_its_own_vm` pass in the round's full Windows test suite on this tree. Remaining: the four marker edits named in the verification plan (`gc/src/g1.rs` two sites, `gc/src/zgc.rs`, `gc/src/zgc/mark_roots.rs`), owned by the G1 and ZGC collectors, out of this round's scope. Retire when `rg "loader_pin::(loader_pin_addr|snapshot)\b" gc/src/g1*.rs gc/src/zgc*` finds no VM-less call.

> **STATUS (2026-09-26, gc-common w36-d): PARTIALLY FIXED. The root side
> (the subject this page was filed about) is FIXED. The marker side is FIXED
> for Generational and still OPEN for G1 and ZGC,** parked on those two
> collector owners as
> `docs/internal/gc-common-round-20260923/handoff-w6a-markers-pick-their-own-heaps-loader-pin.md`.
>
> **Generational marker side, fixed by w36-d.** No Generational site calls a
> VM-less form any more (`rg -n "loader_pin::(loader_pin_addr|snapshot)\b"
> gc/src/gen_heap.rs gc/src/gen_evac.rs` finds only one doc comment):
>
> * The eight young instance->loader lookups (the moving main scan, Phase
>   2.5b, `redrain_serial_closure`, `scan_in_place_young`) go through
>   `gen_heap.rs::young_loader_pin` (~1249), which is
>   `loader_pin_addr_where(cid, |a| young_from.contains(a))`. So does the
>   parallel evacuator (`gen_evac.rs` ~1656, `in_from_extent`).
> * The moving loader rescue's snapshot and the young precise marker's
>   `YoungMarkCtx::loader_pins` use `snapshot_where` over the from-space
>   (~14454, ~16388).
> * The old-gen BFS (~22484) and the walk-gap seed (`seed_old_gen_walk_gaps`,
>   ~28150) use the `_where` forms over `old_gen.contains`.
>
> Each site acts only on a loader inside the space its predicate names, so
> "the row in my space" is exactly the row it needs. With one row the answer
> is the VM-less one. No unit test was added for this: a collision needs two
> VMs' rows on one id in the process-wide registry, and the row choice itself
> is already pinned by `loader_pin::tests::a_marker_picks_the_row_inside_its_own_heap`.
>
> **Still open (G1, ZGC; collector files this lane may not edit).** The exact
> remaining edits:
>
> * `gc/src/g1.rs:18938` (the mark's no-snapshot arm):
>   `loader_pin_addr(header.class_id.as_u32())` becomes
>   `loader_pin_addr_where(header.class_id.as_u32(), <G1 heap containment>)`.
>   `G1Collector::is_heap_addr(addr).is_some()` (`g1.rs:26032`) answers the
>   question, or use a cheaper region-table range test if one is in scope.
> * `gc/src/g1.rs:31844`: `MarkSideTables::capture(epoch)` has no heap in
>   scope. It needs the same predicate as a parameter, then
>   `snapshot()` becomes `snapshot_where(pred)`.
> * `gc/src/zgc.rs:15364` (`visit_pin_edges`) and
>   `gc/src/zgc/mark_roots.rs:698`: `loader_pin_addr(id)` becomes
>   `loader_pin_addr_where(id, <the ZGC heap's reservation-range test>)`.
>
> The page retires when those four land.
>
> **Fixed (root side and registry):**
>
> * `types/src/loader_pin.rs` is VM-keyed: one `(vm, loader)` row per VM per
>   class id, most recent writer last, with the exact per-VM lookup
>   `loader_pin_addr_for_vm` (`:265`) (w5-b, `6128eed8d`). A VM's unload no
>   longer deletes a shadowed VM's row; a disposed VM's rows are dropped at
>   teardown (`forget_vm_loader_pins`, `:225`, called from
>   `native-builtins/src/classloader.rs:287`).
> * Every root deferral goes through `vm/src/memory/roots.rs::vm_loader_pin_addr`
>   (`:268`), which is `loader_pin_addr_for_vm`: `collect_roots` sections 2,
>   3, 6b and 13, `native_roots::defer_or_root`, the indy and
>   `ObjectStreamClass` rows (`native_roots::scan_indy_call_sites` /
>   `scan_osc_cache`, w5-b), and the `ClassValue` deferral in
>   `phases_late::gc_scan_classvalue_cache_roots` (orchestrator w5 merge;
>   `applied/handoff-w5b-classvalue-scan-defers-to-its-own-vms-loader.md`).
>   Confirmed now: `rg -n "loader_pin::loader_pin_addr\b" vm/src native-builtins/src`
>   finds only the non-vacuity assertion in
>   `roots::tests::a_colliding_class_ids_loader_pin_is_used_only_by_its_own_vm`
>   (`roots.rs:3211`).
> * The marker-side API exists (w6-a, `8e8e1cac8`):
>   `loader_pin_addr_where(class_id, is_own)` (`:319`) and
>   `snapshot_where(is_own)` (`:329`) pick the row inside the caller's heap
>   when several VMs share an id; one row is returned without calling
>   `is_own`. Tests: `loader_pin::tests::colliding_class_ids_keep_one_row_per_vm`,
>   `loader_pin::tests::a_marker_picks_the_row_inside_its_own_heap`.
>
> **Before w36-d (marker side, collector files).** No collector called the
> `_where` forms. The VM-less `loader_pin_addr` / `snapshot` calls were:
>
> * `gc/src/gen_evac.rs:1651`;
> * `gc/src/gen_heap.rs`: eight lookups and three snapshots, all converted by
>   w36-d (see above);
> * `gc/src/g1.rs:18938` and `:31844`;
> * `gc/src/zgc.rs:15364`;
> * `gc/src/zgc/mark_roots.rs:698`.
>
> The G1 and ZGC sites are what keeps the page open.
>
> With both VMs' user-loader classes live on one id, the VM-less answer
> names one VM's loader, so the other VM's marker follows a live instance to
> a foreign address (its bounds check rejects it) instead of to its own
> defining loader, which can then be unloaded while its instances are live.
> Exposure: processes with more than one live VM (`libcratonvm` embedders,
> the `vm` unit-test binary).

**Filed:** 2026-09-23 by gc-common round, wave 4, lane B4.
**Backends:** all three; every cycle on ZGC (the default), where
`roots::conditional_loader_metadata` is always true. **Exposure:** any process
with more than one live VM -- `libcratonvm` embedders and this repository's
unit tests, which build one `SharedVm` per test and run them in parallel.

## Evidence (as filed; the root-side half is fixed, see STATUS)

* `types/src/loader_pin.rs` was ONE process-wide map `class_id -> (vm,
  loader address)`, keyed by the bare class id; class ids restart at 0 per
  VM, so two VMs collide and the row belonged to whichever wrote last.
  `loader_pin_addr(class_id)` returns the address and drops the owner. The
  module doc argues the collision is an OVER-approximation -- true for its
  marker consumers only while at most one VM has a user-loader class at the
  id.
* It was an UNDER-approximation for the root DEFERRALS. `collect_roots` §2
  (statics), §3 (class locks), §6b (proxy `Method`s), §13 (condy values),
  `native_roots::defer_or_root` (`Class$Atomic` reflection slots, annotation
  proxies), the generic indy `CallSite`s and the `ObjectStreamClass`
  descriptors all did `if let Some(loader) = loader_pin_addr(cid) {
  add_metadata_pin(vm, loader, value); continue; }` -- REPLACING the root with
  a `metadata_pin` row keyed by that loader address. If the row was VM A's,
  VM B's marker never reached A's loader, so B's value was marked by nothing
  and freed while B's static / lock / call site still named it.
* It needed no user loader in VM B: B's class at id X could be a JDK class,
  and one user-loader class at id X in any other live VM sufficed.

## Failure scenario (root side, fixed)

Two tests running in parallel (default ZGC): test A loads a class through a
custom `ClassLoader` (class id X). Test B's class X is, say,
`java.util.Locale`'s holder of a static cache. B collects: the static's value
is deferred to A's loader address, not marked, reclaimed. B's next read of the
static returns recycled memory -- the `ClassId(0)` / wrong-class family, only
in parallel runs.

## Remaining: the marker side

When BOTH VMs have a user-loader class at id X, the VM-less row names one
VM's loader, so the other VM's marker follows a live instance of X to a
foreign address (rejected) instead of to its OWN defining loader -- that
loader loses the instance->loader edge and can be unloaded while its
instances are live. Each collector site listed in the STATUS block should
call `loader_pin_addr_where` / `snapshot_where` with the predicate its heap
already has for "is this address mine" (live heaps do not overlap, so "the
row inside my heap" is "my VM's row"), or state why its lookup cannot meet a
collision.

## Confirm

```
rg -n "loader_pin::(loader_pin_addr|snapshot)\b" gc/src
```

Open while that lists marker call sites.

## Retire when

Every collector site above calls a `_where` form (or documents why the
VM-less answer is equivalent there), and this page moves to
`docs/internal/gc-common-round-20260923/`.
