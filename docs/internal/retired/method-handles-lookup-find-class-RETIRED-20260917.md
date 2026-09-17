# Retired 2026-09-17: `MethodHandles.Lookup.findClass` was never wired

Branch `claude/quarkus-lookup-oidc-kir-20260916`. Retires
`docs/known-issues/quarkus/method-handles-lookup-find-class.md`.

**The named defect — `findClass` not registered — is fixed and verified
correct in isolation, byte-for-byte against real JDK 25.** A separate,
pre-existing classloader-identity gap (not specific to `findClass`, not
described by the original doc) still fails the one named unit test in the
full Quarkus harness; it is filed on its own as
`docs/known-issues/quarkus/classloader-identity-ambiguity-split-classpath.md`
rather than folded into this retirement, because it is a different bug with
a different (and much larger) blast radius.

## What was actually wrong

`java/lang/invoke/MethodHandles$Lookup` is a VM-fabricated synthetic object
in CratonVM (`native-builtins/src/lookup_define.rs`), not the real JDK class
file — real `defineClass`/`defineHiddenClass`/`defineHiddenClassWithClassData`
were natively wired (WP2.3-B), but `findClass` was not, so dispatch found no
method body at all for it: exactly the interpreter's generic "resolved method
has no Code attribute" path, surfacing wherever a caller happened to catch it
(here, silently swallowed by `ClassLoadingChainAnalyzerTest`'s
`ClassLoadingRecorder.main`, which discards `Exception | LinkageError` from
each entry point — so the discovery set was simply missing `Target`, not
visibly erroring).

## The fix

Registered `MethodHandles$Lookup.findClass(String)Ljava/lang/Class;` in
`native-builtins/src/lookup_define.rs`, implemented as JDK 25's own spec:

```java
Class<?> targetClass = Class.forName(targetName, false, lookupClass().getClassLoader());
return accessClass(targetClass);
```

by resolving `lookupClass().getClassLoader()` (via the existing
`native_class_get_class_loader`, the same native backing
`Class.getClassLoader()`) and delegating to `native_class_for_name` — the
SAME machinery `Class.forName(String, boolean, ClassLoader)` already uses,
including its `loader.loadClass(name)` routing via `invoke_virtual`. That
routing is what makes the resolution observable to a caller's own
`ClassLoader.loadClass` override (a recording classloader, a module
classloader, ...), exactly as real bytecode calling through `Class.forName`
would be — reusing this path rather than hand-rolling a second one was
deliberate: it inherits every existing loader-routing fix
(`native_class_for_name` has ~700 lines of measured JDK-parity behavior) for
free instead of re-deriving a subset of it. `accessClass`'s access check is
approximated (public target, or same runtime package as the lookup class) —
sufficient for every caller a full-power `MethodHandles.lookup()` produces.

## Verification (isolated, no ambiguity)

A minimal repro matching the real JDK idiom exactly — a custom
`URLClassLoader` overriding `loadClass(String)` to record names, loading a
util class through it whose constructor calls
`MethodHandles.lookup().findClass("treeshake.Target")`, with `Target`
reachable **only** via that loader (not also on the JVM's own `-cp`, which is
its own separate bug — see the new doc) — matches real JDK exactly:

```
=== REAL JDK ===
loadedClassNames = [..., treeshake.MHFindClassUtil, ..., treeshake.Target, ...]
Target discovered = true
=== CRATONVM (fixed) ===
loadedClassNames = [treeshake.MHFindClassUtil, treeshake.Target]
Target discovered = true
```

Before the fix, CratonVM raised no exception (findClass's dispatch fell into
a generic no-Code-attribute path caught upstream) and simply never invoked
`loadClass` for the target name.

`cargo test --release -p cratonvm-native-builtins --lib`: 4280 passed, 0
failed. `registrar_drift`: 7 passed, 0 failed.

## The residual: `ClassLoadingChainAnalyzerTest.analyzeFindsClassesLoadedViaMethodHandlesFindClass` still fails in the full harness

Not because of `findClass`. `ForkedJvmEnvironment` (the test's own forked-JVM
harness) writes `MHFindClassUtil.class` + `Target.class` into the SAME temp
directory it also puts on the **forked child JVM's own `-cp`** (alongside
`ClassLoadingRecorder.class`), so both classes are reachable *both* from the
child's system classloader *and* from `ClassLoadingRecorder`'s own
`RecordingClassLoader` (a `URLClassLoader` pointed at that identical
directory). Reproduced this exact shape standalone: with the target class
**also** on the plain `-cp`,
`MHFindClassUtil.class.getClassLoader()` reports
`jdk/internal/loader/ClassLoaders$AppClassLoader` — not the
`RecordingClassLoader` that actually loaded it (proven, since
`RecordingClassLoader`'s own `loadClass` override fired and recorded the
name). `findClass`'s `Class.forName(name, false, lookupClass().getClassLoader())`
then resolves `Target` through the (wrong) app loader, which also finds it on
the plain `-cp` without ever calling `RecordingClassLoader.loadClass`, so
`Target` is never recorded.

This is a general "does `Class.getClassLoader()` report the loader that
`Class.forName(name, true, loader)` actually reentered, or the one that
happened to define the class first" gap — `defining_loader_for` confirmed
empty for the affected `ClassId`, meaning the loader-identity side table
(`register_defining_loader`, gated behind `loader_aware_resolution`, default
on) was never populated for this resolution, so `native_class_get_class_loader`
fell through to its app-loader default. It would affect any reflective
`getClassLoader()` call in the same split-classpath shape, not just
`findClass` — filed separately rather than patched here to keep this
retirement's fix (the isolated, verified `findClass` registration) from being
entangled with a classloader-identity change that needs its own, much wider
regression pass before it can be trusted.
