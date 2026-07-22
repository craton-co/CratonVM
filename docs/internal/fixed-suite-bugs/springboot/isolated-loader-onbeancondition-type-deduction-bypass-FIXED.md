# `@ClassPathExclusions`-excluded jar remains reachable during bytecode-level construction under an isolated loader — `OnBeanCondition` type-deduction test fails to observe the expected `NoClassDefFoundError`

**Status: FIXED — resolved 2026-07-19, same day as filed.** Residual of
[`modifiedclasspath-aether-network-hang-cluster-FIXED.md`](modifiedclasspath-aether-network-hang-cluster-FIXED.md).
Filed as OPEN after observing the failure against a binary built before
merging ~106 commits of `origin/dev` drift into the fix branch; after that
merge (which pulled in unrelated, already-landed fixes from other concurrent
sessions working the same repo) and a rebuild,
`OnBeanConditionTypeDeductionFailureTests` passes **1/1** and
`EhCache3CacheAutoConfigurationTests` (the "possibly related" class noted
below) passes **2/2**. Not independently root-caused — resolved as a side
effect of the drift merge, exact fixing commit not identified. Kept here
(rather than deleted) as a record of the symptom, the standalone-probe
narrowing (exclusion filtering itself works; the gap was specific to
bytecode-level construction under the real Spring/JUnit path), and the
hypothesis, in case it regresses.

## Symptom

`core/spring-boot-autoconfigure`'s `OnBeanConditionTypeDeductionFailureTests
.conditionalOnMissingBeanWithDeducedTypeThatIsPartiallyMissingFromClassPath()`
(`@ClassPathExclusions("jackson-core-*.jar")`) fails:

```
java.lang.AssertionError:
Expecting code to raise a throwable.
	at org.springframework.boot.autoconfigure.condition.OnBeanConditionTypeDeductionFailureTests
		.conditionalOnMissingBeanWithDeducedTypeThatIsPartiallyMissingFromClassPath(OnBeanConditionTypeDeductionFailureTests.java:46)
```

The test does `assertThatException().isThrownBy(() -> new
AnnotationConfigApplicationContext(ImportingConfiguration.class).close())` —
i.e. it expects `tools.jackson.databind.ObjectMapper`'s bean-factory method
(`OnMissingBeanConfiguration.objectMapper()`, `return new ObjectMapper();`)
to fail to construct because `jackson-core` (needed transitively by
`ObjectMapper`) was excluded from the classpath, wrapped as a
`BeanTypeDeductionException` with a nested `NoClassDefFoundError`. On
CratonVM, **no exception is raised at all** — `new ObjectMapper()` completes
successfully despite the exclusion. Reproduced with `cratonvm.exe`
(`sb-runner` harness) in worktree
`CratonVM-aether-modifiedclasspath-20260718-019f753a`:
`SBRUNNER_RESULT tests=1 failed=1`.

## Confirmed: the exclusion mechanism itself is not the bug

A standalone probe (`ExclProbe.java`, replicating `ModifiedClassPathClassLoader`'s
exact URL-filtering + `AntPathMatcher` glob logic by hand, then calling
`Class.forName("tools.jackson.core.JsonFactory", false, isolatedLoader)` and
`ObjectMapper.class.getDeclaredConstructor().newInstance()` through a
hand-built isolated `URLClassLoader`) shows:

- `ManagementFactory.getRuntimeMXBean().getClassPath()` (which
  `ModifiedClassPathClassLoader.extractUrls()` uses when the parent isn't
  itself a `URLClassLoader` — true for both engines here, since
  `ClassLoader.getSystemClassLoader()` is `jdk.internal.loader.ClassLoaders
  $AppClassLoader` on both HotSpot and CratonVM under JDK 25) returns
  identical entries (86) on both engines, correctly including
  `jackson-core-3.1.3.jar`.
- The exclusion filter correctly removes it (85/86 URLs kept) on both
  engines.
- `Class.forName("tools.jackson.core.JsonFactory", false, isolated)`
  correctly throws `ClassNotFoundException` on **both** engines.
- `new ObjectMapper()` **constructed reflectively**
  (`getDeclaredConstructor().newInstance()`) through the hand-built isolated
  loader correctly fails on both engines — CratonVM throws a raw
  `ClassNotFoundException`, HotSpot throws `NoClassDefFoundError: tools/
  jackson/core/TreeCodec` (expected: `Class.forName`/reflective construction
  is documented to surface the loader's checked `ClassNotFoundException`
  directly rather than the bytecode-linkage `NoClassDefFoundError` wrapping —
  this exception-type difference between the two engines in the standalone
  probe is very likely benign and not the bug under investigation here).

This rules out the exclusion/filtering logic, `getRuntimeMXBean()`, and
basic isolated-`URLClassLoader` class resolution as the cause — all behave
correctly in isolation. **The gap is specific to the real
`ModifiedClassPathExtension` + Spring `@Bean`-method-invocation path**,
which resolves `tools/jackson/core/TreeCodec` from *inside* `ObjectMapper`'s
own constructor bytecode (a `NEW`/field-type reference triggered while the
interpreter executes already-loaded `ObjectMapper`'s `<init>`), not via
`Class.forName` or reflective `Constructor.newInstance()` on the excluded
class directly.

## Root-cause hypothesis (not confirmed by a debugger attach or trace)

Bytecode-level class resolution for a reference made from inside an
already-loaded class goes through `resolve_class_loader_aware`
(`vm/src/runtime/interpreter.rs`), which — when the referencing class's
defining loader is a "user-defined" loader (true here: `ModifiedClassPathClassLoader`)
and `env_cache::loader_aware_resolution()` is enabled (true by default) —
takes the "gate-on" path: try `drive_defining_loader_load` (invokes the
defining loader's own `loadClass`) first, and only if *that* fails **and**
`is_isolated_url_loader_definition` recognizes the loader as isolated, throw
`NoClassDefFoundError` instead of falling through to
`shared.load_class_concurrent` (the global, process-wide classpath — which
still has the "excluded" `jackson-core-3.1.3.jar` on it via the full `-cp`
passed to the `cratonvm.exe` process; exclusion is a Java-level, per-loader
URL-list filter only, never removes the jar from the underlying OS-process
classpath). Reading this path suggests it *should* already produce the
correct `NoClassDefFoundError` for our exact scenario — which does not match
the observed "no exception raised at all" result, so either:

- `should_use_loader_initiated_resolution`/`is_isolated_url_loader_definition`
  isn't returning what the code reading above predicts for this specific
  `referencing_class_id` (e.g. a caching layer such as
  `initiating_resolution_cache` or `lookup_loader_initiated`'s fast path
  returns a stale/global answer before the isolated-aware branch is ever
  reached), or
- `TreeCodec` resolution isn't actually happening through
  `resolve_class_loader_aware` at all in this code path (e.g. a different
  linking/verification-time resolution route bypasses it), or
- `ObjectMapper`'s actual bytecode execution under CratonVM's interpreter/JIT
  doesn't hit the same field/constant-pool reference to `TreeCodec` that
  HotSpot's verifier + interpreter do, for unrelated JIT/interpreter
  correctness reasons.

## Possibly related residuals (not confirmed to share this root cause)

- `EhCache3CacheAutoConfigurationTests` fails with `IllegalStateException:
  @ConditionalOnMissingBean did not specify a bean using type, name or
  annotation` — a *different* symptom (deduced bean type set comes back
  completely empty rather than a raised-then-uncaught exception), from the
  same `OnBeanCondition` machinery (`OnBeanCondition.java` `Spec` constructor
  / `deducedBeanTypeForBeanMethod`). Worth checking together with this doc
  since both trace through `OnBeanCondition`'s reflective type-deduction
  path, but not confirmed to be the same bug.
- See [`isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md`](isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md)
  for a separate `ObjectProvider<X>` generic-identity residual in the same
  overall "isolated classloader + reflection" family — plausibly a sibling
  bug, not confirmed to share a single mechanism with this one.

## Suggested next step

Add a temporary trace (e.g. `tracing::warn!`) in
`resolve_class_loader_aware`/`lookup_loader_initiated`/
`is_isolated_url_loader_definition` gated on the referenced name containing
`"jackson"`, rebuild, and re-run
`OnBeanConditionTypeDeductionFailureTests` directly (not via the standalone
probe) to see which branch actually resolves `tools/jackson/core/TreeCodec`
successfully. That will confirm or refute the caching-bypass hypothesis
above and point at the exact line to fix.
