# HIB-CV-24 — Custom child/isolated `ClassLoader` bypassed in `loadClass` (FIXED)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Severity:** High — real correctness bug, deterministic under `--nojit`, HotSpot PASS
**Status:** ✅ **FIXED** (Manifestation A). Ties to **SBR-14** (same root: app-store
resolution ignores a supplied loader's delegation).
**Repro classes:** `scratch/clprobe/CLProbe{A,B,C}.java` (+ the real Hibernate tests).

---

## Manifestation A — `ClassLoaderService` bypasses the supplied `ClassLoader` ✅ FIXED

`org.hibernate.orm.test.bootstrap.registry.classloading.ClassLoaderServiceImplTest`

```
testLookupBefore: expected:<1> but was:<0>   (BEFORE fix)
@@RESULT ... ok=6 failed=1                    (BEFORE)
@@RESULT ... ok=7 failed=0                    (AFTER, ==HotSpot)
```

The test installs an `InternalClassLoader` (counts every `loadClass` whose name
starts with `org.hibernate`) as the TCCL, then builds
`ClassLoaderServiceImpl(null, TcclLookupPrecedence.BEFORE)` and calls
`classForName(...)`. `classForName` does `Class.forName(name, true, aggregated)`
where `aggregated` is Hibernate's `AggregatedClassLoader` — `super(null)` (null /
bootstrap parent) which **overrides `findClass`** to iterate scoped loaders (TCCL
first for `BEFORE`). Expected: the TCCL's `loadClass` is consulted exactly once →
`getAccessCount() == 1`. CratonVM gave **0** — the supplied loader was never
consulted.

### Root cause

`Class.forName(name, init, loader)` (`lang_class.rs`) dispatches
`loader.loadClass(name)` virtually. For a real `java.lang.ClassLoader` the default
real-mode native is **`cl_real_load_class` → `cl_real_load_class_base`**
(`native-builtins/src/classloader_real.rs`). That base delegation resolved the
class through CratonVM's flat global store **first**:

```rust
// 1. Standard VM class loading.
if let Ok(Some(mirror)) = ctx.load_class(&internal) { return Ok(Some(mirror)); }
// 2. ... only NOW try the receiver's findClass override
```

Because CratonVM's global store == the application classpath, step 1 always
answered for an app class — so a null-parent loader's `findClass` override
(step 2) never ran. JVMS §5.3: a **bootstrap** parent cannot load an application
class, so `findClass` MUST run. CratonVM's `load_class` behaves like the app
loader, short-circuiting the supplied loader. (Same defect shape as SBR-14's
`URLClassLoader(parent=null)`.)

Note: the synthetic-mode handler (`classloader.rs::cl_load_class_base_delegation`)
had the identical ordering bug; both were fixed symmetrically. There is also a
dead duplicate stub `native_classloader_load_class` (`lib.rs`) that resolves
globally, but `cl_real_load_class` wins registration in real mode.

### Fix

In `cl_real_load_class_base` (and the synthetic twin), **defer the global
`load_class` resolution to AFTER `findClass`** when the receiver overrides
`findClass` and the requested class is not a bootstrap/platform class:

```rust
let defer_to_find_class = receiver_overrides_find_class(ctx, this)
    && cl_bootstrap_scoped()                 // opt-out gate, default ON
    && !is_bootstrap_class_name(&internal);  // JDK/platform names keep global path
if !defer_to_find_class {
    if let Ok(Some(m)) = ctx.load_class(&internal) { return Ok(Some(m)); }
}
// findClass override; in the deferred case, global store is the LAST resort.
```

- Built-in/app loaders and bootstrap/platform class names keep the fast global
  path → **no regression** to ordinary `Class.forName` / app loading.
- A custom loader that overrides `findClass` with a bootstrap/null parent now runs
  its own `findClass` first; CratonVM's store remains the final fallback so a
  loader whose `findClass` legitimately delegates elsewhere still resolves.
- Gated **default-ON**; opt-out `CRATONVM_CL_BOOTSTRAP_SCOPED=0` restores the
  legacy global-first behavior (the safety net).

### Verification (`--nojit`, ==HotSpot)

| probe / test | HotSpot | CV default | CV `=0` (legacy) |
|---|---|---|---|
| `CLProbeC` (AggregatedClassLoader shape) | count=1 | **count=1** | count=0 |
| `CLProbeA` (loadClass override) | count=1 | count=1 | count=1 |
| `CLProbeB` (defineClass loader) | MyIsolated | MyIsolated | MyIsolated |
| `Ord` (forName app+JDK class) | ok=true | ok=true | ok=true |
| **`ClassLoaderServiceImplTest`** | 7/7 | **ok=7 failed=0** | ok=6 failed=1 |

---

## Manifestation B — `ClassLoaderLeaksUtilityTest`: defining-loader is now CORRECT

The original report (`IllegalStateException: Not being loaded by the expected
classloader`) **no longer reproduces** on current dev. `IsolatedClassLoader.findClass`
→ `defineClass(name, bytes, …)` now records the isolated loader as the defining
loader; `LeakingTestAction.run()` passes its `getClass().getClassLoader().getName()
== "TestIsolatedIsolatedClassLoader"` check (zero `IllegalStateException` in a full
run). `CLProbeB`/`CLProbeD` confirm `defineClass` attributes the right loader even
when the class is also app-loaded. **The "defining loader recorded" item is fixed.**

### ✅ Manifestation B leak-detection — FIXED 2026-06-29 (`fix/hibcv24-loader-unload`)

`ClassLoaderLeaksUtilityTest` **1/2 → 2/2**, both sub-tests fast (3.7 s total, no
180 s timeout), `ClassLoaderServiceImplTest` stays 7/7, ==HotSpot.

**The residual was two layered gaps, the deeper one a general GC defect, not
class-unloading-specific:**

1. **GC weak/phantom-reference clearing did not work end-to-end.** CratonVM's mark
   phase (`gen_heap::for_each_ref_slot` callers) traced *every* reference slot
   strongly, including a `java.lang.ref.Reference`'s `referent`. So a **live**
   `WeakReference`/`PhantomReference` pinned its referent forever — `get()` never
   returned null, phantom queues never fired. The leak detector's `PhantomReference`
   on the isolated loader therefore never enqueued regardless of rooting. (Proven
   with standalone probes: a plain `new Object()` behind a `WeakReference` was never
   cleared in either real-JDK or synthetic mode, while a finalizer on the same
   object *did* run — finalizers are discovered at allocation, weak/phantom only in
   their constructor native, and clearing additionally needs the marker to skip the
   referent.)

2. **The defining-loader side-table hard-rooted every user loader**
   (`gc_scan_loader_singleton_roots`), so even with (1) fixed the loader could not
   be collected.

**Fix — three coordinated, independently-gated changes (all default-ON, opt-outs
revert byte-identical):**

- **`CRATONVM_WEAKREF_CLEAR`** — marker-free two-phase reference clearing. Before
  every collection (`weakref_null_referents_pre_gc`, 6 collect sites) the VM nulls
  the `referent` slot of every active Weak/Phantom reference so the *unmodified*
  marker cannot keep the referent alive; the existing `process_references` then
  clears/enqueues the dead ones, and `process_references_after_gc` restores the
  slots of survivors (referent kept alive via a strong path) and runs
  `remove_collected` to prune dead Reference objects. SoftReferences are left
  strongly reachable (kept). This makes weak/phantom clearing work VM-wide.

- **`CRATONVM_LOADER_UNLOAD`** — `gc_scan_loader_singleton_roots` no longer roots
  the `defining_loader_store` values; `gc_reconcile_defining_loaders` (run in
  `process_references_after_gc`) remaps survivors and prunes loaders collected this
  cycle, using the same survivor predicate as reference processing.

- **`cratonvm-types::loader_pin`** registry (mirrored from `defining_loader_store`
  by `register_defining_loader` / `gc_reconcile` / `reset`). The GC marker consults
  it so **a live object keeps its class's defining loader alive** — the
  `Class`→`ClassLoader` edge HotSpot gets for free, which CratonVM lacked because an
  object holds only a `class_id`, not its loader. Wired into the generational
  collector's Cheney scan, promoted scan, non-moving sweep, and major-GC old-gen
  mark. Conservative (only ever marks *more* live → cannot corrupt). This is what
  keeps `testClassLoaderLeaksDetected` correct: the intentionally-leaked instance
  (stowed in a `ThreadLocal`) now pins its loader, so the loader is *not* collected.

Net behavior, matching HotSpot:
- `testClassLoaderLeaksNegated` — no live instance ⇒ loader unreachable ⇒ collected
  ⇒ phantom enqueues ⇒ assertion passes (fast, no timeout).
- `testClassLoaderLeaksDetected` — instance leaked via `ThreadLocal` ⇒ instance
  pins loader ⇒ loader retained ⇒ phantom never enqueues ⇒ assertion passes.

**Validation:** standalone probes (`scratch/cv24/*.java`) — weak clears, phantom
enqueues, isolated loader collected (LEAK-FREE) and JIT-mode too, leaked-instance
RETAINED, live-loader identity stable across 200 GCs, finalizers still run, all
opt-outs revert; `cratonvm-gc` unit tests 737/737; `ClassLoaderLeaksUtilityTest`
2/2 + `ClassLoaderServiceImplTest` 7/7 via the real harness; sampled
collection/bootstrap Hibernate classes green.

**Not covered (future work):** G1 / ZGC collectors are not default and do not yet
have the referent-skip / loader-pin hooks (no regression — weak/phantom clearing
never worked there either). A young user-loader *instance* whose loader was promoted
to old gen is not pinned across a *major* GC (rare; conservative fallback is the
pre-fix app-loader attribution). Soft references are kept, not cleared under
pressure (intentional — avoids the multi-phase soft-policy mark).
