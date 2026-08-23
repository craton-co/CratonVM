# `MultithreadedInsertionWithLazyConnectionTest` — NOT FIXED. It is a ~6x throughput gap on `CompletableFuture` composition crossing the test's own 10-minute budget, plus a separate flaky duplicate-insert

## Status
**OPEN (2026-08-22). Diagnosed, decomposed, not fixed.** One component was
removed and measured (see §6), and it is **not** enough: the change is neutral
on this workload. Nothing here is a single defect — closing this test needs the
reactive composition path to get several times faster.

A second, *separate* finding is recorded in §5: an intermittent duplicated
INSERT that is not an ID-uniqueness failure. That one is a correctness signal
and is the more interesting half.

## Severity
**MEDIUM.** One class, FAILs on all three collectors, PASSes on HotSpot. It is
the last CratonVM-only failure on the ZGC arm of the whole 249-class suite
(`RESULTS-20260822-3gc-mysql-local.md`).

## What actually fails

The class has two methods. `testIdentityGenerator` PASSes. Only
`testIdentityGeneratorWithTransaction` fails, and the binding constraint is the
test's **own** budget, not the harness's:

* `io.vertx.junit5.Timeout(value = 10, timeUnit = MINUTES)` on the class — this
  is Vert.x's annotation, not JUnit's, and it caps `VertxTestContext`
  completion.
* the harness's `-Djunit.jupiter.execution.timeout.default=120s` fires first in
  a normal suite run, which is why the suite reports
  `TimeoutException: ... timed out after 120 seconds`. Raising it to 900 s does
  **not** make the class pass: the method then dies on the Vert.x 10-minute wall
  with `The test execution timed out`.

| runtime | `testIdentityGeneratorWithTransaction` alone |
|---|---:|
| HotSpot 25.0.3 | **103.3 s** PASS |
| CratonVM (dev `bec3dca17`) | **618 s**, FAIL at the 10-minute wall |

All 12 verticles reach the start latch; none reaches the end latch. It is slow,
not deadlocked.

## Why only the transactional method

The two methods differ in one line:

```java
// testIdentityGenerator — PASSES
s.persist(entity).thenCompose(v -> s.flush()).thenAccept(v -> s.clear())
// testIdentityGeneratorWithTransaction — FAILS
s.withTransaction(t -> s.persist(entity))
```

The passing one **clears the persistence context every iteration**. The failing
one never clears it, so the context grows to `ENTITIES_STORED_PER_THREAD` and
every commit flushes over everything accumulated so far — quadratic by design,
on both runtimes.

The native census confirms the model rather than assuming it:
`java/lang/reflect/Field.get` is called **756 466** times at
`ENTITIES_STORED_PER_THREAD=250`. Predicted: 250 flushes x mean context 125 x 2
fields x 12 threads = 750 000. HotSpot pays the same 756 k reflective reads.

## Scaling — CratonVM is superlinear where HotSpot is flat

`ENTITIES_STORED_PER_THREAD` made settable via `-Dmti.n` (patch-dir copy of the
test; the field is otherwise a compile-time constant):

| N | HotSpot | CratonVM |
|---:|---:|---|
| 250 | 67.4 s | 75-132 s (varies with box load) |
| 500 | 79.0 s | 272 s (6 GB heap) / 358-381 s (1.5 GB) — and one 24 s FAIL |
| 1000 | — | >600 s FAIL |
| 2000 | 103.3 s | >600 s FAIL |

Subtracting the ~58 s fixed cost (Testcontainers MySQL boot + schema +
SessionFactory), the marginal work is ~7 s / 21 s / 45 s on HotSpot against
~17 s / ~300 s / >540 s on CratonVM. A 4x heap buys ~25%, so GC is a
contributor and not the story.

## Where the time goes

`--stack-sample-ms 100`, 3972 samples at N=250, grouped by the deepest frame:

| cost centre | samples | share |
|---|---:|---:|
| `java.util.concurrent.CompletableFuture` | 1479 | **37%** |
| `org.hibernate.reactive.engine.impl.Cascade` | 578 | 15% |
| `org.hibernate.event.*` (flush / dirty check) | 477 | 12% |
| other `org.hibernate` | 432 | 11% |
| `AsyncTrampoline` | 285 | 7% |
| netty / vertx | 383 | 10% |

and `--dump-native-registry` counts **20 218 428** native invocations in a 141 s
run — top rows `Objects.requireNonNull` 5 964 327, `Object.<init>` 4 339 048,
`CompletableFuture.thenCompose` 2 035 783, `CompletableFuture.whenComplete`
1 930 706.

Per-operation A/B on already-completed stages (the reactive shape — a resolved
Vert.x future runs its continuation inline), `HibfixCfProbe.java`:

| op | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `thenCompose` | 37 ns | 10 619-29 175 ns | **290-790x** |
| `whenComplete` | 35 ns | 9 167-27 580 ns | **260-790x** |
| `thenRun` | 8 ns | 4 639 ns | **580x** |
| `thenAccept` | 11 ns | 4 870 ns | **440x** |
| *(calibration: compiled arithmetic loop)* | 287 ns | 711 ns | *2.5x* |

**That 2.5x floor is the point.** The composition primitives are two to three
orders of magnitude off, against an engine that is otherwise 2.5x.

## 5. The other finding: an intermittent DUPLICATED INSERT, not a duplicate ID

Roughly one run in three at N=250-500 fails early instead of timing out, with
either or both of:

```
ConstraintViolationException: Duplicate entry '103' for key 'Entity.PRIMARY'
IllegalStateException: HR000090: Live transaction detected while closing the connection
```

The obvious reading — the sequence optimizer handing two threads the same id —
is **refuted**. The sibling `MultithreadedIdentityGenerationTest` isolates
exactly that (48 threads x 10 000 ids, no inserts) and PASSES on CratonVM 2/2,
at 153-170 s against HotSpot's 125 s, i.e. ~1.3x and correct.

So the ids are unique and the INSERT happened twice — which is the other thing
this class exists to catch, in its own javadoc:

> N.B. We actually had a case in which the IDs were uniquely generated but the
> downstream event was being processed twice (or more) concurrently.

That is the same family as the fixed
[`hib-reactive-3gc-run-regressions-20260820.md`](hib-reactive-3gc-run-regressions-20260820.md)
§8 defect (a deopting lambda body re-run from entry), but it is **not** that
defect: the `site_unresumable` counter that measures its residual reads **0** on
this workload, with `site_resumed=4-5`. Unexplained, intermittent, and the most
promising thread here.

## 6. What was changed, and why it is not the fix

The `CompletableFuture` / `CompletionStage` dependent-stage natives
(`thenApply`, `thenAccept`, `thenRun`, `thenCompose`, `exceptionally`, `handle`,
`whenComplete`) are, over a real JDK, pure delegations back to the method they
shadow — `native_cf_then_compose` detects a real receiver and
`invoke_special`s `uniComposeStage`, which is precisely what the real
`thenCompose` does. They are no longer registered when a real JDK class library
is in use (`NativeMethodRegistry::real_jdk`), which also lifts the
`calls-native-shadowed-method` JIT seal from their callers.

Verified, same binary, `CRATONVM_CF_DELEGATING_YIELD=0` restores the old
behaviour:

| measurement | natives registered | not registered |
|---|---:|---:|
| `--dump-native-registry` `thenCompose` invocations | 440 003 | **not registered** |
| probe `thenCompose` | 12 952 ns | 10 619 ns |
| probe `whenComplete` | 12 021 ns | 9 167 ns |
| probe `compose.chain.x3` | 29 889 ns | 22 397 ns |
| JIT `calls-native-shadowed-method` seals | 1503 | 1473 |
| **this test at N=250** | **74.7 s** | **75.2 s** |

**Neutral on the workload it was built for.** It is kept because it is a
structural redundancy (and the `CompletionStage` interface rows delegated to
`CompletableFuture` for receivers that need not be one), not because it helps
here. 26/28 of the suite's non-PASS union is unchanged by it.

The prediction that motivated it was wrong, and the way it was wrong is worth
recording: `copy()` and `minimalCompletionStage()` build the same `uni*Stage`
machinery with no native and cost 708 / 1149 ns, which suggested the shadow was
~85% of a `thenApply`. It is not — those two take **no lambda**. The residual
cost is the `invokedynamic` capture and the functional-interface dispatch inside
the composition, not the CF bytecode and not the native funnel.

## 7. What to try next

1. **The duplicated INSERT (§5).** A correctness bug beats a throughput one, and
   it is separable: it reproduces at N=250 in ~2 minutes, roughly 1 run in 3.
   Instrumenting the *consumer* rather than the loop is the lesson from the §8
   defect — instrumenting the method under investigation suppressed that one.
2. **Lambda/SAM dispatch inside a composition chain**, per §6's closing
   paragraph — that is where the remaining `thenCompose` microseconds are, and
   it is not the native funnel.
3. Do **not** re-run the experiment in §6. It is done, it is measured, and the
   numbers are above.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner && ./hibfix-mtins-run.sh cv HOTSPOT hibfix-common-mysql-longto.args -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest testIdentityGeneratorWithTransaction
```

`hibfix-common-mysql-longto.args` is `common.args` with `-Ddb=PostgreSQL` ->
`-Ddb=MySQL` and the JUnit default timeout raised to 900 s; the argfile itself
is generated and machine-local, so it is not committed.

## Related files

- `apps/hibernate-reactive/hibernate-reactive-core/src/test/java/org/hibernate/reactive/MultithreadedInsertionWithLazyConnectionTest.java`
- `apps/hibernate-reactive-suite-runner/HibfixCfProbe.java`, `HibfixCfBound.java`, `hibfix-mtins-run.sh`
- [`hib-reactive-3gc-run-regressions-20260820.md`](hib-reactive-3gc-run-regressions-20260820.md) §8
- [`batchtest-mysql-jdbc-batching-slow-20260822.md`](batchtest-mysql-jdbc-batching-slow-20260822.md) — the same "trivial JDK primitive served by a native" shape, and the same conclusion that the funnel's aggregate is small
