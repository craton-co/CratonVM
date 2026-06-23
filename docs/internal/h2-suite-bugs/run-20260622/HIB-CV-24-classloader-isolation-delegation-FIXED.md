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

Residual (separate axis, NOT classloader delegation): the test still does not pass
because `testClassLoaderLeaksNegated` (`NotLeakingTestAction`) waits the full
180 s for the isolated loader to be **garbage-collected** and never sees it
collected — the `defining_loader_store` side-table holds a strong, GC-rooted ref to
every user loader that defines a class (CratonVM has no class/loader unloading). The
phantom-reference leak detector therefore times out. This is a **class-unloading**
gap, not a delegation/defining-loader bug, and is out of scope for HIB-CV-24.
Follow-up: weak-ref the defining-loader side-table once class unloading exists.
