# `PulsarAutoConfigurationTests` — intermittent `ClassCastException: Object cannot be cast to MultiValueMap` inside `OnBeanCondition$Spec`, 2026-08-06

**Status: the two symptoms on this class have diverged. The
`ClassCastException` this page was opened for is ✅ FIXED (2026-08-07); the
deterministic early HANG documented below is 🔴 OPEN, and is what keeps this
page here.**

**The `ClassCastException` was `Stream.collect(Collector)` holding unpinned
references across a moving collection.** `collect_via_collector_protocol`'s
ordinary-`Collector` path — the one
`MergedAnnotationCollectors.toMultiValueMap` takes — held the accumulated
container, the collector, the accumulator, the finisher and every element as
raw `ObjectRef`s across five interpreter re-entries with no `pin_native_root`,
while the same function's other arm pinned all of them. A young collection in
that window handed the caller the container's pre-copy address, which reads
back as an all-zero header, i.e. as `java.lang.Object`. Verified with a
positive control (`probes/CollectorPinProbe.java`, A/B/B/A 296/300, 300/300,
300/300, 296/300; HotSpot 300/300) and 3 clean 74/74 runs of this class. Full
write-up:
`pulsar-onbeancondition-multivaluemap-stream-collect-pin-FIXED-20260807`
(retired).

**This page's GC framing was right, and it talked itself out of it.** It
recorded a `cratonvm::gc::guard` hit naming `MultiValueMap` and then set it
aside under the standing "a reclaim-guard hit is about the address, not the
object" caveat. That caveat is about *reused* addresses;
`location=young TO-space (the inactive semispace)` with `span=…+0x0` is a
different claim — nothing has been re-served there, so it is a reference that
was never remapped. The companion guard lines said so outright: *"the holder
is a frame local, a register, or a native side table — not a heap field"* and
*"`in_published_snapshot=false` … a root COLLECTION gap"*. Read the guard's
`location=` field before applying the caveat.

Everything below about the site-alias census and the dispatch framing stands as
a fact about the workload; it simply was not this bug.

## 2026-08-07 update: a SECOND, unrelated symptom on the same class — a deterministic early HANG, GC-backend-independent

The 2026-08-06/07 Windows full-suite runs (three separate `-Xmx 2g`, 300s/class
runs on the same box, one per GC backend) all HANG on this class instead of
producing the ClassCastException above:

| Run | GC | Result |
|---|---|---:|
| `craton-fullsuite-windows-20260806-s4` | Generational | TIMEOUT/HANG, 300.071s |
| `craton-fullsuite-g1-20260807-s4` | G1 | TIMEOUT/HANG, 300.101s |
| `craton-fullsuite-zgc-20260807-s4` | ZGC (real) | TIMEOUT/HANG, 300.148s |

This is **not** the ClassCastException flake re-manifesting as a hang — it is
a different symptom on the same class, confirmed independently before
assuming any connection (per this session's brief). Read on its own merits:

**Zero JUnit output at all** (`.out.log` is 0 bytes in all three runs — not
even the JUnit Platform launcher's "N containers found" banner) and the
**identical last `.err.log` line, byte-for-byte, in all three runs**:

```
WARN cratonvm_jit::x64::driver: JIT compile bailed: code buffer estimate too small; retrying at the measured size method="net/bytebuddy/implementation/bind/annotation/Argument$Binder.bind:(Lnet/bytebuddy/description/annotation/AnnotationDescription$Loadable;Lnet/bytebuddy/description/method/MethodDescription;Lnet/bytebuddy/description/method/ParameterDescription;Lnet/bytebuddy/implementation/Implementation$Target;Lnet/bytebuddy/implementation/bytecode/assign/Assigner;Lnet/bytebuddy/implementation/bytecode/assign/Assigner$Typing;)Lnet/bytebuddy/implementation/bind/MethodDelegationBinder$ParameterBinding;" code_len=244 capacity=62336 wanted=68331
```

Same method, same `code_len=244 capacity=62336 wanted=68331` numbers, in
all three runs — this is a deterministic stall point, not a random one. It
fires early (~70-90s into each run, right after the "Mockito is currently
self-attaching" banner and the `File fs/separator/pathSeparator` clinit-fixup
warning, well before any test output would normally appear for a 74-test
class), and then **total silence** for the remaining ~210-230s until the
watchdog kills the process — no further JIT warnings, no GC warnings, no
test progress of any kind. That absence of GC activity is itself informative:
a process genuinely doing 74 tests' worth of Mockito/ByteBuddy mock-class
generation for that long would be expected to allocate enough to trigger at
least one young-gen collection, and none of the other HANGs found in this
same triage batch (`QuartzEndpointWebIntegrationTests`,
`WebFluxAutoConfigurationTests`) are this quiet — both of those keep emitting
GC/JIT warnings most of the way to their own timeouts. This one goes
completely dark 70-90s in.

**Historical pattern**: this exact class has intermittently HANGed at 300s
across many independent runs going back to 07-17 (`craton-rerun-20260717`
shard4, `craton-rerun-20260723` shard7, `craton-fullsuite-20260731`,
`craton-rerun-20260731`, and now the three 08-06/08-07 runs above), always
interleaved with clean PASSes (08-02 azure, 07-28 rerun, 08-06
`pulsar-recheck-r1`) and — once — the ClassCastException FAIL this doc was
originally filed for. The HotSpot baseline has never hung on this class
(29.3s clean, 07-17). Isolated single-class reruns (`pulsar-recheck-r1-20260806`,
182s; `craton-hangverify-20260731`, 369s) always pass, which — per this
codebase's established "isolation reruns understate cluster bugs 4:1" lesson
(`fixed-suite-bugs/springboot/mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md`)
— means this needs to be chased under the suite's own concurrency, not in
isolation.

**Ruled out**: the already-fixed recycled-`JitInvokeInfo` dispatch-aliasing
bug (`383e7f5cf`, merged 08-05) that produced a whole cluster of
Mockito/ByteBuddy symptoms including ones in this exact `Argument$Binder`
neighborhood — the binary used for all three runs above (built from `dev` as
of 08-06) contains that fix, and the symptom shape doesn't match anyway (that
bug produced wrong *values* at specific crash sites; this is silence with no
crash at all).

**Not root-caused.** The `code_len=244 capacity=62336 wanted=68331` bail is,
by itself, a normal and handled path (`jit/src/x64/driver.rs:1797-1829`) — it
bails that one compile to the interpreter and records the shortfall for the
next compile attempt at this method to size its buffer from
(`crate::note_code_buffer_shortfall`, `jit/src/lib.rs:12329-12342`); nothing
in that retry path holds a lock or loops. Whether the total silence
afterward is this specific method's *interpreted* execution genuinely
hanging (as opposed to just being slow with nothing else to log), a deadlock
elsewhere in Mockito/ByteBuddy's mock-class generation that happens to be
reached right after this bail, or something in the retry bookkeeping itself,
was not determined this session — no `--stack-dump-on-timeout` capture exists
for any of the three runs (the suite disables the watchdog by default and
relies on its own per-class timeout instead, per
`run-spring-boot-suite.ps1`'s `New-ProcessRecord`). Whoever picks this up
next should rerun this one class alone with
`--stack-dump-on-timeout <some-value-under-300>` (or `--stack-sample-ms`) to
get a real frame at the stall point before guessing further.

## Original entry (2026-08-06), retained below

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
[`spring-boot-annotation-metadata-null-cluster-RESOLVED-20260806.md`](../../internal/fixed-suite-bugs/springboot/spring-boot-annotation-metadata-null-cluster-RESOLVED-20260806.md).
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
  [`...-RESOLVED-20260806.md`](../../internal/fixed-suite-bugs/springboot/spring-bean-attribute-type-null-flake-RESOLVED-20260806.md))
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
