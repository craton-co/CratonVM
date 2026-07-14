# TestDefaultInstanceManager — class-unloading count off by one

**Status:** OPEN (partially fixed — three real, independent VM bugs found;
two fixed and merged; a fourth, deeper bug is the confirmed remaining cause
and is NOT yet fixed). **Severity:** low-medium for the test itself; the
bugs found along the way are higher-severity in general (see below).
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

## Investigation (2026-07-13, two sessions)

The `AnnotationCacheEntry` cache (`ManagedConcurrentWeakHashMap<Class<?>, ...>`)
holds its keys via `WeakReference`. The count only drops once the evicted
JSP's generated `Class` (and transitively its per-JSP `JasperLoader`) becomes
unreachable and the weak ref clears. Four distinct, stacked bugs were found
chasing this across two sessions:

### Bug 1 (FIXED, merged `1205fae8f`) — `SharedVm::class_mirrors` was an unconditional GC root

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
its *defining loader* is independently reachable, via a new registry
`cratonvm_types::mirror_pin` (companion to the existing `loader_pin`,
mirroring its instance→loader edge in the opposite direction:
loader→its-defined-mirrors), consulted by the GC marker. Scoped to the
Generational collector's non-moving marker only — G1/ZGC and the moving
Cheney-copy path fall back to the original unconditional rooting.

### Bug 2 (FIXED, merged `1205fae8f`) — `System.gc()` didn't force a full/major collection

`GenerationalHeap::collect_garbage_inner`'s Phase 5 only ran `major_gc`
(mark-compact of old gen) when old gen crossed a 75%-occupancy threshold —
`System.gc()` had no way to force one, unlike real HotSpot (full GC by
default). **Fix:** `cratonvm_gc::gc_quiescence::request_major_gc()` /
`take_major_gc_request()` (thread-local, consumed exactly once per cycle),
wired into `force_gc_from_native` and `gen_heap.rs` Phase 5.

Neither Bug 1 nor Bug 2 alone or together changed the test's outcome from
`expected:<8> but was:<9>` in a way that stuck — see Bug 4 below for why.

### Bug 3 (FIXED, this session) — `gc_prune_dead_collection_overlays` existed but was never called

`native-collections/src/lib.rs` has a complete, correct function
(`gc_prune_dead_collection_overlays`) to prune stale registry/overlay entries
for `LinkedList`/`LinkedHashMap`/`TreeMap`/`TreeSet`/`ConcurrentSkipListMap`
side-tables once their backing collection object dies — its own doc comment
even says *"that one-line wiring lives outside this crate's scope"* — but
grep confirms **zero call sites anywhere in the tree**. Every such collection
ever touched left a permanent side-table entry (a steady, unbounded leak;
`obj_key_registry`/`lhm_overlay`/`ll_overlay`/etc. only ever grew).

**Fix:** wired `gc_prune_dead_collection_overlays(&is_marked)` into
`process_references_after_gc` (same placement as `reconcile_class_mirrors`,
for the same reason: `update_all_roots` early-returns when `pointer_map` is
empty, the common case for the non-moving JIT-active sweep, so it would
never run there). Verified: prunes ~1000 of ~1470 total overlay slots per GC
cycle in the Tomcat suite. Zero change to rooting behavior — it only removes
bookkeeping for collections `is_marked` already independently agrees are
dead — and a 30-class regression sample plus an isolated single-class
control run showed no new failures (the 3 hangs + 1 fail observed were all
independently reproduced on the **pre-fix baseline binary too** —
pre-existing, see `reference_tomcat_dohead_gc_safepoint_deadlock` /
`reference_tomcat_dohead_speed_oncpu_not_stopped` for the `TestHttpServletDoHeadInvalidWrite*`
family).

**This fix alone does not resolve this test**, because of Bug 4:

### Bug 4 (ROOT CAUSE, NOT FIXED) — `gc_scan_collection_overlay_roots` roots every overlay element unconditionally, with no reachability gate at all

`vm/src/memory/roots.rs` step 17 / `native_collections::gc_scan_collection_overlay_roots`
→ `for_each_overlay_ref(true, |r| roots.push(*r))` roots **every element of
every entry in every overlay table**, full stop — there is no check anywhere
in that function of whether the *backing collection object* is itself
reachable. (Bug 3's prune only removes an entry once its collection is
independently dead by NORMAL graph reachability — but as long as an entry
exists, ALL its elements are unconditional roots on every single cycle,
regardless of the collection's own liveness.)

**Mechanism, confirmed empirically this session:**
1. The JDT Java compiler (`org.eclipse.jdt.internal.compiler`, which Jasper
   uses to compile JSPs) keeps a scratch `List`/array of
   `StackMapFrame` objects internally during bytecode generation for a
   class — a completely ordinary, transient JDK-collection-shaped object.
2. That collection's overlay-table entry (however it got created — any
   `ArrayList`/`LinkedList`/etc. touched during JSP1's compile) is never
   pruned *while the entry's elements themselves are being force-rooted*,
   because forcing the elements alive also, transitively, force-keeps-alive
   anything reachable *forward* from them.
3. One of the force-rooted `StackMapFrame` elements has (or is reachable
   through) a real field chain back into the JDT compiler's own internal
   state (`Compiler`, `JDTCompiler`), which — via a compilation-context/
   listener-style back-reference — reaches the `JspServletWrapper` being
   compiled, whose `theServlet` field holds the actual compiled JSP1
   instance, whose `class_id`'s defining loader (`loader_pin`) is JSP1's
   per-page `JasperLoader`.
4. Result: an internal compiler scratch buffer keeps the **entire
   compilation's object graph — including the evicted JSP's servlet
   instance and its `ClassLoader`** — permanently, artificially reachable,
   long after HotSpot would have (and does) collect all of it once the
   compile finishes and Tomcat drops its own references
   (`FastRemovalDequeue`/`JspServletWrapper.destroy()`).

**Confirmed, not hypothesized:** built a decisive root-membership checker
(`VmHeap::find_referrers`, `find_instances_of_class`, and a full-closure BFS
against the actual `roots` vec fed into the collector — all debug-only,
gated `CRATONVM_DBG_MIRRORPIN=1`, kept in `gc/src/vm_heap.rs`). It proved:
- The evicted JSP1 servlet instance genuinely has a `root_hits > 0`
  intersection with the real, VM-computed root set (21 hits in one run) —
  i.e. this is **not** a marking/liveness-heuristic false positive (an
  earlier hypothesis from the first session, that
  `GenerationalHeap::is_live_young_survivor`'s "non-zero first word" liveness
  check was giving false positives, is **REFUTED** — that function's
  behavior is irrelevant here; the object really is reachable per the VM's
  own root computation).
- Per-step checkpointing of `collect_roots` showed the StackMapFrame hits
  are absent through steps 1–16 and appear starting exactly at step 17
  (`gc_scan_collection_overlay_roots`) — pinpointing the exact mechanism,
  not just a plausible guess.
- Wiring Bug 3's prune did NOT remove them: `gc_prune_dead_collection_overlays`
  correctly found their backing collection's own address still `is_marked
  == true` — consistent with (3) above: the collection is kept alive by the
  very cluster its own force-rooted elements anchor.

**Why not fixed this session:** the correct fix is structurally the same as
Bug 1's (`mirror_pin`): stop unconditionally rooting overlay elements; root
them only when the backing collection object is independently reachable, via
a mark-time propagation registry (collection-address → its overlay element
addresses) consulted at the same marker call sites `loader_pin`/`mirror_pin`
use, plus a corresponding post-GC pruning integration. The difference is
blast radius: Bug 1 touched one cache (`class_mirrors`); this would touch
**six** overlay tables (`ll_overlay`, `lhm_overlay`, `tm_array_table`,
`tm_fast_table`, `ts_array_table`, `cslm_comparator_table`) backing
`LinkedList`/`LinkedHashMap`/`LinkedHashSet`/`TreeMap`/`TreeSet`/
`ConcurrentSkipListMap` — collection types used **extremely** pervasively
throughout the entire test suite and every app this VM runs. A wrong or
partial fix here risks reintroducing exactly the crashes/leaks these overlay
roots were originally added to prevent (`LinkedHashMap.get` crash on
DaCapo's Config map, stale-pointer SIGSEGVs — see the roots.rs step-17
comment history). This needs its own dedicated session with broad regression
coverage across every collection-heavy suite (not just Tomcat), not a
same-session patch.

## What's shipped vs. what's open

- Bug 1 (class_mirrors unconditional rooting) — **FIXED**, merged `1205fae8f`.
- Bug 2 (`System.gc()` not forcing major GC) — **FIXED**, merged `1205fae8f`.
- Bug 3 (`gc_prune_dead_collection_overlays` never wired in) — **FIXED**,
  this session's branch. Real, independent, safe improvement (removes a
  previously-unbounded leak); does not by itself resolve this test.
- Bug 4 (`gc_scan_collection_overlay_roots` unconditional per-element
  rooting) — **OPEN, confirmed root cause of the residual test failure.**
  `TestDefaultInstanceManager.testClassUnloading` will keep failing
  (`expected:<8> but was:<9>`) until this is fixed. Recommended next step:
  build the `mirror_pin`-style conditional-rooting + pin-propagation
  mechanism for all six overlay tables, gated the same way (Generational
  non-moving marker only to start), verified against a broad collection-
  heavy regression sample (not just this one Tomcat test) before merging.

## Debug tooling added (kept, gated behind `CRATONVM_DBG_MIRRORPIN=1`)

- `vm_object::get_or_create_class_mirror` logs `add_mirror_pin` registrations.
- `memory::gc::reconcile_class_mirrors` logs each entry's `is_marked` verdict.
- `native_builtins::classloader::gc_reconcile_defining_loaders` logs each
  loader's `is_marked` verdict.
- `native_collections::gc_prune_dead_collection_overlays` logs total overlay
  slot count and how many it identified as dead each cycle.
- `gen_heap.rs` Phase 5 logs old-gen occupancy / `major_requested` / whether
  major GC will run.
- `VmHeap::find_referrers(target_addr)` (`gc/src/vm_heap.rs`) — reverse-scans
  every live object in the heap for a reference to `target_addr`. Must be
  called during a GC safepoint.
- `VmHeap::find_instances_of_class(class_id)` (`gc/src/vm_heap.rs`) —
  reverse-scans every live object for one whose header class_id matches.
  Same safepoint requirement.

Neither tool is wired to a default call site (the one-off `annotations_jsp`/
`StackMapFrame`-hardcoded watch plumbing used to drive them during this
investigation was removed) — call them ad hoc from a debugger or a temporary
call site when investigating Bug 4's fix. The general pattern that found Bug
4: pick a target class/address, `find_instances_of_class` to get a live
instance, then BFS `find_referrers` outward while checking each visited
address against the *actual* `roots` vec (captured right after
`collect_roots`, before the collector consumes it) for a decisive
root-membership verdict — far more reliable than guessing from `is_marked`
alone.
