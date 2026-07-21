# `@ConditionalOnMissingBean` misses a user bean under `@ClassPathExclusions`'s forked classloader

**Status: OPEN — found 2026-07-20**

## Symptom

`RestClientObservationAutoConfigurationWithoutMetricsTests` and
`RestTemplateObservationAutoConfigurationWithoutMetricsTests`
(`module/spring-boot-restclient`) both fail their single test with:

```
JUnit Jupiter:RestClientObservationAutoConfigurationWithoutMetricsTests:restClientCreatedWithBuilderIsInstrumented()
    => java.lang.IllegalStateException: Unstarted application context ...[startupFailure=org.springframework.beans.factory.support.BeanDefinitionOverrideException] failed to start
     Caused by: org.springframework.beans.factory.support.BeanDefinitionOverrideException: Invalid bean definition with name 'observationRegistry' defined in org.springframework.boot.micrometer.observation.autoconfigure.ObservationAutoConfiguration: @Bean definition illegally overridden by existing bean definition: Generic bean: class=io.micrometer.observation.ObservationRegistry; scope=singleton; ...
```

This is a **new** residual, discovered while verifying that the
`InterceptingExecutableInvoker` livelock fix
([`spring-boot-restclient-residuals-FIXED.md`](../../internal/springboot/spring-boot-restclient-residuals-FIXED.md))
holds — both classes previously HANG-timed-out before ever reaching this
code path, so this bug was invisible until the hang was fixed 2026-07-18.
Reproduces identically with `-Jit off` (`--nojit`), ruling out a JIT
speculation issue.

## Root cause (still not found — extensively narrowed, several strong hypotheses RULED OUT with live evidence)

The test context is:

```java
private final ApplicationContextRunner contextRunner = new ApplicationContextRunner()
    .withBean(ObservationRegistry.class, TestObservationRegistry::create)
    .withConfiguration(AutoConfigurations.of(ObservationAutoConfiguration.class, RestClientAutoConfiguration.class,
            RestClientObservationAutoConfiguration.class));
```

`ObservationAutoConfiguration.observationRegistry()` is
`@ConditionalOnMissingBean` (bean type inferred from its `ObservationRegistry`
return type — `apps/spring-boot/module/spring-boot-micrometer-observation/src/main/java/.../ObservationAutoConfiguration.java:68-71`).
With a bean of that exact type already registered via `.withBean(...)`, the
condition should skip registering the auto-configured
`observationRegistry()` bean entirely — and on real HotSpot it does (no
`BeanDefinitionOverrideException` is possible unless the condition
evaluates "missing" incorrectly, since `ConfigurationClassBeanDefinitionReader
.isOverriddenByExistingDefinition`, where this exception originates, only
runs for a bean method that condition evaluation already decided to keep).

**Isolated the trigger to `@ClassPathExclusions`, not the bean pattern
itself:** the sibling test `RestClientObservationAutoConfigurationTests`
(same module, same `.withBean(ObservationRegistry.class,
TestObservationRegistry::create)` call, same `AutoConfigurations.of(...)`
set, **no** `@ClassPathExclusions`) passes cleanly — 5/5 PASS, verified on
the same build. The only difference between the two test classes is
`@ClassPathExclusions("micrometer-core-*.jar")` on the failing ones, which
routes test execution through
`org.springframework.boot.testsupport.classpath.ModifiedClassPathExtension`
— a JUnit5 extension that builds a `ModifiedClassPathClassLoader` (a
`URLClassLoader` subclass excluding the named jar's packages), swaps the
thread's context classloader to it, and re-runs the entire test
method through a **new, nested** `Launcher.discover()`+`execute()` call
(`ModifiedClassPathExtension.runTest`) — the same mechanism responsible for
Issue A's now-fixed livelock in the sibling doc above, and for the
already-known nested-Launcher-execution complexity documented there.

### Diagnostic tooling added (kept, zero-cost when unset)

A `CRATONVM_DBG_OBSREG=1` env var enables a set of `eprintln!` traces added
this session, gated so they cost nothing in normal operation:

- `classloading/src/class_manager.rs` `define_class_with_options` — logs
  every `(name, loader_id)` a class named `ObservationRegistry`,
  `RestClientObservationAutoConfigurationWithoutMetricsTests`, or
  `TestObservationRegistry` gets defined under.
- `native-builtins/src/classloader.rs` `find_loaded_class_for_loader` — logs
  entry (this loader, name, is-user-defined) and exit (resolved mirror +
  `ClassId`) for any name containing `ObservationRegistry`.
- `native-builtins/src/classloader.rs` `cl_load_class_base_delegation` —
  same entry/exit wrap (the *synthetic-JDK-mode* `ClassLoader.loadClass`
  delegation path — see below, this path turned out NOT to be the one
  actually used in real-JDK-mode runs).
- `native-builtins/src/classloader_real.rs` `cl_real_load_class_base` (the
  *real-JDK-mode* counterpart, confirmed to be the actually-exercised path
  here) — logs the receiver, its class name, the raw `parent` field value,
  the parent's class name, `url_classloader_isolated_from_app(...)`'s
  result, and the `platform_loader_store` singleton pointer.
- `native-builtins/src/lang_class.rs` `native_class_is_assignable_from` —
  logs `(this, other)` class names + resolved `ClassId`s for any
  `isAssignableFrom` call involving a name containing `ObservationRegistry`.

Combined with the pre-existing `CRATONVM_FORNAME_TRACE=1` (already in
`native_class_for_name`), this gives full visibility into every mechanism
that could plausibly resolve `ObservationRegistry`'s `Class` identity for
this test. **Re-running this trace should be the first step of any
continuation** — it is far faster than re-deriving it from scratch.

### RULED OUT (with live evidence, this session)

1. **NOT a `cl_load_class_base_delegation` (synthetic-JDK-mode) parent-delegation
   bug.** That function was never entered at all for `ObservationRegistry` in
   this real-JDK-mode run (zero trace hits) — this box runs tests against a
   real JDK 25 (`-JdkHome`), which uses the *separate*
   `classloader_real.rs::cl_real_load_class_base` implementation instead. Any
   future investigation of "loadClass delegation" must instrument the REAL
   file, not the synthetic one — they are two independent, parallel
   implementations and it is easy to edit/trace the wrong one.

2. **NOT a naive `url_classloader_isolated_from_app` misclassification.**
   Traced `cl_real_load_class_base`'s entry for `ModifiedClassPathClassLoader`:
   `parent_field` is a real, non-null object whose class name genuinely IS
   `jdk/internal/loader/ClassLoaders$PlatformClassLoader`, confirmed against
   the tracked `platform_loader_store` singleton pointer (exact match, not a
   stale-pointer false-positive — GC remap hooks for this singleton
   (`gc_scan_loader_singleton_roots`/`gc_update_loader_singleton_refs`) exist
   and appear correctly wired). This is **by design**: Spring's own
   `ModifiedClassPathClassLoader.compute()` constructs the loader with
   `classLoader.getParent()` — i.e. the app loader's OWN parent (the Platform
   loader in the JDK 9+ three-tier hierarchy), NOT the app loader itself —
   deliberately isolating the forked loader from the app loader so its own
   (filtered) URL set is authoritative. `url_classloader_isolated_from_app`
   correctly identifies this topology; it is not the bug.

3. **NOT a stale/duplicate-`ObservationRegistry`-ClassId bug at the level
   traced.** `ObservationRegistry` IS defined twice — once under
   `Application` (from the OUTER, pre-classloader-swap JUnit test-instance
   construction, whose `contextRunner` field never actually gets `.run()`
   invoked on it — see next point) and once under `UserDefined(N)` (the
   isolated loader's own copy, matching real HotSpot behavior for a loader
   parented above the app loader) — but EVERY subsequent lookup
   (`find_loaded_class_for_loader`, `cl_real_load_class_base`, explicit
   `Class.forName(name, false, loader)` via `CRATONVM_FORNAME_TRACE`, and
   `Class.isAssignableFrom`) consistently and correctly resolves to the
   `UserDefined(N)` copy once it exists. No trace ever showed two DIFFERENT
   `ClassId`s being compared against each other for `ObservationRegistry`.

4. **Confirmed (not ruled out, but load-bearing for the above): the test
   class itself (`RestClientObservationAutoConfigurationWithoutMetricsTests`)
   is ALSO defined twice** — `Application` first (outer JUnit lifecycle
   instance, needed to host `interceptTestMethod`'s invocation-interceptor
   dispatch), then `UserDefined(N)` (the nested `Launcher`'s own fresh
   discovery+instantiation, per `ModifiedClassPathExtension.runTest`'s
   documented recursive-Launcher mechanism). Only ONE test result is
   ever reported (matching `interceptTestMethod` never calling
   `invocation.proceed()` — the outer instance's `@Test` body, and by
   extension its `contextRunner.run(...)`, never executes), so the actually-
   executing instance is the INNER one, whose OWN field initializer
   (`.withBean(ObservationRegistry.class, TestObservationRegistry::create)`)
   re-runs fresh under `UserDefined(N)`'s own loader — meaning its
   `ObservationRegistry.class` literal SHOULD, and per the traces above DOES,
   resolve to the same `UserDefined(N)` copy the auto-config classes use.
   **This is why the classloader-identity hypothesis, despite being the
   obvious first guess and matching the bug's outward shape, does not hold
   up under direct measurement** — everything traced is internally
   consistent.

5. **NOT the `CRATONVM_LOADER_AWARE_RESOLUTION` gate being off.** An earlier
   pass in this investigation misread a STALE doc comment in
   `interpreter.rs` claiming the gate "stays off pending a soak" — the
   authoritative implementation
   (`vm/src/runtime/env_cache.rs::loader_aware_resolution`) has defaulted to
   **ON** since the `context.groovy` bug-cluster fix. Loader-initiated
   `CONSTANT_Class` resolution (`ldc`/`new`/`checkcast`/`instanceof` from
   bytecode defined by a user-defined loader) is active by default; this is
   not a stale-default gap. (The doc comment inside
   `should_use_loader_initiated_resolution` in `interpreter.rs` should
   probably be corrected/removed since it no longer matches
   `env_cache.rs` — left as-is this session to avoid scope creep on an
   unrelated file.)

### One genuinely interesting, likely-tangential finding: non-loader-aware lambda/method-handle dispatch

`TestObservationRegistry` (`io.micrometer.observation.tck.TestObservationRegistry`,
the target of the `TestObservationRegistry::create` method-reference lambda
passed to `.withBean(...)`) **never appears in the `define_class` trace at
all** — it is never (re)defined under `UserDefined(N)`, unlike everything
else this test touches. Reading `vm/src/runtime/interpreter.rs` around lines
21267, 22164, and 22326 (`call_site.impl_handle.class_name` resolution for
invoking a lambda proxy's SAM implementation) shows these use
`shared.class_manager.read().get_loaded_class_id(name)` / `.load_class(name)`
— a **flat, name-only, NOT loader-aware** lookup — completely bypassing the
`resolve_class_loader_aware`/`drive_defining_loader_load` machinery that
ordinary bytecode class-references use (see point 5 above). This means a
method-reference/lambda's *implementation* class is resolved globally
regardless of which loader defined the lambda's call site — a real,
reproducible loader-identity gap, structurally identical in spirit to the
`context.groovy` bug the loader-aware-resolution gate was built to fix.

**However:** `.withBean(Class<T> type, Supplier<T> supplier)` takes the
*explicit* `type` argument (`ObservationRegistry.class`, confirmed correctly
loader-scoped per points 3-4 above) for the bean's registered
`ResolvableType` — the supplier lambda itself is never invoked until the bean
is actually instantiated (lazily, well after `@ConditionalOnMissingBean`
condition evaluation and the `BeanDefinitionOverrideException` this doc is
about would already have fired). So this finding is almost certainly **not**
the direct cause of the exception here, but it is a real, distinct,
worth-fixing bug in its own right — filed inline here rather than as a
separate doc since it was found as a byproduct of this investigation and
not yet confirmed to have its own live-failing test. Next step if picked up
separately: find/construct a test where a lambda's *implementation* class
identity (not just a `Class<T>` literal) needs to be loader-correct — e.g.
`lambda.getClass().getClassLoader()` or a loader-scoped `instanceof` check
against the impl class — under `@ClassPathExclusions`.

### Suggested next steps (in order of expected cheapness)

1. **Re-run the `CRATONVM_DBG_OBSREG=1` + `CRATONVM_FORNAME_TRACE=1` trace**
   (see tooling section above) against current `dev` to confirm all 5 RULED
   OUT points still hold (dev moves fast; re-verify before trusting old
   traces).
2. **Instrument Spring itself, not just CratonVM.** Everything traced on the
   CratonVM side is self-consistent, which points at either (a) Spring's OWN
   `OnBeanCondition`/`ConditionEvaluator`/`ConfigurationClassParser`
   bookkeeping doing something unexpected in this specific nested-Launcher
   scenario — quite possibly unrelated to CratonVM at all and instead a
   genuine, narrow interaction between `ModifiedClassPathExtension`'s
   recursive Launcher and Spring's per-`ConditionEvaluator`/per-context
   caching (worth checking whether this reproduces on real HotSpot too,
   which would reframe this as "not a CratonVM bug" entirely!), or (b) a
   CratonVM gap in a mechanism this session's traces didn't cover (ASM-based
   annotation/metadata reading specifically — `@ConditionalOnMissingBean`'s
   presence/attributes on `ObservationAutoConfiguration.observationRegistry()`
   were never directly verified to be read correctly when the class is
   loaded via this exact path). A quick, cheap check: temporarily add
   `System.err.println` calls (via a scratch patched copy of
   `ObservationAutoConfiguration.java` in the local `apps/spring-boot`
   checkout — it's not vendored read-only) around the `@ConditionalOnMissingBean`
   method to see whether it gets INVOKED at all (proving the condition WAS
   evaluated true) — trivial, no CratonVM rebuild needed.
3. ~~Confirm/refute against real HotSpot.~~ **DONE this session:** both
   classes PASS 1/1 on `-Vm hotspot` (`RestClientObservationAutoConfiguration
   WithoutMetricsTests` 2.285s, `RestTemplateObservationAutoConfiguration
   WithoutMetricsTests` 2.252s). Confirmed genuine CratonVM-specific bug, not
   an environmental/pre-existing upstream test issue — rules out reclassifying
   this as "not a bug."
4. Check whether other `@ClassPathExclusions` classes with a similarly-typed
   user-supplied singleton bean (`.withBean(SameTypeAsAutoConfiguredBean,
   ...)`) show the same failure — only these two were checked this session.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Jit on -TimeoutSec 120 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <a TSV with header "module`tclass" and the 2 rows below>
```
```
module	class
module/spring-boot-restclient	org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests
module/spring-boot-restclient	org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests
```

Control (should PASS, to confirm the `@ClassPathExclusions` isolation still
holds before further investigation):

```
module	class
module/spring-boot-restclient	org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationTests
```

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests` |
| `module/spring-boot-restclient` | `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests` |
