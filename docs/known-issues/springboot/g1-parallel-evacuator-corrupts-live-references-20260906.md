# G1's parallel evacuator still corrupts live references — the half a crash census cannot see

| | |
|---|---|
| **Status** | OPEN. **5 of 6 runs** on dev `48fff33f7`, always the same test method. Not a crash: the process exits 1 with one JUnit failure, and the exception is a different reflection-metadata failure each run. |
| **Scope** | `--XX:UseGc G1`. ZGC, the generational collector and HotSpot are clean on the same class. |
| **Cure** | `CRATONVM_G1_PARALLEL_EVAC=0` — 3/3 on this binary, 3/3 on the two before it. |
| **Reproducer** | 4 minutes, one Spring Boot class, no crash, no sweep. |
| **Why this page exists** | the retired `g1-parallel-evacuator-had-none-of-the-serial-arms-header-screens` write-up closed the G1 evacuation family on a CRASH A/B — *"3 CRASH / 6 with the screens off, 0 / 6 with them on"*. The screens are in this binary and on by default, and `CRATONVM_G1_PARALLEL_EVAC_SCREEN=0` changes nothing in either direction. The screens stopped the crashes. They did not stop the corruption. |

## Reproducer

```bash
# azureuser@20.80.105.49, release binary, jdk-25.0.4+7 — run it ALONE
cd /data/cratonvm/apps/spring-boot/module/spring-boot-integration
CP="../../sb-runner:$(tr '\n' ':' < build/cratonvm-test-cp.txt)"
$CVM --java-home $JDK --Xmx 2g --add-opens=java.base/java.net=ALL-UNNAMED \
     --stack-dump-on-timeout 0 --XX:UseGc G1 \
     -cp "$CP" SbRunner \
     org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests
```

`SBRUNNER_RESULT tests=34 failed=1`, on
`integrationGlobalPropertiesUserBeanOverridesAutoConfiguration` in every failing
run on the last two binaries. The class binds ports and starts servers: a
concurrent copy of it, or a build sharing the box, turns any arm into a HANG that
says nothing. Every number below is one process at a time.

## Arms — one class, one variable, across three dev tips

Two GC fixes landed while this was being measured, and both moved an arm without
closing the defect. The columns are kept separate rather than pooled, because the
movement is the interesting part.

| arm | `8d83c7585` | `0cd363b64` (+ pin/coverage, skip-span) | `48fff33f7` (+ agent `Class[]` relocation) |
|---|---|---|---|
| G1, default | **FAIL 9/9** | **FAIL 4/4** | **FAIL 5/6** |
| `CRATONVM_G1_PARALLEL_EVAC=0` | PASS 3/3 | PASS 2/2 | **PASS 3/3** |
| `CRATONVM_G1_PARALLEL_EVAC_IN_JIT=0` | PASS 4/4 | PASS 2/2 | — |
| `--nojit`, parallel evacuator ON | **FAIL 2/2** | PASS 3/3 | — |
| `--nojit` + `CRATONVM_G1_WORKERS=1` | **FAIL 2/2** | PASS 1/1 | — |
| `CRATONVM_G1_WORKERS=1` (JIT on) | 1 FAIL, 1 PASS | — | — |
| `CRATONVM_G1_PARALLEL_EVAC_SCREEN=0` | FAIL 1/1 | FAIL 1/1 | — |
| `CRATONVM_G1_NARROW_FIXUP=0` | FAIL 1/1 | — | — |
| ZGC / generational | PASS | PASS | — |
| HotSpot `jdk-25.0.4+7` | PASS (runner baseline) | | |

What each row settles:

* **The parallel evacuator is necessary and sufficient to remove it.**
  `CRATONVM_G1_PARALLEL_EVAC=0` is 8/8 clean across all three binaries; nothing
  else tried is.
* **It was not a JIT interaction, and then it was.** On `8d83c7585` the `--nojit`
  arm failed 2/2 *including with one worker* — no JIT, no worker race, still
  corrupt. The pin/coverage and skip-span fixes closed that half; on
  `0cd363b64` `--nojit` passes 3/3. Do not read the surviving half as "always
  needed compiled frames".
* **It is not a worker race.** The single-worker arm failed on the binary where
  the JIT-free half was live.
* **It is not the header screens** — and the `[g1] evacuation ref-scan CLAMPED a
  holder's element walk` count is the same 9–10 in the PASSING serial arm as in
  the failing parallel one, so the clamps are background on this workload.
* **It is not the narrow Phase-4 fix-up.**
* **The rate is falling.** 9/9 → 4/4 → 5/6. Two fixes have each taken a bite;
  a third arm of the same kind may take another. That is a reason to keep a
  reproducer, not a reason to call it closed.

## What the corruption looks like

Not a crash, and not the same exception twice. Every one is a reflection-metadata
read that came back null, or naming the wrong class, or missing a method the
class declares:

```text
IllegalStateException: No bean class name set
  at FullyQualifiedAnnotationBeanNameGenerator.buildDefaultBeanName

BeanCreationException: ... Could not resolve matching constructor on bean class [null]

BeanDefinitionStoreException: Failed to read candidate component class
  Caused by: AnnotationConfigurationException: Attribute 'prefix' in annotation
    [ConfigurationProperties] is declared as an @AliasFor nonexistent attribute
    'value' in annotation [java.lang.annotation.Annotation]

BeanCreationException: Error creating bean with name 'errorChannel'
  Caused by: SpelEvaluationException: EL1004E: Method call: Method
    getIntegrationProperties(DefaultListableBeanFactory) cannot be found on type
    IntegrationContextUtils
```

The third names the mechanism. Spring's `AnnotationTypeMapping.resolveAliasTarget`
turns on one identity comparison, `aliasFor.annotation() == Annotation.class`. A
JUnit `TestExecutionListener`, registered through `META-INF/services` on a
directory placed first on the classpath and evaluating that comparison after
every test method, caught it flipping mid-run:

```text
[ALIAS] START   same=true  target=..Annotation@7e64 targetLoader=null   canon=..Annotation@7e64
[ALIAS] after integrationGlobalProperties...  same=false
                target=..Annotation@7e64 targetLoader=AppClassLoader   canon=..Annotation@66efb6
```

A bootstrap mirror whose `getClassLoader()` becomes the app loader, and an `ldc`
of the same class resolving to a different object, is what a slot that was never
remapped looks like from Java when the bytes it still points at happen to decode.

**That listener is the cheap instrument here and is worth reusing.** It costs one
services file and one class, it runs inside the real test JVM, and it turns "some
test in this class fails" into "the identity flipped between these two test
methods" — for any invariant expressible as a Java expression.

## What this is not

* **Not a Spring Boot problem.** This class is a witness, not a subject. It is
  one of the four on the retired `moving-young-fallback-four-springboot-classes`
  write-up, for the same reason: it allocates hard while application contexts
  start, so it takes young pauses under compiled code.
* **Not the same defect as its sibling page.** The other residual from that work,
  `moving-young-jit-frame-fallback-costs-3-10x-20260906.md`, is the generational
  collector DECLINING to move and paying 3–10x for it. This one is G1 moving and
  not finishing the job. Opposite failures, same precondition.

## Next

With the JIT-free half closed, the surviving half needs compiled frames on the
stack AND the parallel evacuator. The set of things the parallel driver does that
the serial one does not is small: TLAB-carved to-space placement and
`retire_tlab`'s cursor write-back, forward retirement inside `parallel_evacuate`
rather than between Phases 4 and 5, the fused seed/closure ordering, and where
`resurrect_dead_finalizers` runs. The two flat reference walkers are NOT in that
set — `for_each_flat_object_reference_trusting_header` is `..._capped` with
`usize::MAX`, so both drivers walk identically.

Not attempted here: a bisect over those four, and a `CRATONVM_G1_VERIFY_HOLDERS=1`
run against this reproducer.
