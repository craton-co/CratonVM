# `MultithreadedInsertionWithLazyConnectionTest` — NOT FIXED. A ~6x throughput gap on `CompletableFuture` composition crossing the test's own 10-minute budget, plus a separate CUT LOOP that ends after one iteration

## Status
**OPEN (2026-08-23). Diagnosed, decomposed, not fixed.** One component was
removed and measured (see §6), and it is **not** enough: the change is neutral
on this workload. Nothing here is a single defect — closing this test needs the
reactive composition path to get several times faster.

A second, *separate* finding is recorded in §5, and it is the more interesting
half: a `CompletionStages.loop` that terminates after ONE iteration instead of
sixty. That is now measured directly rather than inferred (§5.1), and both the
duplicated INSERT and HR000090 are downstream of it. It is **not fixed**. §5.2
and §5.3 are the two things to read before touching it: the repro dies if the
table is truncated, and the failure rate is too low for the arm sizes the
earlier bisect used.

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

### 5.1 What the primary event actually is — measured, not inferred

`InsertEntitiesVerticle` already counts its own `storeEntity` calls
(`sequentialOperation`). Logging that counter in the verticle's `whenComplete`
handler — the CONSUMER, not the loop — settles what the row count cannot: a
short loop and a lost insert produce the same number of rows.

On a failing run at `-Dmti.threads=24 -Dmti.n=60`:

```
run 6: rows=1328/1440 dup=5 hr90=1 iters=[ITERS 1]
```

**One verticle ran the loop body exactly once instead of sixty times.** The
other 23 reported `ITERS 60`. So the primary event is a `CompletionStages.loop`
that terminates after one iteration; `session.close()` then runs on a live
transaction (HR000090), and the duplicate INSERTs follow from the resulting
rollback/retry traffic — the Duplicate entry lines never appear without a cut
loop first.

That matches the shape of `ArrayLoop`:

```java
public CompletionStage<Boolean> next() {
    current = next( current );                 // skips while !filter.test(index)
    if ( current < end ) { … return consumer.apply( index ); }
    return FALSE;                              // loop over
}
```

A single wrong `filter.test` answer (the filter is `CompletionStages::alwaysTrue`,
which is a constant `return true`) sends `next(int)` straight to `end` and ends
the loop silently, with no exception anywhere.

### 5.2 The repro depends on TABLE SIZE, and a truncating harness measures nothing

This is the trap that cost the most here. The `Entity` table is never cleaned by
the suite, so it accumulates: it held ~140 000 rows when the failure was first
seen. A loop that truncates it between runs — the obvious way to make the row
count meaningful — **destroys the repro entirely**:

| `Entity` rows at run start | runs | failures |
|---|---:|---:|
| 0 (truncated each run) | 18, across 3 binaries and both `LAMBDA_ADAPTER` settings | **0** |
| 141 440 (filler restored) | 6 | 1 (`rows=60/1440`, hr90=25, dup=5) |
| 361 440 | 8 | 1 (`rows=1388/1440`, hr90=3) |

`hibfix-dupins-loop.sh` therefore deletes only `id > 0` and keeps negative-id
filler rows, which can never collide with the sequence. `Entity_SEQ` climbs
monotonically and is never reset, so leftover rows cannot themselves cause a
duplicate id — the duplicate is an in-run race, and the table size only widens
the window.

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

Measured on this workload (24 threads, N=60, filler rows present):

| mode | `screened` | `mismatch` | `opaque` |
|---|---:|---:|---:|
| `=1` | ~500 / run | 0 | 2 |
| `=strict` | 4319 | 0 | **0** |

and the constant-return site it finds is exactly the one the investigation
pointed at: `CompletionStages.alwaysTrue(I)Z`.

**The probe has not yet caught anything**: 10 runs at `=1` all passed, so there
was no failure for it to screen. That is not evidence of correctness — it is
the base-rate problem of §5.3 again, seen from the other side. What the probe
changes is that ONE failing run is now enough.

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

1. Re-run the `CRATONVM_JIT_DENY` bisect at **≥40 runs per arm** with
   `hibfix-dupins-loop.sh` and the filler rows in place, control arm interleaved
   with each test arm rather than measured on a different day.
2. **Run the probe of §5.6 alongside it.** It is built, armed and verified to
   engage on `alwaysTrue`; it just has not seen a failing run yet. Prefer
   `=strict` when the goal is coverage and `=1` when the goal is to keep the
   codegen honest, and read `site_const_screened` before believing any zero.
   One `WRONG ANSWER` line closes this.
3. `--nojit` passed 8/8 and JIT 6/8 at the old rate; that comparison also needs
   redoing at the current rate before it is leaned on.

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
   separable. See §5.6 for the two concrete next steps — and §5.2 before
   running anything, because a harness that truncates the table reproduces
   nothing at all.
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

It needs the negative-id filler rows of §5.2 present in `Entity`.

`hibfix-common-mysql-longto.args` is `common.args` with `-Ddb=PostgreSQL` ->
`-Ddb=MySQL` and the JUnit default timeout raised to 900 s; the argfile itself
is generated and machine-local, so it is not committed.

## Related files

- `apps/hibernate-reactive/hibernate-reactive-core/src/test/java/org/hibernate/reactive/MultithreadedInsertionWithLazyConnectionTest.java`
- `apps/hibernate-reactive-suite-runner/HibfixCfProbe.java`, `HibfixCfBound.java`, `hibfix-mtins-run.sh`, `hibfix-dupins-loop.sh`
- `jit/src/lambda_adapter.rs` — the `AdapterKey` of §5.4 and its `site_shape_collisions` counter
- [`hib-reactive-3gc-run-regressions-20260820.md`](hib-reactive-3gc-run-regressions-20260820.md) §8
- [`batchtest-mysql-jdbc-batching-slow-20260822.md`](batchtest-mysql-jdbc-batching-slow-20260822.md) — the same "trivial JDK primitive served by a native" shape, and the same conclusion that the funnel's aggregate is small
