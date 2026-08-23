# `MultithreadedInsertionWithLazyConnectionTest` — NOT FIXED. A ~6x throughput gap on `CompletableFuture` composition crossing the test's own 10-minute budget, plus a separate defect where a stage completes before its transaction does

## Status
**OPEN (2026-08-23). Diagnosed, decomposed, not fixed.** One component was
removed and measured (see §6), and it is **not** enough: the change is neutral
on this workload. Nothing here is a single defect — closing this test needs the
reactive composition path to get several times faster.

A second, *separate* finding is recorded in §5, and it is the more interesting
half: verticles whose loop stops short, duplicate INSERTs, and HR000090 "live
transaction detected while closing". §5.1 places the common cause at a stage
completing before the work it represents finished — HR000090 precedes the
first duplicate in 8 failing runs out of 8, and one verticle hit it after
running all 60 iterations. It is **not fixed**. §5.3 is the thing to read
before touching it. A 40-run batch put the failure rate at
**20%**, which makes arms affordable — but the earlier bisect was run at arm
sizes that were noise at any of these rates. §5.2 records a table-size
mechanism I claimed and then refuted — it is kept as a worked example of the
same mistake.

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

## 5. The other finding: the loop is CUT SHORT. The duplicate INSERT and HR000090 are both downstream

Some runs fail early instead of timing out, with either or both of:

```
ConstraintViolationException: Duplicate entry '103' for key 'Entity.PRIMARY'
IllegalStateException: HR000090: Live transaction detected while closing the connection
```

The obvious reading — the sequence optimizer handing two threads the same id —
is **refuted**. The sibling `MultithreadedIdentityGenerationTest` isolates
exactly that (48 threads x 10 000 ids, no inserts) and PASSES on CratonVM 2/2,
at 153-170 s against HotSpot's 125 s, i.e. ~1.3x and correct. The duplicated
ids are also not random: they cluster on 52 / 102 / 152, i.e. **50 apart**, the
pooled optimizer's allocation-block boundary.

### 5.1 The primary event, and what 40 strict runs settled

`InsertEntitiesVerticle` counts its own `storeEntity` calls
(`sequentialOperation`). Logging that counter in the verticle's `whenComplete`
handler — the CONSUMER, not the loop — settles what the row count cannot: a
short loop and a lost insert produce the same number of rows.

Verticles do stop early. On a failing run at 24 threads x N=60:

```
run 9: rows=56/1440 dup=9 hr90=25
       iters=[39 27 23 9 35 25 27 21 33 43 31 53 23]
```

thirteen of twenty-four verticles ran a fraction of their sixty iterations.

**But the cut loop is not the primary event.** In the same batch, the FIRST
HR000090 of run 3 comes from a verticle that had just logged `ITERS 60` — it
ran every iteration and still hit "live transaction detected while closing".
Across all eight failing runs of the 40-run batch, HR000090 precedes the first
`Duplicate entry` every time, by 53 to 217 log lines:

| run | first HR000090 | first Duplicate |
|---|---:|---:|
| 3 | 175 | 242 |
| 9 | 173 | 261 |
| 12 | 173 | 226 |
| 19 | 173 | 243 |
| 22 | 165 | 327 |
| 26 | 171 | 270 |
| 27 | 167 | 228 |
| 35 | 169 | 382 |

So the common factor is a **stage completing before the work it represents
finished**. `withTransaction` hands back a completed stage while the
transaction is still live; the loop advances or ends on that, the session is
closed under it, and the duplicate INSERTs and the four
`NonUniqueObjectException`s follow from the resulting inconsistent session
state. Short loops are a symptom of the same thing, not its cause.

#### The parity, which is the sharpest clue in this page

Across the eight failing runs, the verticles that stopped short did so after an
**odd** number of `storeEntity` calls, 88 times out of 90:

```
full (60): 870    short & odd: 88    short & even: 2   (the two are 18 and 24)
```

`sequentialOperation` is a per-verticle field incremented exactly once per
`storeEntity`, so this says a dying verticle has almost always made an odd
number of calls. Under any process that interrupts the loop at a uniformly
random iteration, 88/90 one-sided is not a coincidence.

It is also directly checkable without any new instrument: `storeEntity` sets
`entity.name = beforeOperationThread + "__" + localVerticleOperationSequence`,
so the `name` column IS each verticle's sequence number. Querying `Entity`
after a failing run for a repeated or skipped sequence per thread answers
whether an index is processed twice — which is the exact thing this test's
javadoc says it exists to catch — or simply skipped.

### 5.2 There is NO table-size variable — that claim was mine, and it is refuted

An earlier revision of this section reported that the failure needed ~140 000
accumulated rows in `Entity`, on the evidence of 0 failures in 19 runs against
a truncated table and failures returning once filler rows were inserted.

**That is wrong, and the mechanism makes it impossible.** `BaseReactiveTest`
sets `HBM2DDL_AUTO=create`, so Hibernate DROPS AND RECREATES the schema every
time the SessionFactory is built — once per run. The table is empty at the
start of every run no matter what was in it before.

The arithmetic in the run log says the same thing. After inserting 140 000
filler rows and running six times, a second insert of 360 000 left the table at
**361 440**, not 501 440: the first batch of filler was already gone, wiped by
the first run's schema creation. The failure in that arm happened on run 4 —
on an empty table. `Entity_SEQ` tells the same story: it read 2851 before a
later batch and 2801 after, i.e. it went DOWN, because it too is recreated.

So the two arms were never different. What produced a 19-run clean streak was
the base rate of §5.3 and nothing else: at the ~6-12% measured here,
`0.9^19 ≈ 0.14`. **This is exactly the mistake §5.3 exists to warn about, made
one section earlier**, and it is left in the page rather than quietly deleted
because the shape of it is the lesson: a clean streak invited a mechanism, and
the mechanism was invented to fit the streak.

Two practical consequences:

* the repro needs **no** special table state — run it against whatever is
  there;
* `hibfix-dupins-loop.sh`'s `delete from Entity where id > 0` is redundant. It
  is harmless and is kept only so the row count is read against a known start.

Before theorising about a state variable, check what the harness under test
already resets on its own.

### 5.3 The base rate is too low for the bisect that was built on it

Under the restored conditions the failure rate is **10-15%** (4 failures in 36
control runs), against the ~37% measured the previous day on the same binary
and workload. The rate itself is not stable across host conditions.

At 10-15%, a clean arm of 10 or 12 runs has p ≈ 0.2-0.35 of occurring by
chance. **Every "10/10 clean" arm in the earlier bisect is therefore within
noise** — including `CRATONVM_JIT_LAMBDA_ADAPTER=0`, `CRATONVM_JIT_LAMBDA_SITE=0`,
`CRATONVM_JIT_DENY=CompletionStages$ArrayLoop` and
`CRATONVM_JIT_DENY=CompletionStages.alwaysTrue`. They were significant against a
37% baseline and are not against this one, and an arm measured today cannot be
compared with a baseline measured yesterday. Redoing that bisect needs **≥40
runs per arm** at the observed rate, or a sharper instrument.

Two further arms measured at this rate, both reported for completeness rather
than as findings: `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER=0` 6/6 clean, and its
own control 6/6 clean — the split is uninformative because the control did not
fail either.

### 5.4 What was implemented, measured, and found INERT

`lambda_adapter_entry` cached its emitted thunks under
`(proxy_class_id, impl_entry)` while `emit_adapter` is a function of
`(leading, captures, sam_args, impl_entry)`. `sam_args` is the SAM's own arity —
a property of the call site's functional interface, not of the impl — so the key
omitted an input the body depends on, and `CompletionStages` has three
`alwaysTrue` overloads across three different functional interfaces.

The key now names every input, and `lambda_adapter_shape_collisions()` counts
exactly the reuses the old key would have made and the new one refuses. It is
reported in the `CRATONVM_DBG=lambda-jit` census as `site_shape_collisions`.

**On this workload it reads 0**, with the adapter path fully engaged
(`site_adapters=10 site_cap_adapters=4`). The structural gap was real and is
closed; it is **not** this defect, and the emitted code on this workload is
unchanged by the fix. Recorded so nobody re-derives the hypothesis.

### 5.5 Do not instrument `CompletionStages`

A patched `CompletionStages` that prints `ArrayLoop`'s terminal state
(`applied` / `current` / `end`) ran 12/12 clean. Per §5.3 that number is not
significant on its own, but it is also useless as an instrument: the class is
the one under suspicion, and shadowing it changes what the JIT compiles. The
verticle-side `ITERS` counter in §5.1 is the instrument that works, because it
lives in the consumer.

### 5.6 The VM-side constant-return probe

`CRATONVM_JIT_LAMBDA_CONST_PROBE` replaces the statistics of §5.3 for one
specific shape. `CompletionStages.alwaysTrue` is `return true;` — two bytes of
bytecode — so its correct answer is knowable WITHOUT running it. That makes a
wrong answer provable from a single call, with no baseline, no repeat runs and
no rate to beat.

The probe screens every SAM call whose impl body is a bare push-and-`ireturn`
(`iconst_<n>` / `bipush` / `sipush`) and reports any call that observes
something else. `=1` screens; `=strict` additionally refuses the emitted
inline-cache thunk for such impls — see below.

Counters, in the `CRATONVM_DBG=lambda-jit` census:

| counter | meaning |
|---|---|
| `site_const_screened` | calls actually checked — the ENGAGEMENT counter |
| `site_const_mismatch` | calls that returned the wrong value |
| `site_const_dirty_high` | right int, in a register with a junk upper half |
| `site_const_opaque` | sites whose calls the probe structurally cannot see |

**`site_const_opaque` is the honest part.** An emitted thunk tail-jumps to the
impl and returns straight to its compiled caller, so no Rust runs on that path
and those calls cannot be screened at all. A `mismatch=0` beside a non-zero
`opaque` has not cleared anything; it has failed to look. `=strict` drives
`opaque` to 0 by refusing those thunks — at the cost of changing the very
codegen under suspicion, which is a diagnostic trade and not a measurement one.

For the thunk that IS installed there is a check that needs no failure at all:
`const_thunk_self_test` calls the freshly emitted thunk once, at install, and
compares the result against the constant. A mis-emitted slide, a wrong entry or
a stale cached thunk is then caught on the first run of the process rather than
in one run out of ten. It is inside the probe flag today; promoting it to
always-on is the obvious next step once it has run across a suite.

Measured on this workload (24 threads, N=60):

| mode | `screened` | `mismatch` | `opaque` |
|---|---:|---:|---:|
| `=1` | ~500 / run | 0 | 2 |
| `=strict` | 4319 | 0 | **0** |

and the constant-return site it finds is exactly the one the investigation
pointed at: `CompletionStages.alwaysTrue(I)Z`.

The probe caught nothing on its first 10 runs at `=1` — none of them failed,
so there was nothing to screen. The 40-run strict batch below is the real
answer.

#### The verdict: 40 runs in strict mode, and `alwaysTrue` is EXONERATED

| | |
|---|---:|
| runs | 40 |
| failing runs | **8** (20%) |
| constant-return calls screened | **156 569** |
| `site_const_mismatch` | **0** |
| `site_const_dirty_high` | **0** |
| `site_const_opaque` | **0** |
| `WRONG ANSWER` lines | **0** |

Every one of the eight failing runs was individually clean — 1799 to 2821
calls screened apiece, zero mismatches, and `opaque=0` throughout, so there
were no unscreened calls to hide behind.

**`CompletionStages.alwaysTrue` never returned a wrong answer.** The
hypothesis that a bad `filter.test` sends `ArrayLoop.next(int)` skipping to
`end` is dead, and it is dead on evidence rather than on a clean streak: this
is a negative result taken WHILE the failure was happening, with full coverage,
which is exactly what the probe was built to make possible. §5.3's arithmetic
does not apply to it.

What that removes from suspicion: the `IntPredicate` SAM dispatch, its thunk,
its inline cache, and the constant body itself. What it leaves: `ArrayLoop`'s
own `current`/`end` reads and writes, `consumer.apply(index)`'s stage, and
`asyncWhile` — and, per §5.1, the more likely target is not the loop at all but
a stage completing before its transaction did.

#### The engagement counter earned its keep immediately

The first two builds of this probe read `site_const_screened=0`. Both were
wrong in ways no amount of running would have revealed:

1. The screens covered only the two COMPILED arms. `alwaysTrue` is two bytes
   and may never be nominated for compilation, so the interpreted frame path
   had to be screened too.
2. This VM hands out a zero-padded `code` slice — `alwaysTrue` arrives as
   `[04, ac, 00, 00]`, not `[04, ac]` — and an exact-length slice pattern
   matched nothing. It now matches a PREFIX, which is sound because the prefix
   ends in an unconditional return and the site gate requires an empty
   exception table; `a_handler_makes_the_prefix_argument_invalid` and
   `trailing_padding_does_not_hide_a_constant_body` pin both halves.

A probe reporting `mismatch=0` without `screened` beside it would have passed
for a clean bill of health twice.

#### Reading it

```bash
CRATONVM_JIT_LAMBDA_CONST_PROBE=strict CRATONVM_DBG=lambda-jit ./hibfix-dupins-loop.sh hunt /path/to/cratonvm.exe 40
```

A `[cratonvm-lambda-const] WRONG ANSWER` line names the impl, the expected and
observed values, the raw register word, and which arm served the call. One such
line closes this section.

### 5.7 What to try next

The 20% failure rate measured over the 40-run batch makes arms affordable
again: at that rate a 20-run clean arm is p = 0.012. Use
`hibfix-dupins-loop.sh` and interleave the control with each test arm.

1. **The parity of §5.1, first.** Query `Entity` immediately after a failing
   run and split `name` on `__` to recover each verticle's sequence numbers.
   Repeated sequence -> the downstream event fired twice, which is what this
   test was written to catch. Missing sequence -> the loop skipped an index.
   Neither -> the count is simply where the verticle died, and the parity is
   telling us something about the failure's timing instead. This needs no VM
   change and no new instrument.
2. **`withTransaction` completing early**, per §5.1: a verticle with
   `ITERS 60` still hit HR000090, and HR000090 precedes the first duplicate in
   8 runs out of 8. That points at the transaction's completion stage rather
   than at the loop.
3. Re-run the `CRATONVM_JIT_DENY` bisect at ≥20 runs per arm. `ArrayLoop` and
   `AsyncTrampoline` are still on the list; `alwaysTrue` is **off** it (§5.6).
4. `--nojit` passed 8/8 and JIT 6/8 at the old rate; redo that comparison at
   the current rate before leaning on it.

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

1. **The cut loop (§5).** A correctness bug beats a throughput one, and it is
   separable. See §5.7 for the concrete next steps, and §5.3 for the arm size
   any of them needs.
2. **Lambda/SAM dispatch inside a composition chain**, per §6's closing
   paragraph — that is where the remaining `thenCompose` microseconds are, and
   it is not the native funnel.
3. Do **not** re-run the experiment in §6. It is done, it is measured, and the
   numbers are above.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner && ./hibfix-mtins-run.sh cv HOTSPOT hibfix-common-mysql-longto.args -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest testIdentityGeneratorWithTransaction
```

For the cut loop of §5 the repro is a LOOP, not a run — the failure rate is
10-15%, so a single green run means nothing:

```bash
cd apps/hibernate-reactive-suite-runner && ./hibfix-dupins-loop.sh ctl /path/to/cratonvm.exe 40
```

`hibfix-common-mysql-longto.args` is `common.args` with `-Ddb=PostgreSQL` ->
`-Ddb=MySQL` and the JUnit default timeout raised to 900 s; the argfile itself
is generated and machine-local, so it is not committed.

## Related files

- `apps/hibernate-reactive/hibernate-reactive-core/src/test/java/org/hibernate/reactive/MultithreadedInsertionWithLazyConnectionTest.java`
- `apps/hibernate-reactive-suite-runner/HibfixCfProbe.java`, `HibfixCfBound.java`, `hibfix-mtins-run.sh`, `hibfix-dupins-loop.sh`
- `jit/src/lambda_adapter.rs` — the `AdapterKey` of §5.4 and its `site_shape_collisions` counter
- [`hib-reactive-3gc-run-regressions-20260820.md`](hib-reactive-3gc-run-regressions-20260820.md) §8
- [`batchtest-mysql-jdbc-batching-slow-20260822.md`](batchtest-mysql-jdbc-batching-slow-20260822.md) — the same "trivial JDK primitive served by a native" shape, and the same conclusion that the funnel's aggregate is small
