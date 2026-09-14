# `ParallelExecutionSpringExtensionTests` reads `TIMEOUT` because it is a 10 000-test throughput benchmark — and turning the parallelism OFF does not narrow the gap

| | |
|---|---|
| **Status** | OPEN. **Correctness matches HotSpot exactly** (10/10 repetitions, 10 000/10 000 nested tests succeeded). Throughput only. |
| **Scope** | `org.springframework.test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests` (spring-framework, `spring-test`). |
| **Measured** | 2026-09-10/11, Azure `20.80.105.49`, real JDK 25, binary `cratonvm-springfix2-20260910` (`claude/spring-residuals-20260910`). The host carried load average 40–90 from other sessions throughout; every number below is either a completed-work count or a ratio taken from **concurrently executed** arms. |

## It is not a hang

The suite's `TIMEOUT` row carries `found=0`, which is indistinguishable from a
class that never started. Running the class under a per-test progress listener
answers that directly — each of the ten `@RepeatedTest` repetitions starts and
finishes, and none of them stalls:

```text
[   1.2s]   START  repetition 1 of 10
[  92.0s]   FINISH repetition 1 of 10 -> SUCCESSFUL
[ 156.7s]   FINISH repetition 2 of 10 -> SUCCESSFUL
[ 221.2s]   FINISH repetition 3 of 10 -> SUCCESSFUL
[ 297.8s]   FINISH repetition 4 of 10 -> SUCCESSFUL
[ 366.9s]   FINISH repetition 5 of 10 -> SUCCESSFUL
[ 441.3s]   FINISH repetition 6 of 10 -> SUCCESSFUL
[ 533.6s]   FINISH repetition 7 of 10 -> SUCCESSFUL
[ 607.8s]   FINISH repetition 8 of 10 -> SUCCESSFUL
[ 730.5s]   FINISH repetition 9 of 10 -> SUCCESSFUL
[ 811.5s]   FINISH repetition 10 of 10 -> SUCCESSFUL
[ 811.6s] END
```

Ten repetitions, 65–120 s each, monotone progress, all `SUCCESSFUL`.
HotSpot on the same host in the same minutes: **26.3 s** for the same ten.

## What the class actually does

```java
private static final int NUM_TESTS = 1000;

@RepeatedTest(value = 10, failureThreshold = 1)
void runTestsInParallel() {
    EngineTestKit.engine("junit-jupiter")
        .configurationParameter(PARALLEL_EXECUTION_ENABLED, "true")
        .configurationParameter(PARALLEL_CONFIG_DYNAMIC_FACTOR, "10")
        .selectors(selectClass(TestCase.class))
        .execute()
        .testEvents().assertStatistics(s -> s.started(1000).succeeded(1000).failed(0));
}
```

Each repetition launches a **nested** JUnit Platform run of 1 000
`@RepeatedTest` methods, each with a `@BeforeEach`, a test and an `@AfterEach`,
every one of them resolving an `@Autowired ApplicationContext` parameter
through Spring's `SpringExtension`. Ten repetitions is **10 000 nested tests
and 30 000 parameter resolutions**. It is a throughput benchmark wearing a
correctness test's clothes, and the runner's 180 s per-class cap is what turns
it red.

## Parallelism is not the gap

The obvious reading — "a VM that serialises Java threads would look exactly
like this" — is wrong, and it is cheap to falsify. `ParProbe` runs the class's
own nested `TestCase` through `EngineTestKit` with the parallel switch under
our control. Both VMs were run **concurrently** for each configuration, so the
host's load applies equally to the two arms of every ratio (sequential A/B on
this host has been measured to invent 1.9× differences for a flag that costs
nothing):

| configuration | HotSpot (rep 2) | CratonVM (rep 2) | ratio |
|---|---:|---:|---:|
| `parallel=on`, dynamic factor 10 | 9 043 ms | 69 860 ms | **7.7×** |
| `parallel=off` | 6 120 ms | 75 544 ms | **12.3×** |

Turning parallelism off leaves CratonVM where it was (69.9 s → 75.5 s, i.e.
nothing) while HotSpot gets *faster* (9.0 s → 6.1 s), so the ratio **widens**.
Whatever the cost is, it is per test, not per thread. (That HotSpot is faster
serial than parallel here is itself a load artefact — at load ~50 on 8 cores
there are no spare cores to win with. It does not affect the conclusion, which
rests on CratonVM being unchanged by the switch.)

## It is not the 2026-09-10 reflection fix

The class read `OK` in the pre-fix full-suite sweep and `TIMEOUT` in the
post-fix one, which looks like a regression from
`the-type-variable-scope-walk-never-climbed…`. It is not. That fix adds
`getTypeParameters()` / `getDeclaringClass()` calls to the type-variable path,
so the question deserved a measurement rather than an argument. Two binaries
built from the same base — `39a90d2f4` and `39a90d2f4` + the fix — run
**concurrently**, three repetitions each:

| | rep 1 | rep 2 | rep 3 | median |
|---|---:|---:|---:|---:|
| pre-fix | 128 652 ms | 120 745 ms | 121 163 ms | 121.2 s |
| post-fix | 112 519 ms | 111 723 ms | 133 302 ms | **112.5 s** |

The fixed binary is not slower; if anything it is marginally faster, and the
difference is inside the noise of a host at load 40. The `OK` → `TIMEOUT`
transition is the sweep's, not the binary's: the pre-fix sweep had the host to
itself (sum-class-ms 14.5 M across 2 848 classes) and the post-fix sweep shared
it with five other sessions (61.0 M for the same 2 848 classes, 4.2×). Rerun
alone, 19 of the post-fix sweep's 22 non-`OK` classes come back `OK`.

## What has not been done

Where the 8–12× goes. The workload is dominated by per-test Spring
`TestContext` work — `SpringExtension.supportsParameter` /
`resolveParameter`, context-cache lookup, `@BeforeEach`/`@AfterEach`
dispatch — all reflection-heavy, none of it AOT or javac. That makes it a
different animal from the two other slow classes in this suite
(`BeanRegistrationsAotContributionTests`, `AotIntegrationTests`), both of which
are dominated by generate-and-compile.

Profiling it needs a quiet host: at load 40–90 a profile attributes time to
whoever lost the scheduler lottery. `ParProbe off 5` on an idle box, under the
VM's own sampler, is the next step, and `NUM_TESTS` is already a constant in
the probe so the workload size can be dialled down first — a `TIMEOUT` carries
no number, and the first move on any of these is to parameterise the work.

## Repro

```bash
cd apps/spring-framework/spring-test
CP="/data/springres-work:$(cat /data/springres-work/cp-spring-test.txt)"

# progress trace: is it slow, or is it stopped?
<vm> -cp "$CP" KProgress \
  org.springframework.test.context.junit.jupiter.parallel.ParallelExecutionSpringExtensionTests

# the parallel/serial A/B (run the two VMs concurrently, not one after the other)
<vm> -cp "$CP" org.springframework.test.context.junit.jupiter.parallel.ParProbe on  2
<vm> -cp "$CP" org.springframework.test.context.junit.jupiter.parallel.ParProbe off 2
```

`KProgress.java` and `ParProbe.java` are in
`docs/internal/fixed-suite-bugs/repros/spring-typevar-scope-20260910/`.
`ParProbe` must be compiled into package
`org.springframework.test.context.junit.jupiter.parallel` — the nested
`TestCase` it selects is package-private.
