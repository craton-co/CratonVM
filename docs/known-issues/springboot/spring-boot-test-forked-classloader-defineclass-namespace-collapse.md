# core/spring-boot-test — SpringBootContextLoaderAotTests: classloader parent-chain fidelity gap under `@CompileWithForkedClassLoader`

**Status: OPEN — found 2026-07-20, root-caused to an architectural area
(classloader delegation fidelity), not a specific file:line one-line fix.**

Third-layer residual of
[`docs/internal/fixed-suite-bugs/spring-boot-test-configdata-classpath-scan-cluster-FIXED.md`](../../internal/fixed-suite-bugs/spring-boot-test-configdata-classpath-scan-cluster-FIXED.md)
— read that doc first for the two loader-identity bugs already fixed in the
same test (interface default-method dispatch; annotation enum-value loader
resolution) and for the `builtin_loader_reachable` platform-loader fix that
is real but not sufficient to close this one.

## Symptom

`SpringBootContextLoaderAotTests.loadContextForAotProcessingAndAotRuntime()`
fails with:

```
java.lang.NullPointerException: Cannot invoke "org.springframework.boot.logging.DeferredLogFactory.getLog(java.lang.Class)" because "logFactory" is null
    at org.springframework.boot.cloud.CloudFoundryVcapEnvironmentPostProcessor.<init>(CloudFoundryVcapEnvironmentPostProcessor.java:106)
    ... (java.lang.reflect.InvocationTargetException, via SpringFactoriesLoader$FactoryInstantiator.instantiate)
```

Reached via `SpringBootContextLoader.loadContext` → real `SpringApplication.
run()` → `SpringApplicationRunListeners.environmentPrepared` →
`EventPublishingRunListener` → `EnvironmentPostProcessorApplicationListener`
→ `SpringFactoriesEnvironmentPostProcessorsFactory.getEnvironmentPostProcessors`
→ `SpringFactoriesLoader.load(EnvironmentPostProcessor.class, argumentResolver)`
→ reflective constructor instantiation of
`CloudFoundryVcapEnvironmentPostProcessor(DeferredLogFactory logFactory)`.

HotSpot: passes cleanly (confirmed, same classpath, same JDK 25).

## Root cause (confirmed via live debug tracing in a scratch Gradle checkout)

`@CompileWithForkedClassLoader` (a JUnit extension in
`spring-core-test`) reloads the whole test-class-and-framework graph through
a private `CompileWithForkedClassLoaderClassLoader`, constructed as:

```java
CompileWithForkedClassLoaderClassLoader(ClassLoader testClassLoader) {
    super(testClassLoader.getParent());   // <-- parent SKIPS testClassLoader itself
    this.testClassLoader = testClassLoader;
}
```

Its `loadClass(String)` override delegates `org.junit.*`/`org.testng.*`
names straight to `testClassLoader`, and everything else through
`super.loadClass(name)` — i.e. the JDK's standard algorithm: try the
declared **parent** (here, `testClassLoader.getParent()` — the platform
loader, in a flat `-cp` launch where `testClassLoader` is the application
loader), and only on failure call this loader's own `findClass` override
(which mints a genuinely fresh, isolated copy of the requested class from
the original class bytes).

Since the forked loader's parent is the **platform** loader (not the
application loader), any *non-JDK* class name (e.g.
`org.springframework.boot.support.EnvironmentPostProcessorsFactory`) should
**fail** parent delegation on real HotSpot and fall through to `findClass`,
producing a class object that is **its own, isolated from the application
loader's copy of the same name**.

Debug trace added at the actual failure site (temporary
`System.out.println`s in scratch copies of `SpringFactoriesEnvironmentPostProcessorsFactory.
getEnvironmentPostProcessors` and `CloudFoundryVcapEnvironmentPostProcessor`'s
constructor, recompiled standalone and prepended to the classpath, reverted
afterward) showed:

```
[DBG-C] DeferredLogFactory.class loader=AppClassLoader        (captured when building the ArgumentResolver)
[DBG-B] DeferredLogFactory.class loader=CompileWithForkedClassLoaderClassLoader  (seen by the constructor's own param type)
```

Two **different** `Class` objects for `DeferredLogFactory`, one per loader —
so `ArgumentResolver`'s type-equality match
(`constructor.getParameterTypes()[i].equals(registeredType)`, pure Spring
bytecode, not a CratonVM code path) legitimately fails, and Spring's own
`resolveArgs` supplies `null` for the unmatched parameter (by design — see
`SpringFactoriesLoader$FactoryInstantiator.resolveArgs`).

`Thread.currentThread().getContextClassLoader()` is **confirmed correct**
(reports the forked loader) at the point `ArgumentResolver.of(DeferredLogFactory.
class, logFactory)` runs — this is *not* a context-classloader propagation
bug. The divergence is that `SpringFactoriesEnvironmentPostProcessorsFactory`
itself (the class whose own bytecode contains that `DeferredLogFactory.class`
literal) was loaded via the **application** loader, not the forked one, even
though the call chain that reaches it (`EnvironmentPostProcessorApplicationListener`
→ `EnvironmentPostProcessorsFactory.fromSpringFactories` → `new
SpringFactoriesEnvironmentPostProcessorsFactory(...)`) originates from
classes that verifiably **are** forked-loaded.

Applying the `builtin_loader_reachable` fix (excluding the platform loader
from counting as "safe to consult the flat global class store" — see the
FIXED doc) changes the `defer_to_find_class`/`scoped_user_chain` decision
in `native-builtins/src/classloader_real.rs::cl_real_load_class_base`
correctly for the `EnvironmentPostProcessorsFactory` interface lookup
(confirmed via `CRATONVM_DBG_CL_SCOPE` tracing: `defer_to_find_class=true`,
`scoped_user_chain=true`, i.e. the flat-store step is now skipped and
`findClass` is the authoritative path) — **but the NPE still occurs**,
and `SpringFactoriesEnvironmentPostProcessorsFactory`'s own defining loader
is *still* reported as `AppClassLoader` afterward. This means:

- Either `findClass()`'s own definition path (the forked loader's
  `defineDynamicClass` → `ClassLoader.defineClass`, backing its
  `classResourceLookup` fallback that reads original `.class` bytes off the
  real classpath for non-AOT-generated classes) does not actually produce a
  loader-isolated duplicate under CratonVM — i.e. `defineClass` may be
  **collapsing by name** into the existing global registration instead of
  creating a second, independent `ClassId` scoped to the new defining
  loader, **or**
- there is a *second*, not-yet-traced resolution path (distinct from both
  `resolve_class_loader_aware`'s `New`-instruction path and
  `cl_real_load_class_base`'s `loadClass` path) through which
  `SpringFactoriesEnvironmentPostProcessorsFactory` specifically gets
  resolved, that this investigation didn't reach before time/scope ran out.

**Not confirmed which of the two** — the investigation stopped after
confirming the `builtin_loader_reachable` fix is real and JVMS-correct (kept,
see FIXED doc) but insufficient alone. The most likely next step is tracing
`ClassLoader.defineClass`'s CratonVM-native implementation (real-JDK mode)
to check whether it deduplicates/collapses by binary name across distinct
defining-loader instances, which would be the "flat global class store"
architecture's most fundamental manifestation of this whole bug family (this
same family already required THREE separate, narrowly-scoped patches
elsewhere in the codebase for different call sites —
`loader_interface_override` in `interpreter.rs`, the `container_loader`
threading in `lang_class.rs`'s annotation-enum arm, and now
`builtin_loader_reachable` — suggesting the flat-store model may need a more
systemic fix rather than continuing to patch individual call sites as they
surface).

## Reproduction

```bash
# On the Azure host, core/spring-boot-test module of a compiled spring-boot
# checkout (e.g. /data/data/springboot-jsonreader-deprecation-20260718):
CP=$(cat build/cratonvm-test-cp.txt)
CP="build/classes/java/test:build/classes/java/main:build/resources/test:build/resources/main:../../sb-runner:$CP"
cratonvm --java-home /home/victor/jdk25 -cp "$CP" SbRunner \
  org.springframework.boot.test.context.SpringBootContextLoaderAotTests
```

## Not affected

The 25-class regression sample run for the FIXED doc above (spanning
`context`, `json`, `system`, `web`, `mock`, `filter`, `assertj`,
`bootstrap`, `runner` subpackages) passed 25/25 with all three fixes
applied — this residual is narrowly scoped to
`SpringBootContextLoaderAotTests`'s specific AOT-generation +
`@CompileWithForkedClassLoader` combination, not a broad regression risk.
