# TechEmpowerTest is not a perf class: with the JIT on it returns a WRONG ANSWER in ~25 s, and it PASSES with `--jit off`

**Status: OPEN CratonVM JIT correctness defect. Bisected to a TWO-COMPONENT interaction: `org/hibernate/reactive/` + array-backed list storage (`java/util/ArrayList` / `Arrays`) -- see §8.** Filed 2026-08-22 on `dev`
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

So **something on this path yielded `null` for an id the benchmark guarantees exists** (§8.3 narrows which).
`Randomizer`/`LocalRandom` draw from `[1, 10000]`
(`MAX_OF_RANGE = 10000`), and `createData` inserts exactly ids `1..10000`
(`world.setId( index + 1 )` for `index` in `0..9999`).

### 2.1 DISCRIMINATED 2026-08-22: the rows are all there, so it is on the READ side

> **Partly revised by §8.3 (2026-08-23).** What this section establishes and
> keeps: every row exists, so the write path is exonerated. What it states too
> strongly: that `find()` itself returned `null`. The NPE proves only that a
> null reached `forEach`; the bisect in §8 makes a compiled-`ArrayList` hole
> the leading explanation instead. Read §8.3 before building on the wording
> below.

The measurement §2.1 called for was run, and it settles the question in one
line. A `[COUNT-PROBE]` was added to `TechEmpowerTest` — **the test class, not
`WorldVerticle`**, deliberately, because `createData` holds the suspect
`ArrayLoop` and §8.2 of the hibernate-reactive page showed that instrumenting
a suspect body can hide the defect. It runs after `/createData` has returned
and before `/updates` begins:

```
[COUNT-PROBE] after createData: count=10000 minId=1 maxId=10000 (expected 10000/1/10000)
```

**That line appeared, identically, in all four runs — the three `--jit on`
runs that FAILED and the `--jit off` run that passed.** Critically, run 1
paired `count=10000` with the `NullPointerException` **in the same run**: every
one of the 10 000 rows was present in the database, ids `1..10000` with no
gaps at either end, and the collection handed to `forEach` still contained a
`null` (whether `find()` returned it or the list lost it is settled in §8.3,
not here).

So:

* **Mechanism 1 — `createData` under-inserted — is REFUTED.** The write path
  and the `loop(0, 10000, …)` / `ArrayLoop` machinery are exonerated. The pull
  toward blaming them (because §8's defect lived there) was a bias worth
  naming, and it was wrong.
* **Mechanism 2 — something on the read side yields `null` for a row that
  exists — is CONFIRMED** as the remaining search space. (Narrowed further by
  §8: the null is more likely a hole left in the `ArrayList` that collects the
  results than a wrong return from `find()` itself. Both live on the read side,
  so this section's conclusion stands; only its attribution to `find()` was
  premature.)

A caveat recorded rather than hidden: adding the probe shifted the failure
mode distribution. Before it, 3 of 4 `--jit on` runs took the fast NPE mode;
with it, 1 of 3 did (runs 2 and 3 hit the 300 s fixture deadline instead).
The probe therefore perturbs timing somewhat — but it did **not** mask the
defect (4/4 still fail with the JIT on), and the decisive datum comes from a
run that reproduced the NPE with the probe active, so the conclusion does not
rest on the perturbed runs.

### 2.2 The original two candidates, retained for the record

These were the two candidates before §2.1's measurement. Kept because the
reasoning that picked the wrong favourite is worth preserving:

1. **`createData` under-inserted** — it runs
   `CompletionStages.loop( 0, 10000, index -> session.persist(...) )`, the
   `ArrayLoop` machinery, with `setBatchSize(1000).flush()`. A dropped
   iteration or a lost batch would leave exactly the gaps `find()` later
   misses. **REFUTED by §2.1: count=10000, ids 1..10000, no gaps.**
2. **Something on the read side yields `null` for a row that IS present** —
   produces the identical symptom with no row ever missing. **CONFIRMED by
   §2.1**, then narrowed by §8: the sufficient partner is compiled
   `ArrayList`/`Arrays`, so a hole in the collecting list now outranks a wrong
   return from `find()`.

The instructive part is that candidate 1 was the more attractive one, because
`createData`'s `loop(0, 10000, …)` is the same `ArrayLoop` that
[hib-reactive-3gc-run-regressions-20260820.md](../hibernate/hib-reactive-3gc-run-regressions-20260820.md)
§8 found miscompiled — a known-bad component sitting directly in the suspect
path. It was still the wrong answer. Proximity to a previously-broken
component is a reason to test something first, never a reason to believe it;
one `SELECT count(*)` was enough to settle what argument would have kept
circling.

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

The discriminating query has been run (§2.1): the rows are all present, so
this is the **reactive read path**, and the loop/write machinery is out of
scope. What remains:

1. **Bisect by class with `CRATONVM_JIT_DENY`**, the way the hibernate-reactive
   page's §8.1 did — that technique took its defect from "somewhere in the
   JIT" to a single class in one table of runs. Start with the read-path
   classes this workload actually goes through: `ReactiveDeferredResultSetAccess`
   (already named in the teardown traces), the reactive `find`/load
   plan classes, and `Mutiny`/`Uni` glue. `CompletionStages$ArrayLoop` should
   NOT be the starting point any more — §2.1 exonerated it.
2. **Sweep the JIT feature switches**, cheapest first: §8.3 of the
   hibernate-reactive page lists twelve that were tried there, of which
   `CRATONVM_JIT_LAMBDA_SITE=0` cleared that defect in a single run. Whether
   any of them clears THIS one is unknown and one run each answers it.
3. **Get the failing id.** The probe proves rows `1..10000` all exist; the next
   refinement is to log which id `find()` was called with when it returned
   `null`, then query that row directly. If a specific id or a narrow range
   recurs, that is a much stronger lead than "some find returned null" — and it
   distinguishes a genuinely wrong query result from a lost/misrouted
   continuation delivering someone else's (empty) result.
4. Note the read path here is **concurrent**: 500 in-flight requests across 10
   verticles, each doing 20 sequential `find`s on a shared `Mutiny.SessionFactory`.
   A result being delivered to the wrong pending continuation would present
   exactly as `null` for one caller, and would explain why the defect needs
   the JIT and load to show. That is a hypothesis, not a finding.
5. Do NOT re-file this as a perf/timeout issue without re-reading §1 and §3.

## 8. 2026-08-23 — bisected. It is an INTERACTION, and the evidence now points at compiled `ArrayList`, not at `find()`

25 runs with `CRATONVM_JIT_DENY` (substring on `Class.method`) and its inverse
`CRATONVM_JIT_BISECT_ONLY` (prefix allowlist — everything else force-interpreted).
Same binary throughout (`cratonvm-bisect`, current `dev`), quiet host (load
3–5), fixture pristine (the §2.1 count probe was removed first, so nothing is
perturbing timing).

**Scoring on NPE COUNT, not PASS/FAIL.** Denying a large package makes the run
slow enough to hit the fixture's 300 s deadline, which produces a FAIL that
says nothing about the defect. `grep -c 'because "w" is null'` is immune to
that: 0 means the wrong answer did not occur, ≥1 means it did. Every verdict
below is that count. Where an arm both timed out AND scored 0 it is marked
ambiguous and was re-tested with the allowlist instead of being believed.

### 8.1 Necessary, but NOT sufficient

| lever | scope | NPE |
|---|---|---|
| DENY `org/hibernate/reactive/` | compile everything EXCEPT it | **0** (PASS, 56 s) |
| ONLY `org/hibernate/reactive/` | compile ONLY it | **0** (PASS, 61 s) |

Denying hibernate-reactive fixes it, so its compilation is **necessary**.
Compiling only hibernate-reactive does *not* reproduce it, so it is **not
sufficient**. **This defect requires two components compiled together** — which
already distinguishes it from the §8 defect of the hibernate-reactive page,
where a single class (`CompletionStages$ArrayLoop`) was the whole answer.

### 8.2 Which partner — the allowlist table

All arms below also include `org/hibernate/reactive/`; everything not listed is
force-interpreted.

| partner allowed | NPE | verdict |
|---|---|---|
| `org/hibernate/` (ORM core) | 0 | not a partner |
| `io/smallrye/` (Mutiny) | 0 | not a partner |
| `java/util/concurrent/` (`CompletableFuture`) | 0 | **exonerated** |
| `java/lang/` | 0 | exonerated |
| `io/vertx/core/` (Future/context/event loop) | 0 | **exonerated** |
| `java/` | 1 | reproduces |
| `java/util/` | 1 | reproduces |
| `io/vertx/` | 1 | reproduces |
| `io/vertx/sqlclient/` | 1 | reproduces |
| `java/util/HashMap` | 0 | not it |
| `java/util/IdentityHashMap`,`LinkedHashMap` | 0 | not it |
| **`java/util/ArrayList`** | **2** | **reproduces** |
| **`java/util/Arrays`** | **1** | **reproduces** |

**The async machinery is exonerated by measurement.** `CompletableFuture` and
Vert.x core each compile cleanly alongside hibernate-reactive with no wrong
answer. §7's "a result delivered to the wrong pending continuation" hypothesis
is not supported — it should not be the next thing anyone chases.

What reproduces is **array-backed list storage**: `java/util/ArrayList` alone,
or `java/util/Arrays` alone (which is what `ArrayList` grows through,
`Arrays.copyOf`). `HashMap`, `IdentityHashMap` and `LinkedHashMap` do not.

### 8.3 This reframes the defect, and revises §2.1

§2.1 concluded "`find()` returned `null` for a row that exists", because the
rows were all provably present. That inference had a gap this bisect exposes:
the NPE proves **a null was in the list**, not that `find()` produced it. The
list is built by exactly the implicated machinery:

```java
final List<World> worlds = new ArrayList<>( count );   // array-backed
for ( int i = 0; i < count; i++ ) {
    loopRoot = loopRoot.call( () -> session
            .find( World.class, localRandom.getNextRandom() )
            .invoke( worlds::add ) );                  // 20 async appends
}
…
worldsCollection.forEach( w -> w.getRandomNumber() );   // w is null
```

So the leading hypothesis is now: **compiled `ArrayList` (or the `Arrays.copyOf`
grow path it uses) leaves a null hole** — an element slot counted in `size` but
never written, which `forEach` then hands out. That fits every row of §8.2 and
needs no `find()` defect at all. `new ArrayList<>(count)` is pre-sized to 20
here, so the plain `add` path rather than a resize is the first thing to read.

**It is a hypothesis, not a finding**, and §2.1's wording is now too strong:
what is established is that a null reaches `forEach`, and that compiling
`ArrayList`/`Arrays` alongside hibernate-reactive is sufficient to make that
happen. Which of the two puts the null there is still open, and one probe
settles it — log the value `find()` returned inside `.invoke()`, immediately
before `worlds::add`. If it is non-null there and null at `forEach`, the list
lost it.

`io/vertx/sqlclient/` also reproducing is not yet explained by this story and is
the one loose end; it may reach the same array-backed storage through its own
row containers, or be a second route to the same bug.

### 8.4 Reproducers, cheapest first

```
# ~28 s, fails with the wrong answer, only two components compiled
CRATONVM_JIT_BISECT_ONLY='org/hibernate/reactive/,java/util/ArrayList' … --jit on

# ~60 s, passes — same binary, one component removed
CRATONVM_JIT_BISECT_ONLY='org/hibernate/reactive/' … --jit on
```

That pair is a two-component, same-binary A/B in about 90 seconds total, which
is a far better instrument than the whole-suite run this page started from.

### 8.5 What the next session should do

1. **Settle §8.3's question with the one probe named there** (log what `find()`
   returned, right before `worlds::add`). It decides between "list loses an
   element" and "find returns null", and everything else depends on it.
2. If the list is at fault, read the compiled `ArrayList.add`/`grow` and
   `Arrays.copyOf` bodies — `CRATONVM_DBG=jit-disasm` on the
   `ONLY=…,java/util/ArrayList` arm gives a small, targeted dump because almost
   nothing else is compiled in that configuration.
3. Explain or reproduce the `io/vertx/sqlclient/` route (§8.3's loose end)
   before assuming one mechanism covers both.
4. Do not re-chase `CompletableFuture` / Vert.x-core continuation delivery —
   §8.2 exonerated both by direct measurement.
