# Spring Framework full-suite GC-variant run — dominant cause is an incomplete harness classpath, not a CratonVM defect

**Status:** OPEN (2026-08-10), but the headline finding here is a **harness
bug, not a VM bug** — flagging clearly so it doesn't get mistaken for
CratonVM correctness work. Full 2848-class Spring Framework suite, 3 GC
variants (`-XX:+UseG1GC`, `-XX:+UseZGC`, default/Generational), 1 shard
each, real JDK 25, JIT on. Binary: one `--features zgc` build serving all
three variants via runtime flag, built post-merge of `origin/dev` into
`main` (commit `681b5c1f1`, was `70bf05ed3` pre-merge). Results:
`apps/spring-suite-runner/out/gcvariant-{default,g1,zgc}-jit-real-all-20260810-*/results.tsv`
(full 2848-class sweep) and
`apps/spring-suite-runner/out/gcvariant-{default,g1,zgc}-jit-real-nonpassed-postmerge-*/{results.tsv,raw.log}`
(post-merge targeted rerun of just the non-passing classes — this doc's
per-class evidence is drawn from the `raw.log` there, which carries a
`FAILCAUSE`/`LOADERR` line per failure, not just from `logs/*.log`, since
this harness batches its raw output into one file per run rather than
one-file-per-class).

## Headline counts

| variant | OK | FAIL | LOADERR | TIMEOUT | EMPTY | total |
|---|---:|---:|---:|---:|---:|---:|
| default | 2491 | 335 | 13 | 5 | 4 | 2848 |
| G1 | 2499 | 334 | 13 | 0 | 4 | 2848(*) |
| ZGC | 2494 | 335 | 13 | 2 | 4 | 2848 |

(*G1 sums to 2846/2848 in the raw per-variant counts — 2 classes' status
wasn't captured in the sampled columns; not investigated further, likely a
harness edge case in this specific rerun rather than a new finding.)

Zero `cratonvm::gc::guard` hits of any kind (`HIB-CV-32`/corrupt-Value-cell
or otherwise) across all three variants' postmerge logs — unlike H2 (see
`../../internal/fixed-suite-bugs/h2-suite-bugs/gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md`), Spring
Framework's non-passing set is **not** dominated by the GC-corruption
family at all, in any of its manifestations.

## The actual dominant cause: `NoClassDefFoundError` — 789 of 1156 failure-cause lines (68%)

Counting every `FAILCAUSE`/`LOADERR` line in the default variant's
`raw.log` by exception type:

```
789 java.lang.NoClassDefFoundError
 61 org.springframework.beans.factory.UnsatisfiedDependencyException
 42 java.lang.IllegalStateException
 40 org.junit.platform.launcher.core.DiscoveryIssueException
 34 java.lang.AssertionError
 33 org.springframework.beans.factory.BeanCreationException
 32 javax.management.RuntimeErrorException
 26 javax.management.MBeanException
 25 org.mockito.exceptions.base.MockitoException
 18 org.springframework.beans.factory.parsing.BeanDefinitionParsingException
 15 org.springframework.beans.factory.NoSuchBeanDefinitionException
 10 org.springframework.beans.factory.BeanDefinitionStoreException
 10 org.opentest4j.AssertionFailedError
  ...
```

`NoClassDefFoundError` is not a CratonVM correctness signal here — it means
`common.args`'s test classpath is missing classes that the harness's own
build DID produce. Confirmed for the largest single sub-cause:

```
$ find apps/spring-framework/spring-core-test -path '*/build/classes*'
apps/spring-framework/spring-core-test/build/classes/java/test/...
```

`spring-core-test` (home of `TestGenerationContext`, `TestCompiler`, and
other AOT test-support classes) **is built** — its `.class` files exist on
disk — but isn't on the classpath `run-suite.sh`'s classpath-dump step
(`dump-testcp.init.gradle`) assembled. 185 of the 789 `NoClassDefFoundError`
lines are specifically `org/springframework/aot/test/...` or
`org/springframework/core/test/...` (the AOT/test-support family), spanning
48 distinct test classes.

The remaining ~600 `NoClassDefFoundError` lines are NOT the AOT-support
family — grouping every missing-class target by its top package segment:

```
177 org/springframework/web/
148 org/springframework/jms/
129 org/springframework/aot/
 70 org/springframework/core/
 66 org/springframework/orm/
 64 org/springframework/cache/
 43 org/springframework/oxm/
 42 org/springframework/ui/
 18 org/springframework/mail/
 12 org/springframework/scheduling/
  9 sun/reflect/misc/
  7 org/springframework/context/
  5 org/springframework/transaction/
```

This is a much broader gap than just `spring-core-test`: `spring-web`,
`spring-jms`, `spring-orm`, `spring-context-support` (cache/mail/scheduling
live there), and `spring-oxm` all read as **entirely or mostly missing**
from the shared test classpath, even though the build report for this
session confirmed all 24 java-plugin modules built successfully. This
reads as `run-suite.sh`'s `discover` step indexing test classes from every
built module (hence a 2848-class `all-classes.tsv`), while its classpath
generator only ever assembled entries for a narrower subset of modules —
the two steps have drifted apart. **Not yet root-caused to the specific
`dump-testcp.init.gradle`/`common.args`-merge step that's dropping these
modules** — that's the natural next step, and it should be cheap: diff
`common.args`'s module directory list against `spring-framework/settings.gradle`'s
full module list to see exactly which modules never made it in.

## Everything downstream of the classpath gap is unreliable as a CratonVM signal

Because `NoClassDefFoundError` this severe corrupts JUnit5's own test
discovery for affected classes (a missing type referenced by a test class's
signature or a `@Nested`/parameterized source can abort discovery for the
whole class, not just individual methods), the `UnsatisfiedDependencyException`,
`BeanCreationException`, `BeanDefinitionParsingException`,
`NoSuchBeanDefinitionException`, and `DiscoveryIssueException` counts above
are also suspect until the classpath is fixed — Spring's own bean-wiring
machinery throws exactly these exception types when a referenced class
can't load, so a fair number of them are likely secondary symptoms of the
same gap, not independent findings. **Do not treat any of this run's FAIL
counts as a CratonVM pass-rate baseline** until the classpath is repaired
and the suite is re-run.

The `javax.management.*` (58 combined) and `org.mockito.exceptions.base.MockitoException`
(25) clusters are more likely to be independent, worth a first look once
the classpath is fixed — MBean/JMX-related failures and Mockito failures
aren't an obvious downstream symptom of a missing Spring class the way the
bean-factory exceptions are.

## What's plausibly real, spot-checked

One example FAIL that looks like a genuine behavioral difference, not a
classpath symptom — `AutowiredAnnotationBeanPostProcessorTests.genericsBasedFieldInjectionWithSubstitutedVariables`:

```
java.lang.AssertionError:
Expected size: 1 but was: 4 in:
["X", 1,
    AutowiredAnnotationBeanPostProcessorTests$StringRepository@59838,
    AutowiredAnnotationBeanPostProcessorTests$IntegerRepository@59839]
```

A generics-based repository injection returning 4 candidates instead of the
expected 1 — could be a real classpath-scanning/generics-resolution
difference, or could still be an artifact of the same module-visibility gap
(extra/wrong candidates showing up because of what IS vs isn't on the
classpath). Not resolved either way this session — flagged as the kind of
FAIL worth re-examining once the classpath gap is closed and the noise
clears.

## Recommended next steps

1. **Fix the classpath gap first** — diff `common.args`'s module list
   against `spring-framework/settings.gradle`, add whatever's missing
   (`spring-core-test`, `spring-web`, `spring-jms`, `spring-orm`,
   `spring-context-support`, `spring-oxm`, and their test-scope
   dependencies), regenerate, and re-run the full 2848-class sweep before
   drawing any conclusions about CratonVM's Spring Framework compatibility.
2. Re-run the GC-variant comparison (default/G1/ZGC) after the classpath
   fix — the current ~87% OK rate and its GC-invariance (see
   `gc-corruption-guard-fixed-by-dev-merge-20260810.md`) were both measured
   against this incomplete classpath, so neither should be treated as
   final either.
3. Once the classpath is fixed and the suite is clean(er), re-triage the
   `javax.management.*`/Mockito clusters first, since they're least likely
   to just evaporate as classpath noise.

## Related

- `gc-corruption-guard-fixed-by-dev-merge-20260810.md` (this folder's
  sibling in `h2/`) — the before/after `dev`-merge story that prompted this
  run; also documents the h2 side of the same GC-variant sweep.
- `../../internal/fixed-suite-bugs/h2-suite-bugs/gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md` — H2's (RETIRED: its shared SIGSEGV was a NIO view-storage defect, not a GC one)
  results from the identical run, where (unlike here) the non-passing set
  IS dominated by real CratonVM-level defects (a shared SIGSEGV site, a
  GC-guard near-miss) rather than a harness gap.
- `aotintegration-hangs-after-the-unmodifiable-get-fix.md` — an earlier Spring
  AOT finding, **closed 2026-08-10** and retired to the internal archive. Its
  root cause was a compiled `invokestatic` binding its owner class by binary
  NAME, so under `@CompileWithForkedClassLoader` it called into the other
  loader's copy. Anything in the `org/springframework/aot/` cluster above that
  involves two loaders defining one name should be re-run against a binary
  carrying that fix before being investigated on its own.
