# TechEmpowerTest is not a perf class: with the JIT on it returns a WRONG ANSWER in ~25 s, and it PASSES with `--jit off`

**Status: OPEN CratonVM JIT correctness defect.** Filed 2026-08-22 on `dev`
(`b8fa0585e` present — this is NOT that defect, see §4). Reproduces 4/4 with
the JIT on and 0/3 with it off, on a quiet Azure host, against a HotSpot
control that passes.

**This reclassifies the class.**
[residual-seven-after-the-afc-fix-20260817.md](../hibernate/residual-seven-after-the-afc-fix-20260817.md)
§2.1 files `techempower.TechEmpowerTest` as volume-driven and bound by its
fixture's own hardcoded deadline — "323.7 s, still cut off (>25x)", one of
five classes said to be "correct when given enough time", with the explicit
note that **no VM flag or runner override can reach** that deadline, so it
stays reported as FAIL. `hib-reactive-3gc-run-regressions-20260820.md` §7.3,
§8.7 and §9.3 all carried that filing forward.

It is wrong, at least as of today's `dev`. The class **passes on CratonVM in
about 72 s** with the JIT off — comfortably inside the fixture's 5-minute
budget — and the only reason it fails is that the JIT makes it produce a
wrong result. It never needed a bigger budget.

## 1. The measurement

Azure host (`20.80.105.49`), live Postgres via Testcontainers, 1-minute load
average 6.8–14 across all runs (recorded because a timeout mode is involved;
see §3). Binary `cratonvm-teverify`, built from the worktree at current `dev`,
`git merge-base --is-ancestor b8fa0585e HEAD` confirmed. Same
`common.args`/classpath for every arm, with
`-Djunit.jupiter.execution.timeout.default=600s` supplied through
`HR_CLASS_OVERRIDES` so that JUnit's own 120 s per-test timer is not the
binding limit and the fixture's 5-minute Vert.x deadline is (see §5 — getting
this wrong is easy and this page's first attempt did).

| arm | result | wall |
|---|---|---|
| **real HotSpot** (JDK 25, same host/classpath/args) | **PASS 1/1** | 13.3 s |
| **CratonVM `--jit off`** | **PASS 3/3** | 74.9 / 72.2 / 72.6 s |
| **CratonVM `--jit on`** | **FAIL 4/4** | 31.3 / 22.5 / 21.7 / 305.8 s |

`--jit off` is not merely "different", it is *correct and fast enough*: 72 s
against a 300 s fixture deadline, with no VM flag or fixture patch involved.

## 2. What actually fails

`TechEmpowerTest` deploys 10 `WorldVerticle` instances, calls `/createData`
once, then fires **500 concurrent** `/updates?queries=20` requests; the
fixture fails on the first response whose status is not 200/204
(`AssertionFailedError: Expected status code 200 or 204, but was 500`).

The 500 is a server-side unhandled exception, and the FIRST one in the log is
the cause — everything after it (`VertxException: Connection is not active
now, current status: CLOSING`, `ClosedConnectionException`) is teardown
cascade and should not be mistaken for the defect:

```
SEVERE io.vertx.ext.web.RoutingContext  Unhandled exception in router
java.lang.NullPointerException: Cannot invoke
  "org.hibernate.reactive.it.techempower.World.getRandomNumber()" because "w" is null
```

That is `WorldVerticle.updateWorlds`:

```java
worldsCollection.forEach( w -> {
    final int previousRead = w.getRandomNumber();   // <- w is null
```

`worldsCollection` is built by `randomWorldsForWrite`, which appends whatever
`session.find( World.class, id )` yields:

```java
loopRoot = loopRoot.call( () -> session
        .find( World.class, localRandom.getNextRandom() )
        .invoke( worlds::add ) );
```

So **`find()` handed back `null` for an id the benchmark guarantees exists.**
`Randomizer`/`LocalRandom` draw from `[1, 10000]`
(`MAX_OF_RANGE = 10000`), and `createData` inserts exactly ids `1..10000`
(`world.setId( index + 1 )` for `index` in `0..9999`).

### 2.1 Two candidate mechanisms, NOT yet discriminated

This page deliberately stops before naming one, because the evidence so far
is equally consistent with both:

1. **`createData` under-inserted.** It runs
   `CompletionStages.loop( 0, 10000, index -> session.persist(...) )` — the
   `ArrayLoop` machinery — with `setBatchSize(1000).flush()`. If the compiled
   loop drops an iteration, or a batch flush silently loses rows, the missing
   ids are exactly what `find()` would later miss.
2. **`find()` is wrong for a row that IS present.** The reactive load path
   returning `null` for an existing row would produce the identical symptom
   without any row ever being missing.

**The discriminating measurement is one query**: after `/createData` returns
200 and before `/updates` runs, `SELECT count(*) FROM World` (or select the
missing id directly once the failing id is logged). 10 000 means the defect is
in the read path; fewer means it is in the write/loop path. That is the
honest first step for whoever takes this, and it is cheap.

Note the pull toward mechanism 1: `createData`'s `loop(0, 10000, …)` is the
same `ArrayLoop` that
[hib-reactive-3gc-run-regressions-20260820.md](../hibernate/hib-reactive-3gc-run-regressions-20260820.md)
§8 found miscompiled. **That is a reason to check it first, not a reason to
believe it** — §8's defect is fixed and present in this binary, and §4 below
shows this failure predates and survives that fix.

## 3. The failure mode is bimodal, which is why this was misfiled for so long

Across the four `--jit on` runs:

| mode | runs | wall |
|---|---|---|
| NPE -> 500 (wrong answer) | **3 of 4** | 21.7–31.3 s |
| fixture's own 300 s Vert.x deadline | 1 of 4 | 305.8 s |

The dominant mode is a fast wrong answer. But the class can also simply run
out the fixture deadline — and **that is the only mode the earlier filings
ever recorded** (residual-seven's "323.7 s, still cut off"). Anyone who
sampled this class once, caught the timeout variant, and filed it under the
volume/lambda-dispatch-cost family would have produced exactly the record
that exists today. The lesson is the one this repo keeps relearning: a single
observation of a bimodal failure names the wrong family.

## 4. It is NOT the §8 lambda-deopt defect, and not fixed by it

* The 500 was already observed on a binary built **before** `b8fa0585e`
  (`hib-reactive-3gc-run-regressions-20260820.md` §7.3, 17.7 s, same
  "Expected status code 200 or 204, but was 500").
* It reproduces 4/4 on a binary that **contains** `b8fa0585e` (verified by
  `merge-base --is-ancestor`).

So it is a distinct, still-open JIT correctness defect. §8 repaired seven
classes; this is not one of them.

## 5. A harness trap this page fell into first — `--timeout` is the wrong knob

The first `--jit on` attempt reported `FAIL 0/2`-shaped results in 251 s with
JUnit's own `TimeoutException: … timed out after 120 seconds`. The runner's
`--timeout` bounds only the forked process's wall clock; the binding limit was
`-Djunit.jupiter.execution.timeout.default=120s` from `common.args`. To let
the fixture's own deadline bind, that property must be raised **after** the
argfile — which is what `class-overrides.tsv` / `HR_CLASS_OVERRIDES` flags do
(residual-seven §4 documents the ordering fix that made per-class `-D`
overrides effective at all). Every number in §1 was taken with it raised to
600 s, via a temporary `HR_CLASS_OVERRIDES` table so the tracked
`class-overrides.tsv` was not modified.

## 6. Reproducer

```
# fails 4/4 (wrong answer in ~25 s, or the 300 s fixture deadline)
HR_CLASS_OVERRIDES=<tbl> CV_BIN=<binary> \
  ./run-hibernate-reactive-suite.sh --list <one-line list> \
  --shards 1 --timeout 600 --jit on

# passes 3/3 in ~72 s
… --jit off
```

where `<tbl>` is a one-row TSV:

```
org.hibernate.reactive.techempower.TechEmpowerTest<TAB>600<TAB>-Djunit.jupiter.execution.timeout.default=600s
```

This is a ~25 s reproducer with a clean same-binary A/B and a passing HotSpot
control — considerably better instrumentation than the class's previous
filing implied was available.

## 7. What the next session should do

1. **Run the discriminating query in §2.1 first.** It splits the search space
   in half for the cost of one `SELECT count(*)`, and every other step depends
   on which half.
2. If it is the write path, the `ArrayLoop`/`loop(0, 10000, …)` compiled body
   is the obvious suspect and `CRATONVM_JIT_DENY` can bisect it by class the
   way §8.1 did (`CompletionStages$ArrayLoop` was the single class that
   mattered there).
3. If it is the read path, this is unrelated to the loop machinery and needs
   its own bisect; `CRATONVM_JIT_LAMBDA_SITE=0` and the other twelve switches
   §8.3 enumerated are the cheapest first sweep, since one of them cleared the
   §8 defect in a single run.
4. Do NOT re-file this as a perf/timeout issue without re-reading §1 and §3.
