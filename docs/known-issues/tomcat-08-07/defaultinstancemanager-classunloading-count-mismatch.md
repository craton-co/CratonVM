# TestDefaultInstanceManager — class-unloading count off by one

**Status:** OPEN (partially fixed — two real, independent VM bugs found and
fixed; a third, deeper bug is the confirmed remaining cause and is NOT yet
fixed). **Severity:** low-medium for the test itself; the two fixed bugs and
the one remaining bug are all higher-severity in general (see below).
**HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.core.TestDefaultInstanceManager.testClassUnloading`
fails:
```
1) testClassUnloading(org.apache.catalina.core.TestDefaultInstanceManager)
java.lang.AssertionError: expected:<8> but was:<9>
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.Assert.fail(Assert.java:89)
	at org.junit.Assert.failNotEquals(Assert.java:835)
	at org.junit.Assert.assertEquals(Assert.java:647)
```
The test loads 2 JSPs, captures `count = instanceManager.getAnnotationCacheSize()`,
loads a 3rd JSP (which evicts the 1st under `maxLoadedJsps=2`), forces
`System.gc()`, then polls `backgroundProcess()` for up to 1s expecting the
cache to shrink back to `count`. CratonVM's cache never drops below `count+1`.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES on HotSpot.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName defaultinstmgr `
  -Start <idx> -Count 1 -TimeoutSec 180 -Parallel 1
# org.apache.catalina.core.TestDefaultInstanceManager (index 155 in apps/tomcat/.suite/all-tests.txt)
```
Use `-TimeoutSec 180`, not 60 — this box is a heavily shared host (see
`feedback_shared_host_multitenant_confound` in project memory); under load
the class alone can take 60-150s just for Tomcat+JSP-compile startup, which
reads as a false HANG at a 60s timeout.

## Investigation (2026-07-13, worktree `fix/instmgr-classunload-count-20260713`)

The `AnnotationCacheEntry` cache (`ManagedConcurrentWeakHashMap<Class<?>, ...>`)
holds its keys via `WeakReference`. The count only drops once the evicted
JSP's generated `Class` (and transitively its per-JSP `JasperLoader`) becomes
unreachable and the weak ref clears. Three distinct, stacked bugs were found
chasing this:

### Bug 1 (FIXED) — `SharedVm::class_mirrors` was an unconditional GC root

`vm/src/memory/roots.rs` step 6 rooted **every** `java.lang.Class` mirror ever
created, forever, with no gate. A mirror's `classLoader` field is a real heap
edge to its defining `ClassLoader`, so this transitively kept **every**
user-defined `ClassLoader` that ever had a class reflected on
(`getClass()`, annotation scanning — i.e. almost all of them) alive forever,
completely defeating `CRATONVM_LOADER_UNLOAD` (default ON) for such loaders.
This is a real, generally-applicable classloader-leak bug, not specific to
JSPs — any app relying on custom-classloader GC (OSGi-style plugin unloading,
hot-redeploy, `ClassLoaderLeaksUtilityTest`-style tests) would leak every
custom loader indefinitely.

**Fix:** `roots.rs` step 6 now only unconditionally roots mirrors of
built-in-loader classes; a user-defined class's mirror is rooted only when
its *defining loader* is independently reachable. Since CratonVM's synthetic
`ClassLoader` has no heap-traceable `ClassLoader.classes` bookkeeping (unlike
a real JDK), a new registry `cratonvm_types::mirror_pin` (companion to the
existing `loader_pin`, mirroring its instance→loader edge in the opposite
direction: loader→its-defined-mirrors) is populated at mirror-creation time
and consulted by the GC marker (`gc/src/gen_heap.rs`, the same two call
sites `loader_pin` already uses) so a genuinely-live loader keeps its
classes' mirrors alive too. A dead mirror is pruned post-GC by the new
`memory::gc::reconcile_class_mirrors` / `rebuild_mirror_pins`. **Scoped to
the Generational collector's non-moving marker only** (`gc_quiescence::is_active()`
+ `GcAlgorithm::Generational`) — G1/ZGC and the moving Cheney-copy path fall
back to the original unconditional rooting (safe, just doesn't get the fix in
those modes; G1 already had an analogous pre-existing gap for `loader_pin`
itself, so this doesn't newly regress it).

Verified: this fix alone changed the test's failure from
`expected:<8> but was:<9>` (permanent over-retention) to
`expected:<8> but was:<6>` (proves classes/loaders now genuinely become
collectible — an under-retention in the *opposite* direction, from an
initial classification bug that used a mutable liveness side-table instead
of the permanent per-class loader-kind; that was found and corrected in the
same session, see commit history on this branch).

### Bug 2 (FIXED) — `System.gc()` didn't force a full/major collection

`GenerationalHeap::collect_garbage_inner`'s Phase 5 only ran `major_gc`
(mark-compact of old gen) when old gen crossed a 75%-occupancy threshold —
`System.gc()` had no way to force one. Real HotSpot's `System.gc()` triggers
a full (young+old) collection by default. This is also a real,
generally-applicable gap: any test/app relying on `System.gc()` to reclaim
already-promoted garbage (a common weak/soft/phantom-reference-based
resource-cleanup pattern) would see stale results until old gen happened to
independently cross the occupancy threshold.

**Fix:** new `cratonvm_gc::gc_quiescence::request_major_gc()` /
`take_major_gc_request()` (thread-local, check-and-clear-once semantics, same
style as the module's other per-cycle flags). `force_gc_from_native`
(`System.gc()`'s native impl) calls `request_major_gc()`; Phase 5 in
`gen_heap.rs` ORs the occupancy check with `take_major_gc_request()`
(evaluated unconditionally, not short-circuited, so the request can't leak
into a later unrelated minor GC).

Verified via debug tracing (`CRATONVM_DBG_MIRRORPIN=1`) that major GC now
actually runs on `System.gc()` (`Phase5 ... major_requested=true
will_run_major=true`). Did NOT by itself change the test's outcome, because —

### Bug 3 (ROOT CAUSE, NOT FIXED) — `is_live_young_survivor`'s liveness heuristic is unsound

`GenerationalHeap::is_live_young_survivor` (called from `VmHeap::is_addr_live`,
consulted by `gc_reconcile_defining_loaders`, weak/soft/phantom reference
processing, and now also by my `reconcile_class_mirrors`) is:
```rust
pub fn is_live_young_survivor(&self, addr: usize) -> bool {
    ...
    let word0 = unsafe { std::ptr::read(addr as *const u64) };
    word0 != 0
}
```
i.e. "is this address in the young from-space range, with a non-zero first
word." This was added 2026-07-07 to fix the *opposite* problem (genuinely-live
non-moving-sweep survivors being wrongly judged dead, since the non-moving
path produces no `pointer_map` entries the way the moving Cheney-copy path
does — see the "RRWL hold-count IMSE/hang family" in git history).　The
non-moving sweep (`sweep_young_non_moving`) reclaims dead objects by adding
`(offset, size)` to an **out-of-band** free list (`Arena`'s `free_small`/
`free_large`, `gc/src/arena.rs`) — it does **not** zero the reclaimed memory.
A real object's header always has a non-zero first word
(`class_id|kind|element_type`), so a **dead, correctly-reclaimed, but
not-yet-reallocated** object reads back exactly like a live one: `word0 != 0`.

Confirmed empirically (`CRATONVM_DBG_MIRRORPIN=1` + a new debug-only
`VmHeap::find_referrers` heap reverse-scanner added this session,
`gc/src/vm_heap.rs`): for this test, the per-JSP `JasperLoader` that should
have been collected reports `is_addr_live=true` (`is_old_gen_addr=false`,
confirmed `old_gen_used=0` — nothing was ever promoted, ruling out Bug 2's
occupancy angle) while a full live-heap reverse scan finds **zero** real
objects referencing it (`referrers=0`). I.e. the object genuinely IS garbage
and was almost certainly already correctly identified as such by the
non-moving sweep's mark phase — `is_addr_live` just can't see that, because
its heuristic can't distinguish "still reachable" from "reclaimed, not yet
reused."

**Why not fixed this session:** the natural fix (check
`header.gc_flags & GC_FLAG_MARKED` — which the mark phase does set on
survivors this session's tracing confirmed the code already uses — instead
of / in addition to the "non-zero word0" heuristic) touches a function
consulted by weak/soft/phantom reference processing and classloader
unloading VM-wide, with a known-delicate history (the 2026-07-07 fix it
would be revising was itself built to close a hang bug — the "RRWL
hold-count IMSE/hang family"). A wrong fix here risks reintroducing that
hang or a worse false-negative (a still-*live* object wrongly swept). This
needs its own dedicated session: read the sweep's mark-bit lifecycle in
full (does `GC_FLAG_MARKED` get reliably cleared at the start of every
non-moving sweep before re-marking, so a stale bit from an earlier cycle
can't cause a *different* false positive/negative?), then change
`is_live_young_survivor` carefully with the RRWL-family regression tests as
a gate, not just this one Tomcat test.

## What's shipped vs. what's open

- Bug 1 (class_mirrors unconditional rooting) — **FIXED**, merged.
- Bug 2 (`System.gc()` not forcing major GC) — **FIXED**, merged.
- Bug 3 (`is_live_young_survivor` false-positive liveness) — **OPEN, confirmed
  root cause of the residual test failure.** `TestDefaultInstanceManager.
  testClassUnloading` will keep failing (`expected:<8> but was:<9>`) until
  this is fixed. Recommended next step: instrument/verify `GC_FLAG_MARKED`'s
  clear-and-remark lifecycle in `sweep_young_non_moving`, then swap
  `is_live_young_survivor`'s check to the mark bit, re-verify against the
  RRWL hold-count regression scenario Bug 2's neighbor fix (2026-07-07) was
  built for, THEN re-run this test.

## Debug tooling added (kept, gated behind `CRATONVM_DBG_MIRRORPIN=1`)

- `vm_object::get_or_create_class_mirror` logs `add_mirror_pin` registrations.
- `memory::gc::reconcile_class_mirrors` logs each entry's `is_marked` verdict.
- `native_builtins::classloader::gc_reconcile_defining_loaders` logs each
  loader's `is_marked` verdict.
- `gen_heap.rs` Phase 5 logs old-gen occupancy / `major_requested` / whether
  major GC will run.
- `VmHeap::find_referrers(target_addr)` (`gc/src/vm_heap.rs`) — reverse-scans
  every live object in the heap for a reference to `target_addr`. Must be
  called during a GC safepoint. Not wired to a default call site (removed
  the one-off `annotations_jsp`-hardcoded watch-address plumbing used to
  find Bug 3) — call it ad hoc from a debugger or a temporary call site when
  investigating Bug 3's fix.
