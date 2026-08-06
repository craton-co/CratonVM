# `PulsarAutoConfigurationTests` — intermittent `ClassCastException: Object cannot be cast to MultiValueMap` inside `OnBeanCondition$Spec`, 2026-08-06

**Status: OPEN — reproduced once, did not reproduce on immediate rerun.**

## Symptom

`module/spring-boot-pulsar`'s `PulsarAutoConfigurationTests` FAILed 1/74 on
the Windows-box 46-class non-passed rerun (242.7s, `tests=74 failed=1
aborted=0 skipped=2`), all other 73 tests in the same class — which exercise
the identical `OnBeanCondition`/`@ConditionalOnBean`/`@ConditionalOnMissingBean`
machinery on every autoconfiguration candidate — passed clean:

```
JUnit Jupiter:PulsarAutoConfigurationTests:SchemaResolverTests:whenHasUserDefinedBeanDoesNotAutoConfigureBean()
  => java.lang.AssertionError:
Expecting:
 <Unstarted application context ... startupFailure=java.lang.IllegalStateException>
to contain bean of type: <org.springframework.pulsar.core.SchemaResolver>
but context failed to start:
 java.lang.IllegalStateException: Error processing condition on org.springframework.boot.pulsar.autoconfigure.PulsarAutoConfiguration.pulsarReaderFactory
 	at org.springframework.boot.autoconfigure.condition.SpringBootCondition.matches(SpringBootCondition.java:60)
 	at org.springframework.context.annotation.ConditionEvaluator.shouldSkip(ConditionEvaluator.java:100)
 	...
 Caused by: java.lang.ClassCastException: java.lang.Object cannot be cast to org.springframework.util.MultiValueMap
 	at org.springframework.boot.autoconfigure.condition.OnBeanCondition$Spec.<init>(OnBeanCondition.java:583)
 	at org.springframework.boot.autoconfigure.condition.OnBeanCondition.getMatchOutcome(OnBeanCondition.java:147)
```

The failing line is `apps/spring-boot/core/spring-boot-autoconfigure/.../OnBeanCondition.java:583`:

```java
MultiValueMap<String, @Nullable Object> attributes = annotations.stream(annotationType)
    .filter(MergedAnnotationPredicates.unique(MergedAnnotation::getMetaTypes))
    .collect(MergedAnnotationCollectors.toMultiValueMap(Adapt.CLASS_TO_STRING));
```

i.e. `Stream.collect(Collector)` — using Spring's own custom
`MergedAnnotationCollectors.toMultiValueMap` collector, not one of
`java.util.stream.Collectors`' built-ins — returned something that is not
actually a `MultiValueMap` at the assignment site.

## A paired GC-guard hit, same target class — not treated as a verdict

The `.err.log` shows a `cratonvm::gc::guard` line ~1.4s before the failure,
naming the exact same class:

```
2026-08-06T01:52:41.878513Z ERROR cratonvm::gc::guard: checkcast receiver points into RECLAIMED memory —
  a still-referenced object was collected. `java.lang.Object` here is the all-zero header the collector
  left behind, not a real Object. obj="0x25ab0c96b20" location=young TO-space (the inactive semispace)
  span="0x25a94000000+0x0" target_class=org.springframework.util.MultiValueMap
```

This is a tempting but *not yet earned* GC theory — per
[[reference_reclaim_guard_hit_is_about_the_address_not_the_object]], the
guard's "RECLAIMED memory" ring is re-served by the allocator, so a hit is
evidence about the *address's* history, not proof the object at it now is
actually corrupt; a prior investigation on this exact repo
(`CacheAutoConfigurationTests`) led with the identical framing and it turned
out to be a JIT native-dispatch defect with zero collector involvement. **Not
confirmed either way here** — no fatal-error report's `gc young-gen actual:`
line was captured for this run (the process didn't crash, `SbRunner` just
recorded the caught `ClassCastException` as a normal test failure), so the
cheap refutations that doc lists (check the actual young-gen collection
count; `CRATONVM_GC=-moving-young`; larger `-Xmx`) have not been run.

## Reproducibility: does not reproduce on immediate rerun

Reran `PulsarAutoConfigurationTests` alone, same binary, same host,
immediately after: **74/74 PASS, 182.2s** (`pulsar-recheck-r1-20260806`).
One failure in two attempts. This rules out a deterministic/systemic
`Stream.collect`-with-custom-`Collector` dispatch bug (which would fail
every time, on every one of the ~74 identical-shape `OnBeanCondition.Spec`
constructions in the class, not 1 of 74) and points at a timing-sensitive
one-shot condition — consistent with either the GC-guard's own hypothesis or
a JIT tier-up/dispatch race, per the two candidate mechanisms already on file
for this general shape of bug ([[reference_reclaim_guard_hit_is_about_the_address_not_the_object]],
[[reference_native_census_diffed_jit_vs_nojit_names_a_dispatch_divergence]]).
Not disambiguated — would need many more repro attempts (the existing
`spring-bean-attribute-type-null-flake-20260803.md` doc needed ~130 hunt runs
to characterize a comparably rare flake) to even get a hit rate, let alone
root-cause it live.

## Not the same bug as the other open `OnBeanCondition` docs

Checked against both existing `OnBeanCondition`-family docs before filing
this as new:

- `isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md` (repeatedly
  reopened) — different symptom shape entirely (`BeanTypeDeductionException`/
  empty-deduction/wrong-exception-message, tied to `@ClassPathExclusions`
  isolated classloaders). This class doesn't use classpath exclusion.
- `spring-bean-attribute-type-null-flake-20260803.md` — same general
  "rare, one-shot, Spring reflection/annotation-processing miss" family and
  same `OnBeanCondition.Spec` constructor neighborhood, but a different
  concrete failure: an `IdentityHashMap` primitive-wrapper lookup returning
  `null` inside `TypeMappedAnnotation.adaptForAttribute`, not a
  `Stream.collect` result failing a checkcast. Not folded in — different
  code path, different exception class, no confirmed shared cause.

## Affected classes

- `module/spring-boot-pulsar` — `org.springframework.boot.pulsar.autoconfigure.PulsarAutoConfigurationTests$SchemaResolverTests.whenHasUserDefinedBeanDoesNotAutoConfigureBean`
  (1 failure in 2 attempts; all other 73 tests in the class pass)

Log:
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-20260806-s5/all-jit/logs/module_spring-boot-pulsar.org.springframework.boot.pulsar.autoconfigure.PulsarAutoC-72a68e29781a.{out,err}.log`

## Suggested next step

Loop this single test method (or the whole class) N times under
`CRATONVM_DBG_JIT_DISASM`/site-cache tracing and separately under
`CRATONVM_GC=-moving-young`, the way
`spring-bean-attribute-type-null-flake-20260803.md` did for its own rare
miss, to get a hit rate and determine whether the GC-guard framing or a
dispatch-cache framing is the real mechanism before attempting a fix.
