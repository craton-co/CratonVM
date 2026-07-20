# core/spring-boot-test — config-data loading gaps, duplicate classpath scan results, missing PropertySource

**Status: PARTIALLY FIXED (2026-07-20) — Clusters A, B, D, E confirmed fixed upstream (verified against dev tip, HotSpot-matching PASS); Cluster C has 2 newly root-caused-and-fixed CratonVM bugs plus 1 further OPEN residual.**

Originally six `core/spring-boot-test` classes failing/hanging via 5 distinct signatures, found 2026-07-17 and none root-caused at the time. Re-verified 2026-07-20 in worktree `fix/sb-configdata-classpath-scan-cluster-20260719` (branched from dev `54003fb83`, merged forward to dev `91ee66ca7`): the doc was stale — dev had already fixed Clusters A, B, D, and E independently since 2026-07-17. Only Cluster C's original signature was also stale (already-changed failure) and needed fresh investigation, which found two genuine CratonVM interpreter/native bugs (now fixed) plus one further, deeper residual (still open).

## Cluster A — classpath-root `application.properties` config-data not composed into Environment — FIXED (upstream, before this session)

| Class | Failure (as originally documented 2026-07-17) |
|---|---|
| `ConfigDataApplicationContextInitializerTests` | `expected: "bucket" but was: null` |
| `ConfigDataApplicationContextInitializerWithLegacySwitchTests` | same shape |

**Verified 2026-07-20: both PASS** against current dev (`environment.getProperty("foo")` correctly returns `"bucket"`). Not re-root-caused — some other, unrelated dev commit(s) between 2026-07-17 and 2026-07-20 fixed the classpath-root config-data composition path. No action needed.

## Cluster B — `@Value` placeholder left unresolved — FIXED (upstream, before this session)

`SpringBootTestCustomConfigNameTests`: originally `expected: "bar" but was: "${test.foo}"`.

**Verified 2026-07-20: PASSES** against current dev. Not re-root-caused. No action needed.

## Cluster C — `SpringBootContextLoaderAotTests` — 2 CratonVM bugs found+fixed this session; 1 residual still OPEN

The original 2026-07-17 signature (`IllegalStateException: Found multiple @SpringBootConfiguration annotated classes` via `AnnotatedClassFinder.scanPackage`) **no longer reproduces** — it was also stale. Re-running against current dev surfaces a completely different failure chain, root-caused to two distinct CratonVM interpreter/native bugs (both now fixed on branch `fix/sb-configdata-classpath-scan-cluster-20260719`, merge pending) plus a third, deeper residual.

Both fixed bugs are instances of the same general class of problem: **`SpringBootContextLoaderAotTests` is annotated `@CompileWithForkedClassLoader`**, which re-runs the entire test method inside a second, isolated `ClassLoader` (`CompileWithForkedClassLoaderClassLoader`) so AOT-generated sources can be freshly compiled and loaded. CratonVM's various "loader-aware resolution" mechanisms (built up incrementally across several `@CompileWithForkedClassLoader`/`@ClassPathOverrides` bug fixes referenced elsewhere in `docs/internal/spring/CRATONVM-SPRING-GENUINE-BUGLIST.md`) don't uniformly cover every place a `Class` identity crosses a loader boundary.

### Bug 1 (FIXED): interface-default-method dispatch bypassed a receiver's own override

`vm/src/runtime/interpreter.rs`, the `loader_interface_override` block (~line 20000, inside `execute_invoke_kind`): when an `invokeinterface` call's receiver's defining loader has its *own* copy of the CP-resolved interface, dispatch was unconditionally redirected to resolve against *that* interface's class_id — even when the receiver's own class hierarchy already declares a concrete (non-default) override of the exact method. That skipped straight past the override to the interface's own default-method body.

Concretely: `TestContextAotGenerator` calls `AotContextLoader.loadContextForAotProcessing(MergedContextConfiguration, RuntimeHints)` on a `SpringBootContextLoader` instance (loaded by the fork's classloader). `SpringBootContextLoader` overrides that exact 2-arg method (`SpringBootContextLoader.java:118-121`) but not the interface's legacy 1-arg default. Dispatch landed on `AotContextLoader`'s own 2-arg default body instead (`AotContextLoader.java`'s default just calls the 1-arg default, which unconditionally throws `UnsupportedOperationException("Invoke loadContextForAotProcessing(MergedContextConfiguration, RuntimeHints) instead")`).

**Fix**: before applying the loader-exact interface redirect, `find_method_recursive` is now run against the receiver's own `class_id` first; the redirect only fires when that resolves to an interface (i.e. the receiver would fall through to the default anyway). See the commit for the full comment/rationale.

### Bug 2 (FIXED): annotation-default `Enum` values resolved via a loader-blind global lookup

`native-builtins/src/lang_class.rs`, `annotation_element_to_java_typed`'s `AnnotationElementValue::Enum` branch: unlike its sibling `Class`-valued branch (which already resolves through `container_loader` — the annotation's declaring class's own loader — when present), the `Enum` branch always used the loader-blind global `ctx.class_id_by_name(class_name)`.

Concretely: `@SpringBootTest`'s `useMainMethod()` attribute defaults to `UseMainMethod.NEVER`. Under the fork, CratonVM's default-value resolution fetched the enum constant via the *global* table, which returned a same-named but reference-*unequal* `NEVER` constant relative to the fork-loader's own copy that `SpringBootContextLoader.getMainMethod`'s bytecode compares against with `==`. The comparison silently failed, `useMainMethod` behaved as `ALWAYS` (enum ordinal 0), and `getMainMethod` threw `IllegalStateException: Main method not found on 'SpringBootContextLoaderAotTests$ExampleConfig'`.

**Fix**: the `Enum` branch now mirrors the `Class`-valued branch — resolves via `resolve_annotation_class_via_loader(ctx, container_loader, class_name)` first, falling back to the global lookup only when no `container_loader` is available or the loader-scoped resolution fails.

### Residual (OPEN): `SpringFactoriesEnvironmentPostProcessorsFactory` ends up defined by a different loader than sibling SPI classes it instantiates

After both fixes above, `SpringBootContextLoaderAotTests` progresses substantially further into `SpringApplication`'s real bootstrap, then fails with:

```
java.lang.NullPointerException: Cannot invoke "org.springframework.boot.logging.DeferredLogFactory.getLog(java.lang.Class)" because "logFactory" is null
	at org.springframework.boot.cloud.CloudFoundryVcapEnvironmentPostProcessor.<init>(CloudFoundryVcapEnvironmentPostProcessor.java:106)
```

via `SpringFactoriesLoader$FactoryInstantiator.instantiate` → `resolveArgs` → `SpringFactoriesLoader.ArgumentResolver` (real, unmodified spring-core-7.0.7 bytecode). `ArgumentResolver.of(DeferredLogFactory.class, logFactory)` matches constructor parameter types by `Class.equals` (== identity, `Class` doesn't override `equals`). Added temporary tracing (removed before commit; reproducible by re-adding a debug print at the same two call sites) showed:

- `SpringFactoriesEnvironmentPostProcessorsFactory` (the class holding the `ldc DeferredLogFactory.class` literal via `ArgumentResolver.of`) is defined by the **Application** (system) loader.
- `CloudFoundryVcapEnvironmentPostProcessor` (an `EnvironmentPostProcessor` SPI implementation, discovered and instantiated via the same `SpringFactoriesLoader` call) is defined by **`UserDefined(3)`** — the fork's own loader.

These are two *different* `DeferredLogFactory` class identities, so `Class.equals` fails and the constructor argument resolves to `null`.

Per `CompileWithForkedClassLoaderClassLoader`'s real bytecode (`org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader`, decompiled from `spring-core-test-7.0.7.jar`), its constructor sets `super(testClassLoader.getParent())` — i.e. its OWN parent is the *grandparent* of the original test class's loader, not the test loader itself — and `loadClass` delegates everything except `org.junit`/`org.testng` straight to `super.loadClass()` (standard parent-first `ClassLoader` delegation, falling to the fork's own `findClass` — which loads from the freshly-compiled-in-memory AOT sources — only when the parent chain can't resolve it). Under this scheme, for a flat single-classloader launch (which is how `SbRunner`/this suite runs, with `-cp` and no Gradle-worker-style loader layering), *both* `SpringFactoriesEnvironmentPostProcessorsFactory` and `CloudFoundryVcapEnvironmentPostProcessor` should end up defined by the fork loader (since the grandparent-only chain can't resolve either from the ordinary classpath) — so `SpringFactoriesEnvironmentPostProcessorsFactory` showing up as Application-loader-defined instead looks like the actual bug: it (or whatever earlier call resolved/instantiated it) took a shortcut back to an already-loaded, stale Application-loader copy instead of going through this classloader's real (parent-then-self) delegation.

**Not yet root-caused to a specific file:line.** Next step: trace backward from wherever `new SpringFactoriesEnvironmentPostProcessorsFactory(...)` (in real `EnvironmentPostProcessorsFactory.fromSpringFactories`, spring-boot core) executes — i.e. find the `New`/class-resolution call that decided `SpringFactoriesEnvironmentPostProcessorsFactory`'s class_id, and check whether it went through the loader-faithful path or a name-collapsed one. Likely candidates: whatever mechanism populates `crate::classloader::defining_loader_for`/the per-loader exact-class index for a class loaded via a bare (non-`URLClassLoader`) `ClassLoader.loadClass()`/`defineClass()` call chain like this one, versus the more commonly-exercised `URLClassLoader`-based forks (`ModifiedClassPathClassLoader`, used by `@ClassPathOverrides`/`@ForkedClassPath`).

Log: `apps/spring-boot-suite-runner/.suite/results/configdata-classpath-scan-postmerge/all-jit/logs/core_spring-boot-test.org.springframework.boot.test.context.SpringBootContextLoaderAotTests.out.log`.

## Cluster D — missing `"random"` PropertySource — FIXED (upstream, before this session)

`SpringBootContextLoaderTests.propertySourceOrdering()` (1 of 26 tests in the class).

**Verified 2026-07-20: all 26/26 tests PASS**, including `propertySourceOrdering` (`"random"` now correctly present in the property-source list). Not re-root-caused. No action needed.

## Cluster E — `DuplicateJsonObjectContextCustomizerFactoryTests` HANG — REFUTED (does not reproduce)

The originally-hypothesized "regression-in-place-of-fix" (SSLSocketFactory fix causing a real-but-blocked network hang instead of the original crash) does **not** reproduce. **Verified 2026-07-20: PASSES in 1.3-2.5s**, no hang. The `org.json:json:20140107` coordinate used by `@ClassPathOverrides` on this test is already present in `~/.m2/repository` (with a matching `_remote.repositories` marker for `central`), and Aether resolves it from the local cache without needing network I/O in this environment. No action needed; the earlier hang was likely either already fixed alongside another SSL/TLS fix landed between 2026-07-17 and now, or was itself a transient/environmental artifact of that specific rerun.

## Affected classes (updated status)

- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `ConfigDataApplicationContextInitializerWithLegacySwitchTests` — **FIXED** (Cluster A)
- `core/spring-boot-test` | `SpringBootTestCustomConfigNameTests` — **FIXED** (Cluster B)
- `core/spring-boot-test` | `SpringBootContextLoaderAotTests` — **OPEN residual** (Cluster C; 2 of ~3 known bugs fixed this session)
- `core/spring-boot-test` | `SpringBootContextLoaderTests` — **FIXED**, 26/26 (Cluster D)
- `core/spring-boot-test` | `DuplicateJsonObjectContextCustomizerFactoryTests` — **FIXED** / does not reproduce (Cluster E)
