# Quarkus: `Class.getClassLoader()` reports the wrong loader when the same class name is also on the plain classpath

## Status
**OPEN, root-caused.** Split out of
`docs/known-issues/quarkus/method-handles-lookup-find-class.md` during its
retirement (`docs/internal/retired/method-handles-lookup-find-class-RETIRED-20260917.md`)
— that doc's named `findClass` defect is fixed; this is a different,
pre-existing bug the fix's own verification exposed.

## Symptom

`ClassLoadingChainAnalyzerTest.analyzeFindsClassesLoadedViaMethodHandlesFindClass`
still fails after `MethodHandles.Lookup.findClass` was correctly wired:

```
org.opentest4j.AssertionFailedError: Should discover Target loaded via MHFindClassUtil ==> expected: <true> but was: <false>
```

## Root cause

When a class name is reachable **both** from a user-defined `ClassLoader`
(here, a `URLClassLoader` subclass pointed at some directory) **and** from
the JVM's own `-cp`/system classloader (because that same directory also
happens to be on the plain classpath), `Class.getClassLoader()` on the class
loaded via the user loader reports the system/app loader instead — even
though the user loader's own `loadClass` override provably fired for it.

Minimal repro (standalone, no Quarkus): a `RecordingClassLoader extends
URLClassLoader` pointed at a temp dir containing `treeshake.MHFindClassUtil`
and `treeshake.Target`; `Class.forName("treeshake.MHFindClassUtil", true,
recordingLoader)` loads and records it correctly, but if `-cp` **also**
includes a directory containing the same two classes:

```
loadedClassNames = [treeshake.MHFindClassUtil]        // loadClass DID fire for it
MHFindClassUtil.class.getClassLoader() ==> jdk/internal/loader/ClassLoaders$AppClassLoader
```

instead of the `RecordingClassLoader` instance that actually resolved it.
Confirmed via `defining_loader_for(vm, class_id)` (the side table
`register_defining_loader` populates, gated behind
`CRATONVM_LOADER_AWARE_RESOLUTION`, default on) returning `None` for the
affected `ClassId` — the resolution never reached the `defineClass1`-family
native that records it, meaning `RecordingClassLoader.loadClass`'s call into
`URLClassLoader`'s real `findClass`/`defineClass` bytecode aliased to a
class identity the system classloader had already established, rather than
genuinely defining a fresh copy under the user loader's own namespace.
`native_class_get_class_loader` (`native-builtins/src/lang_class.rs`) then
falls through its fallback chain to "no defining-loader entry, not a JDK
package → singleton app loader".

Every Quarkus-suite test that reaches this exact shape (an entry class also
present, incidentally, on the outer classpath in addition to an isolated
loader) will silently misattribute `getClassLoader()` the same way. This one
surfaced it because `MethodHandles.Lookup.findClass`'s spec (`Class.forName(name,
false, lookupClass().getClassLoader())`) round-trips through
`getClassLoader()` to pick the loader to search with; most other
class-loading seeds in this test file hand a loader in directly and never
need that round trip, which is why 12 of `ClassLoadingChainAnalyzerTest`'s 13
methods (and `analyzeFindsClassesLoadedViaClassForName3Arg` in particular,
which also takes an explicit loader) are unaffected.

## Why this needs its own fix, not a quick patch alongside `findClass`

This sits squarely inside CratonVM's loader-identity/aliasing system
(`is_user_defined_loader`, `defining_loader_for`,
`register_defining_loader`, `loader_aware_resolution`,
`gc_reconcile_defining_loaders`) — extensively fought-over territory per its
own comments (see `hib-bytecode-enhancement-loader-faithful-linking-FIXED.md`,
`loader-identity.md`), with numerous other tests (CGLIB/Hibernate proxy
identity, Spring's `DynamicClassLoader`, WildFly's `ModuleClassLoader`, ...)
depending on its exact current behavior. A change here needs the wide
regression pass those docs describe, not a two-line patch verified against
one test.

## Affected Tests / Scenarios
- `io.quarkus.deployment.pkg.steps.ClassLoadingChainAnalyzerTest.analyzeFindsClassesLoadedViaMethodHandlesFindClass`
- Any code performing `Class.forName(name, true/false, explicitLoader)` (or
  `Lookup.findClass`, which reduces to it) where the resolved class's own
  runtime type is *also* independently reachable from the plain classpath.

## Remediation / Solution Plan
1. Find where `URLClassLoader.findClass`/`ClassLoader.defineClass`'s native
   path (`native-builtins/src/classloader.rs`, the `defineClass1`-family
   native around line 4340) decides to alias an existing globally-known
   `ClassId` instead of defining a genuinely separate one under the calling
   loader's namespace, and audit whether `register_defining_loader` needs to
   fire on the ALIASED path too (recording "the loader whose `defineClass`
   call actually resolved this, this time" even when the underlying `ClassId`
   is shared) — or whether class identity needs to stop being shared across
   loaders for this case at all (the more correct, more invasive option).
2. Re-run the full loader-identity-sensitive regression set referenced in
   `loader-identity.md` before landing any change here.
