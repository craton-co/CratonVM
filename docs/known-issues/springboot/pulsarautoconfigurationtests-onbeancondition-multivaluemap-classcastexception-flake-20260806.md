# `PulsarAutoConfigurationTests` — intermittent `ClassCastException: Object cannot be cast to MultiValueMap` inside `OnBeanCondition$Spec`, 2026-08-06

**Status: OPEN — reproduced once; 6 further attempts clean. The prescribed
site-alias census has now been RUN, and it is not quiet.**

**2026-08-06 update.** Two things changed, neither of them a fix:

* The `spring-bean-attribute-type-null-flake` cause (recycled-`JitInvokeInfo`
  aliasing, `383e7f5cf`) is **ruled out by ancestry** — see the OnBeanCondition
  comparison below. This flake survived that fix.
* 4 more attempts, 4 lanes concurrent on the Windows box: **74/74 clean each**
  (`tests=74 failed=0 skipped=2`). With the original 1-in-2, that is 1 failure
  in 6 known attempts. Concurrency alone does not raise the rate the way it does
  for the OAuth2 read-timeout flake, so whatever the trigger is, it is not
  simple load.

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
Not disambiguated — would need many more repro attempts (the
`spring-bean-attribute-type-null-flake` doc needed ~130 hunt runs to
characterize a comparably rare flake) to even get a hit rate, let alone
root-cause it live.

**Read this before spending runs on the GC framing.** That page has since been
resolved, and *not* as a GC defect: it was the recycled-`JitInvokeInfo`
dispatch defect (`383e7f5cf`), where a site key freed with its `CompiledMethod`
and re-issued let one call site return another's answer — see
`fixed-suite-bugs/springboot/spring-boot-annotation-metadata-null-cluster-RESOLVED-20260806.md`.
Two things follow for this page. First, its ~1030 instrumented hunt runs found
nothing because both detectors watched the map and the map was innocent — a hit
rate is not worth buying with runs while the instrument points at the wrong
layer. Second, `CRATONVM_DBG_SITE_ALIAS=1` measures the *precondition* of that
defect rather than its rare corruption, so it answers "does this workload
alias, and at which call sites?" in a **single** run. For a `Stream.collect`
result failing a checkcast that is the cheapest first question. Caveat: the fix
is already on `dev`, so a positive alias census on current `dev` says the
workload recycles keys, not that it is still corrupting — testing the defect
itself needs a pre-`383e7f5cf` anchor binary.

## Not the same bug as the other open `OnBeanCondition` docs

Checked against both existing `OnBeanCondition`-family docs before filing
this as new:

- `isolated-loader-onbeancondition-type-deduction-bypass-FIXED.md` (repeatedly
  reopened) — different symptom shape entirely (`BeanTypeDeductionException`/
  empty-deduction/wrong-exception-message, tied to `@ClassPathExclusions`
  isolated classloaders). This class doesn't use classpath exclusion.
- `spring-bean-attribute-type-null-flake` (now
  `fixed-suite-bugs/springboot/spring-bean-attribute-type-null-flake-RESOLVED-20260806.md`)
  — same general "rare, one-shot, Spring reflection/annotation-processing miss"
  family and same `OnBeanCondition.Spec` constructor neighborhood, but a
  different concrete failure: an `IdentityHashMap` primitive-wrapper lookup
  returning `null` inside `TypeMappedAnnotation.adaptForAttribute`, not a
  `Stream.collect` result failing a checkcast. Not folded in — different code
  path, different exception class, no confirmed shared cause.

  **Ruled out 2026-08-06 by ancestry, not by argument.** That page resolved to
  the recycled-`JitInvokeInfo` aliasing defect (`383e7f5cf`, landed 08-05
  13:22), whose signature is exactly "a call returns the wrong object" — so it
  was a live candidate here. It is not: the binary this Pulsar failure was
  found on was built from `origin/dev @ 65a3085f5` (08-05 22:12), which
  **contains** that fix (`git merge-base --is-ancestor 383e7f5cf 65a3085f5`).
  The flake survived it. An earlier revision of this section argued the
  opposite from symptom shape; one ancestry check settles it, and it is the
  check to run FIRST whenever a symptom resembles a known dispatch bug.

## Affected classes

- `module/spring-boot-pulsar` — `org.springframework.boot.pulsar.autoconfigure.PulsarAutoConfigurationTests$SchemaResolverTests.whenHasUserDefinedBeanDoesNotAutoConfigureBean`
  (1 failure in 2 attempts; all other 73 tests in the class pass)

Log:
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-20260806-s5/all-jit/logs/module_spring-boot-pulsar.org.springframework.boot.pulsar.autoconfigure.PulsarAutoC-72a68e29781a.{out,err}.log`

## The site-alias census has been run — NOT quiet (2026-08-07)

Ran the class once under `CRATONVM_DBG_SITE_ALIAS=1` on a binary containing
`383e7f5cf` (`cratonvm-ovlbatch0806`, dev + the overlay GC fixes).

* **Verdict: `tests=74 failed=0 aborted=0 skipped=2`** — no repro. That makes it
  **1 failure in 7 known attempts**.
* **Census: loud.** 1007 site keys observed; the printer hit its 40-line cap and
  emitted `(further hits are counted, not printed)`. Key recycling is pervasive
  in this workload, not marginal.
* **The aliased keys include the annotation-reading sites this bug runs on:**

  | key | was | now |
  |---|---|---|
  | `…f1340` | `AnnotatedElement.getDeclaredAnnotations()` | `asm/ClassReader.readUnsignedShort(I)I` |
  | `…f1740` | `AnnotatedElement.getDeclaredAnnotation(Class)` | `ConcurrentReferenceHashMap$Segment.getReference(…)` |
  | `…f1e40` | `AnnotatedElement.getAnnotations()` | `ConcurrentReferenceHashMap$Reference.get()` |
  | `…f6340` | `AnnotationTypeMappings.size()I` | `bytebuddy/utility/Invoker.invoke(…)` |

**What this does and does not establish.** The fix is already on this binary, so
per the caveat below a positive census says the workload **recycles keys**, not
that it is still corrupting — the instrument measures the *precondition*. It is
still the answer the page asked for: the census is not quiet, so by this page's
own decision rule the **dispatch framing is the one to pursue and the GC framing
can be deprioritised** without buying a hit rate. What is new is that the
precondition is dense at *exactly* the annotation-reading sites
`OnBeanCondition$Spec` uses to build the `MultiValueMap`.

The next question is therefore narrow: is there a site-keyed structure that does
**not** re-validate identity after `383e7f5cf`? That fix cleared the eight
site-keyed dispatch memos; anything else keyed on a `JitInvokeInfo` address
would still be exposed, and this workload would trigger it.

**Answered the same day: no, not in `vm/`.** Every `JitSiteKey`-keyed structure
is declared through the `site_keyed_memos!` macro, which *generates* the flush
alongside the declaration — so coverage is structural, not a list someone has to
remember to update. Enumerating `JitSiteKey`-keyed statics finds the eight memos
plus `SITE_IDENTITY`, which is the `CRATONVM_DBG_SITE_ALIAS` diagnostic map
itself and carries no dispatch decision. So the dense aliasing above is the
precondition being satisfied against a defence that is, as far as `vm/` goes,
complete — which makes "an uncovered site memo" the wrong place to look next and
leaves this page genuinely open rather than nearly-solved.

**Harness trap, cost one run of confusion.** The emitter writes `[site-alias]`
(hyphen); a census grep for `site_alias` (underscore) matched nothing and
reported "CENSUS QUIET", which reads exactly like the negative result that would
have sent this investigation to the GC framing. **Grep the emitter's literal tag,
and treat a clean negative as a harness bug until the instrument is shown to
speak.**

## Suggested next step

Run the class **once** under `CRATONVM_DBG_SITE_ALIAS=1` first. That names the
call sites whose `JitInvokeInfo` address was recycled, in one run, and costs
nothing; if `Stream.collect` / `Collector` / `MultiValueMap` sites appear in
that census, the dispatch framing is the one to pursue and the GC framing can
be deprioritised without buying a hit rate. Only if the census is quiet is it
worth looping the method N times under `CRATONVM_DBG_JIT_DISASM`/site-cache
tracing and separately under `CRATONVM_GC=-moving-young`. Doing it in that
order is the lesson from the resolved page: it spent ~1030 runs establishing a
hit rate for a mechanism its instruments could not see.
