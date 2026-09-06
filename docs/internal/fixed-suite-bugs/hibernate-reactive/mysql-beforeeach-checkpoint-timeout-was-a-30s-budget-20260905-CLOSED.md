# The hibernate-reactive MySQL `@BeforeEach` checkpoint-timeout family was a 30-second budget, not a defect — and it was hiding one real JIT miscompile, now FIXED

## Status

**CLOSED 2026-09-05.** Retires the public page
`hib-reactive-mysql-beforeeach-checkpoint-timeout-family-20260905` (deleted
from `docs/known-issues/hibernate/` by the same commit), which reported ~21 of 23-25 FAIL classes sharing one
`VertxTestContext` checkpoint-timeout signature on two of three GC arms and
said "root cause not yet isolated". It is isolated now, with the control arm
that page did not have.

Two separate things were tangled in it:

1. **The family itself is a budget, not a fault.** vertx-junit5 gives an
   intercepted `@BeforeEach` a fixed **30-second** join budget. On the MySQL
   arm that budget has to cover the Testcontainers MySQL container start AND
   the `SessionFactory` build, and CratonVM's share of the second is large
   enough that under shard concurrency the pair crosses 30 s. HotSpot, on the
   same host under the same concurrency, crosses it too — just far less often
   (2 of 24 classes against CratonVM's 17 of 24).
2. **One genuine correctness defect was inside the same FAIL set** and had
   nothing to do with the timeout: a JIT optimizing-tier phi-copy
   register-aliasing miscompile that made `java.time.Duration.toNanos()`
   return `seconds * 1e9 + seconds`. **FIXED** — see
   `../jit/jit-ir-phi-copy-register-alias-20260905-FIXED.md`.

## The four-arm control the original page did not have

Azure Linux (8 cores, shared, load 10-20 throughout), one binary built from
`origin/dev` at `a044e1fe1`, MySQL 26.7.0 via Testcontainers with
`testcontainers.reuse.enable=false` — one fresh container per class, exactly
the isolation the Windows run had — one class per JVM, `-Ddb=MySQL`. The class
list is the 24-class FAIL/HANG union of the three Windows GC arms, minus the
four the page itself excludes (`TechEmpowerTest`,
`DatabaseHibernateReactiveTest`, `MultithreadedInsertion*`).

| arm | classes | all-pass | classes with the checkpoint signature |
|---|---:|---:|---:|
| CratonVM, one class at a time | 24 | **23** | 0 |
| HotSpot, one class at a time | 24 | **24** | 0 |
| CratonVM, **6 concurrent** JVMs | 24 | **7** | 17 |
| HotSpot, **6 concurrent** JVMs | 24 | **22** | 2 |

The signature reproduces on Linux, on both VMs, and only under concurrency.
That is what settles it: a CratonVM correctness defect cannot make HotSpot
produce the identical `TimeoutException: The test execution timed out ... ->
checkpoint at io.vertx.junit5.VertxExtension.lambda$testContext$1` out of
`interceptBeforeEachMethod`, and 2 of 24 HotSpot classes did.

The single CratonVM isolation failure is
`types.BasicTypesAndCallbacksForAllDBsTest` (26 ok / 2 failed) — the JIT
miscompile in item 2, not the timeout family (`checkpointTO=0` on that row).

## The arithmetic

Per-class wall, same host, same load, three representative classes:

| class | CratonVM iso | HotSpot iso | CratonVM 6-way | HotSpot 6-way |
|---|---:|---:|---:|---:|
| `LockTimeoutTest` | 26 s | 16 s | 35 s | 36 s |
| `NoEntitiesTest` | 23 s | 14 s | 38 s | 28 s |
| `MutationDelegateTest` | 26 s | 18 s | 49 s | 35 s |
| (24-class median) | ~26 s | ~15 s | ~44 s | ~33 s |

The container start is logged and is inside the timed `@BeforeEach` — the
container is a per-JVM static that the first test touches, and with one class
per JVM that first touch is always the `@BeforeEach`:

```
CratonVM, isolation :  container PT10.5-11.0S, JUnit total 21.7-24.5 s
CratonVM, 6-way     :  container PT17.0-19.4S, JUnit total 33.1-47.0 s
```

So the budget is spent as `container start + SessionFactory build <= 30 s`.
At 6-way the container alone takes 17-19 s, leaving 11-13 s; CratonVM's build
does not fit in that and HotSpot's (whose whole isolated class, container
included, is 14-17 s) usually does. Everything the original page found follows
from that one inequality:

* **GC-independent** — the budget does not care which collector is running.
  92% overlap between the ZGC and Generational FAIL sets is what a shared
  threshold produces, not a coincidence needing explanation.
* **Feature-area-independent** — soft-delete, embedded ids, mutation
  delegates and `NoEntitiesTest` (which has no entities at all) fail
  identically because none of them has reached its own code yet.
* **Only the sharded suite** — one class at a time, 23 of 24 pass.
* **Only MySQL** — the same suite on the same host with PostgreSQL is
  201 PASS / 3 FAIL (`fullbatch-craton-20260903`), because a Postgres
  container starts in a fraction of the MySQL one's time.
* **`NoLiveTransactionValidationErrorTest` is not "ZGC-only"** — the earlier
  page that guessed that (`../hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md`)
  was reading a threshold, and a threshold has no collector.

## Four corrections to the original page, from its own logs

**1. The checkpoint timeout is the FIRST failure of each class, not the
family.** Counting all three shards of each Windows arm:

| arm | `The test execution timed out` | `Failed to read any response from the server` |
|---|---:|---:|
| default (ZGC) | 38 | 240 |
| generational | 40 | 281 |
| g1 | 34 | 232 |

The dominant exception is
`io.vertx.sqlclient.ClosedConnectionException: Failed to read any response
from the server`, raised out of `SqlClientPool` while building
`JdbcEnvironment` — i.e. the SECOND and later attempts, after the first
`@BeforeEach` has already blown its budget and left the pool half-built. The
page reports the first exception of each class and calls it the family; the
retries are the bulk of the log. (This second-order shape did not reproduce on
Linux at all: `closedconn=0` on every row of both 6-way arms. It is a Docker
Desktop / Windows artifact of the pool being abandoned mid-handshake.)

**2. Container-start latency is not "ruled out", it is half the budget.** The
page rules it out by comparing 30-32 s starts against the harness's 240 s
per-class cap — the wrong cap. Against the 30 s cap that actually fires, a
30-42 s container start has already spent the whole budget before Hibernate
runs a line. The page's own data says the same thing once it is cross-tabbed:
container start does not separate PASS from FAIL (PASS n=148, 16.3/23.8/43.4 s
min/median/max; FAIL n=10, 18.1/22.6/26.3 s) because it is only ONE of the two
terms — but 13 further FAIL classes never logged a completed container start
at all.

**3. `@Timeout(value = 10, timeUnit = MINUTES)` on `BaseReactiveTest.before`
is not the budget.** `VertxExtension.joinActiveTestContexts` reads the
annotation from `extensionContext.getTestMethod()` — the TEST method — never
from the `@BeforeEach` it is intercepting. Test classes here carry no
`@Timeout`, so the 30 s default applies to the setup join. That is upstream
vertx-junit5 behaviour and is correct; it is simply much smaller than the page
assumed.

**4. The ZGC arm's 165 `zgc real: field index OOB index=0 num_slots=0
op="get"` lines are a real but SEPARATE signature.** They appear in the ZGC
arm only; the Generational arm has zero VM-level warnings of any kind and
fails the same 22 classes. A ZGC-only event cannot be the cause of a
GC-independent family. (That signature is the documented stale-pointer-into-a-
compacted-away-object shape; `CRATONVM_DBG_ZGC_CORPSE` names the corpse.)

**5. The G1 arm's `MultithreadedIdentityGenerationTest` HANG is the OTHER
family.** The page flags it as "consistent with the same defect manifesting
more severely under G1" and asks for a re-check. It is not: on Linux the class
passes in isolation (113 s CratonVM, 55 s HotSpot) and under 6-way load fails
with `TimeoutException: testIdentityGenerator ... timed out after 120 seconds`
out of `TimeoutExceptionFactory` — the harness's own
`junit.jupiter.execution.timeout.default`, a different exception shape
entirely, and exactly where the page already puts its sibling
`MultithreadedInsertionTest`:
`docs/known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`.

## What was actually fixed

`types.BasicTypesAndCallbacksForAllDBsTest` failed 2 of 28 in **isolation**,
with no timeout and no connection loss:

```
testLocalDateTimeType : java.lang.ArithmeticException: / by zero
        at java.time.LocalTime.truncatedTo(LocalTime.java:991)
        at java.time.LocalDateTime.truncatedTo(LocalDateTime.java:1120)
testInstant           : the same, wrapped in a CompletionException
```

`LocalTime.truncatedTo` divides by `unit.getDuration().toNanos()`, and after
JIT warm-up `ChronoUnit.MILLIS.getDuration().toNanos()` answered **0** —
198,356 wrong answers in 200,000 calls, first wrong at call 1,644. Root cause
is a phi-copy register-aliasing miscompile in the optimizing tier, fixed on
this branch with an end-to-end regression test; the full write-up is
`../jit/jit-ir-phi-copy-register-alias-20260905-FIXED.md`.

## The residual

**CORRECTED 2026-09-06.** This section first said the bootstrap is "roughly
1.7x HotSpot's on the full per-class wall (and several times HotSpot's once the
shared container start is subtracted)". That number came from comparing the
per-class medians of two arms run hours apart on a shared host, and it does not
survive a decomposition. Re-measured on dev tip `6430e495f`, same host, same
JDK, three reps each, median:

| stage | CratonVM | HotSpot | ratio |
|---|---:|---:|---:|
| VM boot floor (trivial `main`) | 400 ms | 206 ms | 1.9x, but 194 ms absolute |
| `MetadataAccessTest` (no live DB) | 11.7 s | 9.7 s | **1.21x** |
| `NoEntitiesTest` (+ MySQL container) | 30.5 s | 26.2 s | **1.16x** |

So the real gap on this workload is **1.16-1.21x**, not 1.7x, and the VM boot
floor is a rounding error against a 30-second budget. The 1.7x was an artifact
of comparing arms taken at different host loads; the ratio on these workloads
is dominated by Docker and moves with load, which is exactly why the page's
own four-arm control had to run its two VMs under the SAME concurrency.

**And the CPU is not where the page assumed.** The execution profiler added on
2026-09-06 (`CRATONVM_PROFILE_SAMPLE_MS=10`) puts 837 samples against an 11.7 s
wall for the no-DB stage -- so ~8.4 s of CPU and ~3.3 s of waiting -- and the
ranking is dominated not by Hibernate but by the harness around it:
Testcontainers' shaded Jackson annotation introspection
(`AnnotatedClass.resolveMemberMethods/construct`, `TypeBindings.create`,
`POJOPropertyBuilder.mergeAnnotations`), docker-java's HTTP parsing
(`BasicLineParser.parseStatusLine`, `CharArrayBuffer.substringTrimmed`),
`ServiceLoader` lookup, netty's `InternalThreadLocalMap`/`Recycler`, and
log4j's `PluginRegistry`. `ArrayList.<init>` is the single hottest method at
5.73%. Hibernate appears once in the top 25, at 1.55%
(`AggregatedClassLoader.getResources`).

Read that as a RANKING, not as percentages: the profiler samples at safepoint
polls, so a thread blocked in a native call or parked contributes nothing and
interpreted frames are over-represented against compiled ones.

The practical consequence is that "make the hibernate-reactive bootstrap
faster" is mostly NOT a Hibernate or `SessionFactory` problem on this
harness.

A 1.16-1.21x VM does not on its own put a class over 30 seconds; the container
start does, and the VM gap decides which side of the line the sum lands on.
Whatever is left of it belongs with the existing cost families rather than in a
per-class bug page:

* `docs/known-issues/hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`
* `../hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md` (the
  `CompletableFuture`/lambda-composition cost section)
* `docs/known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`

**Do not re-open this page from a sharded MySQL run.** A 3-arm x 3-shard
MySQL run is 9 concurrent JVMs each starting its own MySQL container; the
30-second budget will be crossed and the same 20-odd classes will report the
same signature, on any VM. Run the affected class alone before believing it,
and put a HotSpot arm under the SAME concurrency next to it — that pairing is
what took this page from "root cause not yet isolated" to closed in one
afternoon.

## Reproduction

Nothing here needs a harness. `common-mysql.args` is
`apps/hibernate-reactive-suite-runner/common.args` with `-Ddb=PostgreSQL`
rewritten to `-Ddb=MySQL`; nothing else differs, and the runner class is the
one already sitting beside it.

```bash
# one class, one JVM, its own MySQL container -- 23-24 of 24 pass, both VMs
cd apps/hibernate-reactive-suite-runner
<cratonvm> --java-home <jdk25> @common-mysql.args -Dcraton.batch=1 CratonRunner org.hibernate.reactive.NoEntitiesTest
<jdk25>/bin/java @common-mysql.args -Dcraton.batch=1 CratonRunner org.hibernate.reactive.NoEntitiesTest

# the same command six ways at once is the whole defect: 7/24 against 22/24.
```

The Windows evidence the original page was written from is at
`apps/hibernate-reactive-suite-runner/runs/mysql-{default,generational,g1}-20260905-3gc-mysql-local/`
(untracked).
