# `MultithreadedInsertionWithLazyConnectionTest` — NOT FIXED. A ~6x throughput gap on `CompletableFuture` composition crossing the test's own 10-minute budget, plus a separate defect that silently DROPS inserts — 1200 attempted, 93 landed

## Status
**OPEN (2026-08-23). Diagnosed, decomposed, not fixed.** One component was
removed and measured (see §6), and it is **not** enough: the change is neutral
on this workload. Nothing here is a single defect — closing this test needs the
reactive composition path to get several times faster.

A second, *separate* finding is recorded in §5, and it is the more interesting
half: INSERTs are silently LOST, and the loss carries a SUCCESS signal —
959 transactions reported committed against 56 rows on the table (§5.8). No
sequence number is ever committed twice (§5.1), so the event is DROPPED rather
than duplicated. The first error in a failing run is `HR000089: Connection is
closed` raised inside the ID GENERATOR's CAS retry, meaning a retry outlived
its session; the thread-safety assertion is a late symptom, and the CAS storm
is an EFFECT (§5.9). Six hypotheses are measured and dead; the composition
path is CORRECT but 64-95x slower than HotSpot on the retry shape. It is
**not fixed**. §5.3 is the thing to read
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

#### The `name` column answers it: the event is DROPPED, not duplicated

`storeEntity` sets `entity.name = <thread>__<localVerticleOperationSequence>`,
so the rows ARE each verticle's sequence log and need no new instrument.
`hibfix-seqcheck.sh` runs until a failure and queries before the next run drops
the table.

Control, on a passing run: 1440 rows, 1440 distinct names, every thread
complete over `0..59`.

On a failing run (24 threads, N=60, 26th attempt):

| | |
|---|---:|
| `storeEntity` calls made (sum of `ITERS`) | **1200** |
| rows that landed | **93** |
| distinct names among them | **93** |
| repeated `(thread, seq)` pairs | **0** |
| `Duplicate entry` errors | 19 (14 distinct ids) |
| HR000090 | 23 |

**No sequence number is ever committed twice.** The test's javadoc worries
about a downstream event "being processed twice (or more) concurrently"; what
this measures is the other half of the same sentence — events being
**dropped**. 1200 inserts were attempted and 1107 vanished, while only 19 of
them produced any error at all. The rest failed silently.

The surviving rows show the shape. Gaps are scattered, not a truncated tail:

```
thread-13: 3,4,5,6,7,8, _ ,10,…,16, _ ,18,…,28     (0,1,2 never landed; 9 and 17 missing)
thread-16: 1,…,11, _ ,13,…,16                      (0 never landed; 12 missing)
thread-14: 0,…,8                                    (complete)
```

so individual iterations disappear mid-stream while the verticle carries on —
and fourteen verticles reported `ITERS 60`, a full loop, yet no thread landed
more than 24 rows.

That is the mechanism §5.1 describes, now with numbers behind it:
`s.withTransaction(...)` hands back a stage that completes before the COMMIT
does. The loop advances on it, the verticle finishes, and at `session.close()`
the transaction is still live — HR000090, "it will be roll backed" — so
everything that had not really committed is discarded. The 19 duplicate-key
errors are a smaller, secondary effect of two verticles racing on the id
sequence; they cannot account for 1107 missing rows.

**Caveat on scope.** A rolled-back row is not in the table, so this rules out
double-COMMIT rather than double-EXECUTION. It does not prove nothing ran
twice — it proves nothing landed twice, and that the dominant effect is loss.

#### The parity, still unexplained

Verticles that stop short do so after an **odd** number of `storeEntity`
calls, 96 times out of 100 across every failing run measured — 88 of 90 in the
40-run strict batch, and 8 of 10 in the independent `hibfix-seqcheck.sh` run
above, which used a different binary and no probe at all. The four exceptions
are 18, 24, 18 and 30.

`sequentialOperation` is a per-verticle field incremented exactly once per
`storeEntity`, so this says a dying verticle has almost always made an odd
number of calls. Under any process that interrupts the loop at a uniformly
random iteration, 96/100 one-sided is not a coincidence.

The sequence query above did NOT explain it: no index is committed twice, and
the gaps are scattered rather than clustered at the end. So the parity is not
"one extra call after an even number of successes". It survives a change of
binary and of probe setting, which rules out the probe as its cause, and it is
the one signal in this page with no candidate mechanism attached.

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

### 5.8 Where the data actually goes, and the four hypotheses that died finding out

#### The loss is silent and it carries a SUCCESS signal

Instrumenting the test's own two lambdas — `WT` counts entries to
`(s, entity) -> s.withTransaction(...)`, `PERSIST` counts entries to the
`t -> s.persist(entity)` work lambda — plus a `whenComplete` on each stage:

| | |
|---|---:|
| `withTransaction` work lambdas invoked | 1016 |
| `persist` stages completing successfully | 965 |
| `withTransaction` stages reporting **success** | **959** |
| rows in the table | **56** |

**`WT == PERSIST` exactly**, so `withTransaction` always runs the work it is
given — it does not short-circuit. And 959 transactions reported success while
903 of those rows do not exist.

Server-side, from MySQL's own general log (`hibfix-commitcheck.sh`):

| stage | count |
|---|---:|
| `storeEntity` calls attempted | 1093 |
| INSERTs that reached MySQL | 134 |
| BEGINs | 86 |
| COMMITs | 62 |
| ROLLBACKs | 24 |

`86 = 62 + 24` on every connection, so the transactions that DO run are
well-formed. The work never reaches the database at all.

#### The first error is the ID GENERATOR on a closed connection

Ordering the errors of a failing run by line number rather than by which looked
most interesting:

| event | first at line |
|---|---:|
| **`HR000089: Connection is closed`** | **113** |
| `HR000090: Live transaction on close` | 133 |
| `NonUniqueObjectException` | 137 |
| `Duplicate entry` | 163 |
| `AssertionFailure: non-threadsafe access` | 372 |

The thread-safety assertion is a LATE symptom, not the root. The first error's
stack names the id generator's optimistic-CAS retry:

```
HR000089: Connection is closed
  at SqlClientPool$ProxyConnection.connection(:274)
  at SqlClientPool$ProxyConnection.selectIdentifier(:402)
  at TableReactiveIdentifierGenerator.nextHiValue(:112)
  at TableReactiveIdentifierGenerator.checkValue(:149)
  at TableReactiveIdentifierGenerator.lambda$nextHiValue$1(:140)
```

`checkValue` retries `nextHiValue` whenever the CAS `update Entity_SEQ set
next_val=NEW where next_val=OLD` affects 0 rows. Reaching a CLOSED connection
there means **a retry outlived the session that owned it**.

#### The CAS degenerates into a retry storm — direction of causation UNKNOWN

`hibfix-seqrace.sh` reads the CAS traffic off the server:

| run | total UPDATEs | distinct | attempts per block |
|---|---:|---:|---:|
| HotSpot, passing | 57 | 53 | **1.1** |
| CratonVM, passing | 56 | 52 | **1.1** |
| CratonVM, FAILING | 97 | **8** | ~12, one UPDATE tried **30x** |

A healthy run has essentially no contention on the sequence row. A failing run
piles 30 racers onto a single `101 -> 151` and only ever allocates 8 blocks.

**This is not yet a cause.** The storm is equally consistent with being an
EFFECT: once inserts stop happening (134 of 1093), the verticles stop waiting
on I/O and spin through iterations, which is exactly what would pile them onto
the id generator. Establishing the order needs the general log's `event_time`
correlated against the first `HR000089` on a run that is kept — the first
attempt at this lost its data when the control run truncated
`mysql.general_log`.

#### Four hypotheses killed, three of them without any statistics

* **`alwaysTrue` returning a wrong `false`** — 156 569 constant-return calls
  screened across 8 failing runs, 0 mismatches, 0 opaque (§5.6).
* **`Thread.currentThread()` identity under JIT** — `EventLoopExecutor.inThread()`
  reduces to netty's `Thread.currentThread() == this.thread`, so a wrong TRUE
  there would run a continuation on a foreign event loop.
  `HibfixThreadIdentityProbe` reproduces exactly that shape: **48 000 000
  checks on 24 threads, 0 wrong answers**, matching HotSpot.
* **Connection-pool sharing** — the ~2 INSERTs per BEGIN seen in failing runs
  suggested two sessions per physical connection. On PASSING runs both runtimes
  are byte-identical: 6 connections, 1440 begins, 1448 inserts, 1440 commits.
  Pool usage is not the defect.
* **The `gc::guard` "descriptor-aware field access DESTROYED the value"
  warning** — with the ANSI escapes stripped, passing and failing runs carry
  the identical five shapes (`class_id` 12/64/158/1602, ~28 lines each). Noise.

#### What IS established

The failure is JIT-dependent. Interleaved on one binary, alternating arms:
**JIT 2/8 failures, `--nojit` 0/8**, and the failing JIT run finished in 15 s
against ~35 s healthy because it did almost no work.

#### The one structural fact worth carrying forward

The continuation in every one of these traces is resumed from
`FutureBase$EmitResultTask.run` called by netty's `runAllTasks` — i.e. it was
SCHEDULED as a task, not run inline. Whatever goes wrong, it goes wrong in what
the future's context was when the task was posted, not in the inline-vs-schedule
decision itself.

### 5.9 The CAS storm is an EFFECT, and the composition path is correct but 64-95x slow

#### Timeline: the id block runs out, and everything happens at once

`hibfix-seqtime.sh` buckets INSERTs and `Entity_SEQ` traffic into tenths of a
second off MySQL's own clock, so no cross-log correlation is needed. A failing
run:

| ds (0.1s) | inserts | seq updates | seq selects | commits |
|---:|---:|---:|---:|---:|
| 56-59 | 10-12 | **0** | **0** | 11-14 |
| 60 | 6 | 2 | 10 | 6 |
| 61-69 | 0-3 | 4-6 | 2-6 | 0-2 |
| 78-95 | 3-15 | 0-6 | 0-6 | 4-9 |

The healthy phase has **no sequence traffic at all** — the pooled optimizer's
50-id block is still being handed out. At ds 60 the block runs out, all 24
verticles need an id at once, and the inserts collapse in the same bucket.

The app's first `HR000089` is at its own second 5, i.e. BEFORE the storm at
6.0. **So the CAS storm is not the first event**, and §5.8's open question is
answered: it is an effect, or at best a co-symptom. The search does not stop at
the id generator.

#### Three probes, three clean results, and one very large number

Each probe reproduces one link of the retry path and nothing else, at volume,
hot enough to compile:

| probe | what it isolates | CratonVM result | HotSpot | CratonVM |
|---|---|---|---:|---:|
| `HibfixThreadIdentityProbe` | `Thread.currentThread() == field`, i.e. netty's `inEventLoop()` | **48 000 000 checks, 0 wrong** | 1.2 s | 12.0 s |
| `HibfixComposeProbe` | `thenCompose` relaying a recursive, async inner stage | **480 000 chains, 720 000 retries, 0 early, 0 wrong** | 3.8 s | **361.5 s** |
| `HibfixVertxBridgeProbe` | `io.vertx.core.Future.toCompletionStage()` | **144 000 crossings, 0 early, 0 wrong, 0 lost** | 0.75 s | **48.0 s** |

Every link is CORRECT. And the composition links are **95x** and **64x**
slower than HotSpot on precisely the shape `nextHiValue` retries through — far
worse than the ~2.5x engine floor of the table in "Where the time goes".

#### The 95x was under-measured, and the cause is `VarHandle`

`HibfixComposeProbe`'s own profile showed ~45% of its time in the
`ScheduledThreadPoolExecutor` it used to complete the inner stage — AQS,
`ReentrantLock`, `DelayedWorkQueue` — against ~24% in `CompletableFuture`. Its
95x was a real ratio for a mixed workload but NOT a measurement of composition.

`HibfixComposeProbe2` removes the scheduler entirely: each thread owns its
futures, the gates are completed after composition so every relay still takes
the not-yet-complete path, and there is not a lock in it.

| | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| 4 800 000 compose chains | **412 ms** | **359 197 ms** | **~872x** |

The scheduler had been DILUTING the cost, not inflating it. `wrong=0` over 4.8
million chains, so this is purely cost.

Profiling that clean run:

| frame | share |
|---|---:|
| `CompletableFuture.tryPushStack` | **34.0%** |
| `CompletableFuture$UniCompose.tryFire` | 21.4% |
| `CompletableFuture.completeRelay` | 12.4% |
| `CompletableFuture.uniComposeStage` | 8.0% |

`tryPushStack` is `NEXT.set(c, h)` then `STACK.compareAndSet(this, h, c)` — two
`VarHandle` operations on a reference field, **uncontended** here because each
thread owns its futures.

That led to `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
(FIXED and retired to the internal tree on 2026-08-27):

| operation | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `VarHandle.compareAndSet` reference | 9.2 ns | 488.9 ns | 53x |
| `VarHandle.set` reference | 1.0 ns | 303.4 ns | **303x** |
| `VarHandle.compareAndSet` int | 8.6 ns | 300.2 ns | 35x |
| `AtomicInteger.incrementAndGet` | 5.0 ns | 6.2 ns | **1.2x** |

`AtomicInteger` at parity is what makes it conclusive: this is `VarHandle`
specifically, not atomics and not the box. The VM's `VARHANDLE_READ_DIRECT_FNS`
binds READS of PRIMITIVE fields only — its own doc says `L` and `[` "are absent
on purpose" — so every write and every CAS still pays the generic dispatch
funnel.

**This is the composition cost, and it is a far better lead than anything else
on this page**: it is deterministic, needs no database and no failing run, and
it is the plausible enabling condition for the correctness failure here — a
sequence race HotSpot settles in microseconds run through machinery two orders
of magnitude slower.

#### What that buys, and what it does not

It explains the SHAPE of the failure without yet naming the defect. A
sequence-allocation race that HotSpot resolves in microseconds is being run
through machinery two orders of magnitude slower, which is how a normally
uncontended block hand-off (1.1 attempts per block, measured, both runtimes)
becomes a 30-way pile-up.

**But slowness alone is refuted as the cause**: `--nojit` is slower still and
is 0/8 (§5.8), where JIT is 2/8. So the operative variable is not mean speed —
it is the VARIANCE the JIT introduces, with some threads running compiled and
some interpreted or deoptimizing, which is what lets many verticles arrive at
the same `next_val` together. That is a hypothesis, and it is the first one
here that both fits `--nojit` and explains the 1.1-vs-30 attempt counts.

#### Six hypotheses now dead

`alwaysTrue` (§5.6), thread identity, connection-pool sharing, the `gc::guard`
warning (§5.8), `thenCompose` relaying, and the Vert.x bridge. Four of the six
were killed by standalone probes in seconds rather than by batches in minutes,
which is the method to keep: reproduce the exact shape, run it hot, count.

### 5.10 What to try next

The rate drifts between 0% and 25% with no code change, so size arms by §5.3
and **discard any arm whose control did not fail**.

1. **DONE (2026-08-27), and it was not the `VarHandle` funnel.** The funnel
   was fixed — `set` bound, the global mutex removed, the CAS served in-funnel,
   the volatile stripe pool unpacked from a single cache line — and composition
   barely moved. What the 872x actually was: `UniCompose.tryFire` and
   `UniRelay.tryFire` force-interpreted by a stale `ForkJoinTask`-subclass
   blocklist, plus a `dup_x2` shape the single-pass backend could not prove.
   Composition is **3.95x** faster and the gap is 446x -> 113x. See
   `performance/completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`
   and `performance/varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`.
   The residual is
   `performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md` (internal), whose successor
   `performance/completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
   (internal) closed on 2026-09-02 with composition **1.19x** faster and all
   three of its residuals discharged. What is still open from that line is
   [`../perf/composition-native-callback-and-the-promotion-question-20260902.md`](../perf/composition-native-callback-and-the-promotion-question-20260902.md).
2. **Test the variance hypothesis of §5.9 directly.** It predicts that
   anything reducing JIT timing variance reduces the failure, while anything
   reducing mean speed does not. `CRATONVM_BG_COMPILE=0` (synchronous
   compilation) and a pinned tier are the two levers that change variance
   without changing the code path. Interleave with a control.
3. **Done — the 95x is now the 872x of §5.9, and profiled.** `HibfixComposeProbe2`
   is the reproducer: 412 ms against 359 s, no database, no flake.
4. **Find the event before `HR000089`.** It is the first error that reaches
   the test's own hooks, but those hooks only wrap `withTransaction` and
   `persist`. Something aborts an iteration before that; wrapping the loop
   body's returned stage would catch it.
5. Do NOT re-run, all measured and negative: the `alwaysTrue` screen (§5.6),
   thread identity, pool comparison, guard comparison (§5.8), `thenCompose`
   relaying, and the Vert.x bridge (§5.9).

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

1. **The silent insert loss (§5).** A correctness bug beats a throughput one,
   and it is separable. See §5.10 for the concrete next steps, §5.8 and §5.9
   for what is already measured and negative, and §5.3 for the arm size any of
   them needs.
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
- `apps/hibernate-reactive-suite-runner/HibfixCfProbe.java`, `HibfixCfBound.java`, `hibfix-mtins-run.sh`, `hibfix-dupins-loop.sh`, `hibfix-seqcheck.sh`, `hibfix-commitcheck.sh`, `hibfix-seqtime.sh`, `HibfixComposeProbe.java`, `HibfixVertxBridgeProbe.java`, `hibfix-wtcheck.sh`, `hibfix-arms.sh`, `hibfix-jitab.sh`, `hibfix-seqrace.sh`, `HibfixThreadIdentityProbe.java`
- `jit/src/lambda_adapter.rs` — the `AdapterKey` of §5.4 and its `site_shape_collisions` counter
- `fixed-suite-bugs/hibernate/hib-reactive-3gc-run-regressions-FIXED-20260824.md` §8
- `fixed-suite-bugs/hibernate/batchtest-mysql-jdbc-batching-NOT-A-VM-DEFECT-20260822.md` — the same "trivial JDK primitive served by a native" shape, and the same conclusion that the funnel's aggregate is small

---

## 8. 2026-08-24 — the `invokedynamic` bridge does NOT retire this class (checked, negative)

Recorded because the obvious question after
`fixed-suite-bugs/jit/techempower-wrong-answer-was-the-indy-trap-FIXED-20260824.md`
is whether the same merge helps here. `TechEmpowerTest` was retired by
`730d3e0d9` (compiled code can now EXECUTE an `invokedynamic`), and this class
sits in the same reactive-dispatch cost family, so it is a reasonable thing to
hope for. **It does not.**

Local Windows box, live Postgres via Testcontainers, binary built from `dev`
`b70870c36` (indy merge confirmed present by `merge-base --is-ancestor`), JUnit's
own per-test timer raised to 900 s through `HR_CLASS_OVERRIDES` so the fixture's
hardcoded `@Timeout(10, MINUTES)` is what binds:

| arm | `ok` / 2 | wall |
|---|---:|---|
| default (indy bridge on) | 1 | 674 s |
| `CRATONVM_JIT_INDY_BRIDGE=0` | 0 | 666 s |
| `CRATONVM_JIT_INDY_BRIDGE=0` | 1 | 97 s |
| `CRATONVM_JIT_INDY_BRIDGE=0` | 1 | 669 s |

**Status on today's `dev` is unchanged from this page's own:** 1 of 2, with
`testIdentityGeneratorWithTransaction` still exceeding the fixture's own
deadline. The indy work does not close it, and nobody should re-run this
expecting otherwise.

### 8.1 A claim this section deliberately does NOT make

The first `INDY_BRIDGE=0` run had `testIdentityGenerator` (the *non*-transactional
method, which §Status records as passing) fail with
`ConstraintViolationException: duplicate key value violates unique constraint
"entity_pkey"`. On one run that reads like "the indy bridge is what keeps this
method correct" — the same wrong-answer shape the TechEmpower page pins on that
trap.

Two further runs of the same arm did not reproduce it: **1 of 3**. So the event
is intermittent and is NOT attributable to the flag on this evidence; it is at
least as consistent with the duplicate-INSERT behaviour §5 already treats as
downstream. Establishing a real rate difference here needs the kind of run count
§5 used (40), not three.

It is written down only so the next reader who sees one duplicate-key failure
under that flag knows it has been seen, and knows it did not survive repetition.
