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

## Regression note — confirmed still failing 2026-07-23 (craton-rerun-20260723), but with a NEW, different symptom

`EhCache3CacheAutoConfigurationTests` (the "possibly related" class noted
above) is FAILing again as of the 2026-07-23 rerun, but **not** with this
doc's `@ConditionalOnMissingBean did not specify a bean` symptom — both its
test methods now fail with a JUnit Platform `DiscoveryIssueException`
("`UniqueIdSelector [...] could not be resolved`") during test *discovery*,
before either test body runs. Root-caused as a new doc:
[`modifiedclasspathextension-nested-launcher-uniqueid-discovery-failure-FIXED.md`](modifiedclasspathextension-nested-launcher-uniqueid-discovery-failure-FIXED.md).
`OnBeanConditionTypeDeductionFailureTests` itself was not part of this
session's assigned batch and was not re-checked.

## Regression note (2026-07-23) — "FIXED" was never a real fix; the underlying `OnBeanCondition`/isolated-loader gap is back

**`OnBeanConditionTypeDeductionFailureTests.conditionalOnMissingBeanWithDeducedTypeThatIsPartiallyMissingFromClassPath`
fails again** in the `RunName=craton-rerun-20260723` rerun
(`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.condition.OnBeanConditio-ef3749960ee5.out.log`).
This doc's own status line already flagged the risk: "resolved as a side
effect of the drift merge, exact fixing commit not identified" — i.e. it was
never actually root-caused or fixed at the code level, just observed passing
after an unrelated ~106-commit merge. 4 days and presumably more `dev` drift
later, it fails again — **not silently re-filed, treated as this doc's
original bug recurring.**

The *current* symptom is narrower than originally described, worth noting
explicitly since the doc's stated mechanism may no longer be the accurate
description: back in 2026-07-19, `new ObjectMapper()` **silently succeeded**
(no exception at all). Now, an exception **is** raised — but it's the wrong
shape for the test's `.satisfies(...)` assertions:

```
16:13:52.600 [main] WARN ... AnnotationConfigApplicationContext -- Exception encountered during context initialization ...
  org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'objectMapper' ...:
  Failed to instantiate [tools.jackson.databind.ObjectMapper]: Factory method 'objectMapper' threw exception with message: tools.jackson.databind.ObjectMapper
=> org.assertj.core.error.AssertJMultipleFailuresError
```

The test expects a nested `OnBeanCondition.BeanTypeDeductionException` (from
`OnBeanCondition`'s own reflective return-type deduction, thrown *before*
the factory method is ever invoked) wrapping a `NoClassDefFoundError`. What
actually happens now is different: `OnBeanCondition`'s deduction apparently
succeeds (no `BeanTypeDeductionException` in the chain at all — the failure
is a plain `BeanCreationException`/"Factory method ... threw exception"),
meaning the `objectMapper()` factory method itself gets invoked and *some*
exception is thrown from inside real `ObjectMapper` construction — but its
message is suspiciously just the bare string `"tools.jackson.databind.ObjectMapper"`,
i.e. `ObjectMapper`'s **own** class name, not the missing/excluded
`jackson-core` class it should be failing to resolve. That message shape
does not match either engine's expected `NoClassDefFoundError` text
(HotSpot would name the actually-missing class, e.g. `tools/jackson/core/TreeCodec`)
and is not investigated further here — flagged as a narrower, possibly
distinct-mechanism variant of this doc's original gap, not assumed identical
without confirmation.

**A second, related class regressed too:**
`module/spring-boot-security-oauth2-authorization-server`'s
`OAuth2AuthorizationServerAutoConfigurationTests.autoConfigurationDoesNotCauseUserDetailsServiceToBackOff`
now fails with exactly the symptom this doc's "Possibly related residuals"
section already flagged for `EhCache3CacheAutoConfigurationTests` (deduced
bean type set comes back empty, not a raised-then-uncaught exception):

```
Caused by: java.lang.IllegalStateException: @ConditionalOnMissingBean did not specify a bean using type, name or annotation
     at org.springframework.boot.autoconfigure.condition.OnBeanCondition$Spec.validate(OnBeanCondition.java:655)
     at org.springframework.boot.autoconfigure.condition.OnBeanCondition$Spec.<init>(OnBeanCondition.java:603)
     at org.springframework.boot.autoconfigure.condition.OnBeanCondition.getMatchOutcome(OnBeanCondition.java:147)
```

Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard8/logs/module_spring-boot-security-oauth2-authorization-server.org.springframework.boot.security.oaut-9461ed871e55.out.log`.
This class was **not** previously listed as affected by this doc or its
sibling `isolated-loader-objectprovider-generic-identity-mismatch-FIXED.md`
(both docs' "Affected classes"/regression scope predate this class's
appearance in this exact failure shape) — added here as the same
`OnBeanCondition` type-deduction family, not confirmed to share the exact
same code-level cause as the primary class above (different symptom:
deduction returns empty vs. deduction+construction throwing the wrong
exception shape), but clearly the same general "isolated-loader +
`OnBeanCondition` reflective deduction" gap this doc already tracks as
unresolved.

**Net effect: this doc's "FIXED" status is not trustworthy for either class
going forward** — both should be treated as OPEN until someone does the
`tracing::warn!`-based investigation the "Suggested next step" above already
called for, which never happened.
