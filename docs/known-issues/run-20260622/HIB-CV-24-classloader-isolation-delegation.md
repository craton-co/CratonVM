# HIB-CV-24 — Classloader isolation/delegation not honored (non-JIT, deterministic)

> ✅ **FIXED (Manifestation A) 2026-06-23.** Root cause: real-mode
> `cl_real_load_class_base` (`native-builtins/src/classloader_real.rs`) resolved
> through CratonVM's global store BEFORE a null-parent loader's `findClass`
> override, bypassing the supplied loader. Fix defers global resolution to after
> `findClass` for null-parent `findClass`-overriding loaders (gate
> `CRATONVM_CL_BOOTSTRAP_SCOPED`, default-ON). `ClassLoaderServiceImplTest` 6/1→7/7
> ==HotSpot, no parent-first regression. Manifestation B's defining-loader is also
> correct now; its residual is a class-unloading/leak-detector gap (separate).
> Full write-up:
> `docs/internal/h2-suite-bugs/run-20260622/HIB-CV-24-classloader-isolation-delegation-FIXED.md`.


**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — real correctness bug, **deterministic, reproduces under `--nojit`**, HotSpot PASS
**Status:** Confirmed; two independent test manifestations; same theme (custom/isolated `ClassLoader` not used as required)

Related to the known classloader-isolation handoff (`SBR-14`).

---

## Manifestation A — `ClassLoaderService` bypasses the supplied `ClassLoader`

`org.hibernate.orm.test.bootstrap.registry.classloading.ClassLoaderServiceImplTest`

```
testLookupBefore:  java.lang.AssertionError: expected:<1> but was:<0>
```

The test builds a `ClassLoaderServiceImpl` over a custom `IsolatedClassLoader`
that counts accesses, loads a class through the service, then asserts
`icl.getAccessCount() == 1`. On CratonVM the count is **0** — i.e. CratonVM
resolved the class **without** delegating to the supplied `ClassLoader`
(it served the class from some other/parent/internal loader). HotSpot: PASS.

## Manifestation B — class loaded by the wrong classloader

`org.hibernate.orm.test.bootstrap.registry.classloading.ClassLoaderLeaksUtilityTest`

```
testClassLoaderLeaksDetected: java.lang.IllegalStateException: Not being loaded by the expected classloader
```

`ClassLoaderLeakDetector.verifyActionNotLeakingClassloader(...)` loads an action
class in a throwaway isolated `ClassLoader` and asserts the loaded class's
`getClassLoader()` is that isolated loader. On CratonVM the class comes back
associated with a **different** loader → the detector aborts with the
`IllegalStateException`. HotSpot: PASS.

## Why it's a real CratonVM bug

- Both reproduce **deterministically** standalone under `--nojit` (not the JIT
  bug family HIB-CV-20/21).
- Both **PASS on HotSpot**.

## Root area

CratonVM's `ClassLoader` handling does not faithfully route class/resource
resolution through an application-provided child/isolated `ClassLoader`:
- delegation order / `loadClass` dispatch appears to short-circuit to a
  parent or VM-internal loader (Manifestation A: supplied loader never called);
- the defining loader recorded for a loaded class is not the loader that was
  asked to load it (Manifestation B: `Class.getClassLoader()` wrong).

This is the same class-identity / per-loader isolation gap tracked as SBR-14.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-ClassLoaderServiceImplTest> 0
# testLookupBefore -> AssertionError expected:<1> but was:<0>

cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-ClassLoaderLeaksUtilityTest> 0
# testClassLoaderLeaksDetected -> IllegalStateException: Not being loaded by the expected classloader
```

## Triage

Genuine correctness bug, independent of the JIT. Custom-`ClassLoader` delegation
+ per-loader class identity. Strong **hand-off** candidate (ties to SBR-14
classloader isolation). Deterministic repros above make it tractable.
