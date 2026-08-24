# TechEmpowerTest is not a perf class: with the JIT on it returns a WRONG ANSWER in ~25 s, and it PASSES with `--jit off`

**Status: OPEN CratonVM JIT correctness defect. The reactive LOAD PATH returns `null` for a row that exists (§9, measured directly); compiling `org/hibernate/reactive/` is necessary and a second component (`java/util/ArrayList`, `Arrays`, or `io/vertx/sqlclient/`) acts as a trigger (§8.2, §9.1). The failing id is RANDOM every time, so it is a RACE, not a bad row (§10).** Filed 2026-08-22 on `dev`
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

So **something on this path yielded `null` for an id the benchmark guarantees exists** (§9: it is `find()` itself).
`Randomizer`/`LocalRandom` draw from `[1, 10000]`
(`MAX_OF_RANGE = 10000`), and `createData` inserts exactly ids `1..10000`
(`world.setId( index + 1 )` for `index` in `0..9999`).

### 2.1 DISCRIMINATED 2026-08-22: the rows are all there, so it is on the READ side

> **Questioned by §8.3, then VINDICATED by §9 (2026-08-23).** §8.3 argued this
> section over-attributed the null to `find()` and that a compiled-`ArrayList`
> hole was likelier. A direct probe (§9) measured `find()` returning `null`
> itself, with counts matching exactly. **This section's reading is correct as
> written**; the §8.3 detour is left in place because the way it went wrong is
> instructive, not because it stands.

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
`null`. (Whether `find()` returned that null or the list lost it is settled in
§9: `find()` returned it.)

So:

* **Mechanism 1 — `createData` under-inserted — is REFUTED.** The write path
  and the `loop(0, 10000, …)` / `ArrayLoop` machinery are exonerated. The pull
  toward blaming them (because §8's defect lived there) was a bias worth
  naming, and it was wrong.
* **Mechanism 2 — the read path yields `null` for a row that exists — is
  CONFIRMED**, and §9 pins it precisely: `find()` itself returns the `null`,
  measured at the moment it is produced, before the collecting list is
  involved at all.

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
   §2.1** and pinned by §9 to `find()` itself. The sufficient partner is compiled
   `ArrayList`/`Arrays`, but §9 shows that is a TRIGGER, not the source of the
   wrong value.

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

## 9. 2026-08-23 — §8.3's `ArrayList` hypothesis is REFUTED. `find()` really does return `null`, and §2.1 was right all along

§8.5 step 1 asked for one probe: log what `find()` returned immediately before
`worlds::add`. It was run, and it goes against the hypothesis §8.3 had just
promoted.

`WorldVerticle` was instrumented with a `FIND_NULL` counter incremented inside
`.invoke(...)` at the moment `find()` yields a value — i.e. **before** the
element reaches the list — plus a scan of the collected list immediately before
`forEach` touches it, both reported on one line so they cannot drift apart:

| run | config | `listNulls` | **`findNullsSoFar`** |
|---|---|---:|---:|
| min-1 | ONLY `org/hibernate/reactive/,java/util/ArrayList` | 1 | **1** |
| min-2 | ONLY `org/hibernate/reactive/,java/util/ArrayList` | 1 | **1** |
| full-1 | full JIT | 1 | **4** |

```
[NULLPROBE] listNulls=1 listSize=20 listClass=java.util.ArrayList findNullsSoFar=1
```

**`find()` returned `null` before the list was ever involved.** In the minimal
runs the counts match exactly — one null produced, one null present — so the
`ArrayList` stored faithfully what it was handed. `listSize=20` is the expected
`queries=20`, with no short or over-long list.

* **§8.3's hypothesis — compiled `ArrayList` leaves a counted-but-unwritten
  hole — is REFUTED.** `ArrayList` and `Arrays` are exonerated as the *source*
  of the null.
* **§2.1's original reading is CONFIRMED** and can be relied on again: something
  in the reactive **load path** returns `null` for a row that exists. The
  hedge added to §2.1 and §2.2 by §8.3 can be read as resolved in §2.1's favour.

### 9.1 So what is `java/util/ArrayList` doing in the bisect table?

§8.2 remains factually correct — compiling `org/hibernate/reactive/` together
with `java/util/ArrayList` (or `Arrays`, or `io/vertx/sqlclient/`) is sufficient
to make the defect appear, and `HashMap`/`IdentityHashMap`/`LinkedHashMap` are
not. But §9 shows those partners are **not where the wrong value comes from**.

The consistent reading is that the partner acts as a **catalyst, not a culprit**:
compiling it perturbs inlining/tiering/timing enough for the *load path's* latent
defect to fire. That also explains the otherwise-awkward fact that two unrelated
partners (`java/util/ArrayList` and `io/vertx/sqlclient/`) are each sufficient —
they need not share a mechanism, only an effect on compilation of the code
around the load path.

**This is a caution about bisect results generally**, worth carrying beyond this
page: `CRATONVM_JIT_DENY`/`BISECT_ONLY` identify components whose compilation is
*necessary or sufficient for the symptom*, which is not the same as the component
that computes the wrong value. §8 read the table as if it were, and was wrong
within a day.

### 9.2 A grep artifact that nearly reported the defect as fixed

The instrumented runs scored `NPE=0` under the established
`grep -c 'because "w" is null'`, which reads exactly like the defect having
disappeared. It had not. Recompiling `WorldVerticle` changed the JDK helpful-NPE
message from `because "w" is null` to `because "<local…` — the lambda parameter
name is not preserved the same way through the ad-hoc `javac` invocation used
here. The NPE was still thrown, from the same
`WorldVerticle.lambda$updateWorlds$2`, with the same resulting 500.

Any future instrumentation of this class must re-check the grep pattern against
the new build rather than reusing the one in this page, or match on something
stable such as `getRandomNumber()` / `Unhandled exception in router`.

### 9.3 What the next session should do

1. **The target is the reactive load path returning `null` for a present row**,
   with the partner components treated as triggers rather than suspects. The
   `ONLY='org/hibernate/reactive/,java/util/ArrayList'` arm remains the right
   harness because it keeps almost nothing else compiled — but bisect *within*
   `org/hibernate/reactive/` now needs the deny lever, since §8.1 established
   that compiling that package alone does not reproduce.
2. Instrument one level deeper than §9: `find()` here is
   `Mutiny.Session.find` → the reactive `IdentifierLoadAccess`/load-plan path.
   Log the id passed in beside the `null` result, then query that id directly —
   if a specific id or range recurs, that is a far stronger lead than "some
   find returned null".
3. `ReactiveDeferredResultSetAccess` is still the most-named class in the
   teardown traces and has not been directly instrumented.
4. Do not re-chase `ArrayList`/`Arrays` (§9), `CompletableFuture`/Vert.x core
   (§8.2), or `createData`/the write path (§2.1). Four components have now been
   eliminated by direct measurement; the page's value is as much in that list as
   in the open question.

## 10. 2026-08-23 — the failing id is RANDOM every time, so this is a race, not a bad row

§9.3 step 2 asked for the id passed to the failing `find()`. The pristine code
passes `localRandom.getNextRandom()` inline, so the id is unrecoverable at the
point the null is seen; `randomWorldsForWrite` was restructured minimally to
bind it (`final Integer probeId`) and print on null:

```
[IDPROBE] find returned NULL for id=3022 nullNo=1
```

Nine runs of the minimal two-component repro
(`ONLY='org/hibernate/reactive/,java/util/ArrayList'`), five of which failed:

| run | result | ids that returned `null` |
|---|---|---|
| 1 | PASS | — |
| 2 | FAIL | 3022 |
| 3 | PASS | — |
| 4 | FAIL | **4176, 5472** |
| 5 | PASS | — |
| 6 | FAIL | 9836 |
| 7 | FAIL | 3140 |
| 8 | FAIL | **7669, 1521** |
| 9 | PASS | — |

**Seven ids, all distinct: 1521, 3022, 3140, 4176, 5472, 7669, 9836.**

No id repeats across runs. They are spread across the whole `[1, 10000]` space
with no clustering, no boundary values (never 1 or 10000), and no relation to
`createData`'s `setBatchSize(1000)` — none is at or adjacent to a multiple of
1000. Every one of them is provably present in the database: §2.1 established
`count=10000, minId=1, maxId=10000` with no gaps.

### 10.1 What this rules out

* **Not a specific bad row**, and not a row that failed to persist — the ids
  differ every time and all exist.
* **Not a boundary/off-by-one** in id handling — no extreme or near-extreme id
  ever appears.
* **Not a batching artifact** from the write side — no alignment with the 1000-row
  batch size.
* **Not deterministic on input at all**: the same workload, same binary, same
  configuration produces a different id each time, and often none.

What is left is a **race**: under 500 concurrent `/updates?queries=20` requests
across 10 verticles, an arbitrary in-flight `find()` occasionally resolves to
`null` for a row that exists. Two runs produced *two* nulls, so it is not a
once-per-process event either.

This is the first evidence that positively characterises the defect's *nature*
rather than its location, and it narrows the remaining hypotheses considerably:
a load-plan/session-state race, or a result being resolved against the wrong
in-flight request, both fit; a wrong constant, a bad row, and an id-arithmetic
error do not.

### 10.2 The probe suppresses the defect, and the numbers say by how much

With this instrument the failure rate drops to **5 of 9** runs, against **4 of 4**
on the uninstrumented binary (§1). The restructuring is the likely cause — binding
`probeId` and replacing the `worlds::add` method reference with a lambda changes
what that hot body compiles to.

The passing runs are also markedly slower (57–109 s) than the failing ones
(27–51 s), which is consistent with the whole class of observations on this page:
the configurations that avoid the wrong answer are the slower ones, and the race
needs the fast path to lose.

Anyone tightening this further should expect the instrument to fight them, and
should keep a same-binary uninstrumented control alongside — the pattern §8.2 of
the hibernate-reactive page established, and which held again here.

### 10.3 What the next session should do

1. **Treat it as a concurrency defect from here.** The remaining question is
   which shared state is being raced, not which row or which id.
2. The cheapest next discriminator is **concurrency scaling**: the fixture's
   `REQUEST_NUMBER = 500` and `VERTICLE_INSTANCES = 10` are plain constants in
   `TechEmpowerTest`/`WorldVerticle`. Dropping the concurrency toward 1 and
   seeing where the failure rate goes to zero would say whether the race is
   between requests, between verticles, or within a single session's own
   pipeline — and a configuration that still fails at low concurrency would be a
   far easier target to debug than 500 in-flight requests.
3. `ReactiveDeferredResultSetAccess` remains the most-named class in the traces
   and still has not been instrumented; a per-request identity on the result-set
   access path would show directly whether one request is being handed another's
   (empty) result.
4. Everything eliminated so far, in one place, so it is not re-tried: the write
   path (§2.1), `ArrayList`/`Arrays` as the source (§9), `CompletableFuture` and
   Vert.x core (§8.2), ORM core and Mutiny (§8.2), and now data-dependence of any
   kind (§10.1).
