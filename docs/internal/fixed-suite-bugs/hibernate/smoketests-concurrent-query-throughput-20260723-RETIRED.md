# Hibernate `SmokeTests#testQueryConcurrency` — the 120 s timeout — RETIRED 2026-08-05

> Retired from `docs/known-issues/hibernate/smoketests-concurrent-println-timeout-20260723.md`.
> **Every load-bearing number and every lever in that page was stale**, most of
> them by more than an order of magnitude, and two of its three "reproduce it
> like this" instructions could not be followed at all: the probes it points at
> were never committed, and the environment variable its central experiment
> turns on was deleted from the codebase on 2026-07-31. What follows is the
> re-measurement, the fixes that came out of it, and what is genuinely left.

## Status

**Closed as written; the class is now an ordinary timeout-marginal one.**

Run against its REAL cap — `common.args` sets
`-Djunit.jupiter.execution.timeout.default=120s`, so an ordinary invocation is
the test — on a contended box, five attempts on the final binary gave:

| outcome | count | detail |
|---|---:|---|
| **PASS** | **2** | `ok=1 failed=0`, `test_ms` 118 631 and 125 968, **`sql=40012`** both times |
| timeout | 2 | `TimeoutException`, `sql=17632` / `22696` — killed part-way, as a real timeout looks |
| non-start | 1 | `found=0` after 1 370 ms — the class was never discovered; the instability family below, not a timeout |

`sql=40012` is the whole workload (50 forks × ~802 statements), so the passes
are passes, not the vacuous kind this page also documents. `test_ms` exceeding
120 000 on a pass is expected: it is measured from JUnit's `executionStarted`
and includes `@BeforeEach`/`@AfterEach`, while the cap applies to the method
body.

So the class **completes its 20 000 transactions in roughly 119–126 s** where
the old page recorded **614 608 ms**, and it now sits either side of the line
depending on host load. That puts it in the category this suite's README
already has — "timeout-marginal class + contended host", alongside
`BatchTest`, `DynamicBatchFetchTest` and `OracleInlineMutationStrategyIdTest` —
rather than in the "needs a 5.2x breakthrough, gated on the tiered manager"
category the old page invented for it.

The two sub-defects that page listed as "worth fixing on the way" are fixed or
no longer reproduce. What remains is that marginal-host behaviour and an
unrelated stability defect belonging to another page — both stated below with
their measurements, neither of them the thing this page was tracking.

A note on attribution: two runs of the PRE-fix binary interleaved with the
above gave one timeout and one death, which is too few samples to claim this
branch changed the pass rate, and this branch's measured wins (+11% on array
allocation at 4 threads, five methods newly compilable) are not big enough to
expect it to. **The gap was already ~22x before this work**; what this page
corrects is the record, not primarily the number.

## The correction, in one table

Every row was re-measured on 2026-08-05 against dev tip, on the same box, with
HotSpot JDK 25 as the control.

| the old page said | measured 2026-08-05 |
|---|---|
| CratonVM 614 608 ms vs HotSpot 15 943 ms — **38.6x** | **~22x** (see the fits below) |
| "closing it needs **~5.2x**" | needs **~1.3x**, and less than that on a quiet host |
| `--nojit` is 4% FASTER — "the JIT is worth 4%" | JIT is a **~1.6x WIN** over `--nojit` |
| compiling Hibernate is **2.2x WORSE** | not reproducible: the experiment's switch no longer exists, and the default already compiles Hibernate |
| **1531 of 1642** hot methods never compile; 1 ms spent compiling | **39 of 1115** still interpreted; **876 at C2**; 57 ms compiling |
| 99.6% of non-compiles are **banned by design** | `ineligible-by-policy=0` — the static ban list was deleted 2026-07-31 |
| six named compile failures, "the actionable set" | none of those six; a different set of ~23, now each naming its refusing site |
| quieting JDBC TRACE logging is worth ~10% | CratonVM emits **zero** such lines; there is nothing to quiet |
| three probes "left behind in `apps/hib-suite-runner/`" | **none of them existed** — not in git, not on disk |

## How the current numbers were obtained

The old page's single data point (one 10-minute run with the cap lifted) cannot
separate the fixture's fixed setup cost from its per-fork cost, and the two
differ by two orders of magnitude. The fixture takes `-Dcraton.smoke.forks=N`,
so fit a line instead.

`MethodRunner`, `-Djunit.jupiter.execution.timeout.default=3600s`, `test_ms`:

| forks | HotSpot | CratonVM |
|---:|---:|---:|
| 2 | — | 17 009 |
| 6 | 2 912 | — |
| 8 | — | 34 462 / 33 506 |
| 14 | 3 651 | — |
| 26 | 4 783 | — |

* HotSpot: **93 ms/fork** marginal, ~2 350 ms fixed ⇒ 50 forks ≈ **7 000 ms**.
* CratonVM: **~2 832 ms/fork** marginal, ~11 300 ms fixed ⇒ 50 forks ≈ **153 000 ms**.

That is **~22x** overall and **~30x** on the marginal cost, against the old
page's 38.6x — and it puts the real test at ~1.3x over the 120 s cap rather than
5.2x. Both CratonVM rows were taken while the box was compiling; see
"Measurement conditions" below before treating 153 s as the floor.

`SmokeConcProbe` (restored, see below) agrees independently: its HotSpot
`PROJECTED_TEST_MS` of 7 083 lands within **2%** of the 7 000 ms the line above
predicts for the real test. The probe is a faithful stand-in, not an
approximation of one.

## The claim that mattered most, and why it is gone

The old page's conclusion — "the JIT is currently a net negative on
dispatch-heavy ORM code … closure is therefore gated on the tiered-manager
work, not on anything local to this test" — rested on one row of its lever
table: `CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/` making the run 2.2x slower.

**That experiment cannot be run today, and its premise is inverted.**
`CRATONVM_JIT_ALLOW_PACKAGES` was deleted along with `vm/src/jit/skip_list.rs`
on 2026-07-31; nothing reads it. It existed to *lift* a ban, and the ban it
lifted — `org/hibernate/` wholesale, plus `org/h2/` and the
`AbstractQueuedSynchronizer` `getState`/`setState`/`compareAndSetState` family —
is gone too. Hibernate is compiled by default now, so an ordinary run **is** the
stronger form of that experiment.

Run interleaved (A-B-B-A, both orders, `SmokeConcProbe 4 400 5 2 full`,
`PROJECTED_TEST_MS`):

| arm | samples | median |
|---|---|---:|
| default (JIT compiles Hibernate) | 128 675 · 120 813 · 126 375 · 148 900 | **~127 000** |
| `--nojit` | 166 400 · 172 675 · 241 588 · 264 138 | ~207 000 |

The JIT is a **~1.6x win**, not a 1.5–2.2x loss. Do not cite the old finding for
any workload without re-measuring — see
`reference_jit_invoke_cache_thrash_dispatch_heavy`'s own 2026-08-04 update,
which retracts the same claim for a different Hibernate test after
`try_jit_compile_callee_slow` was fixed to scan an inherited method's bytecode
against its *declaring* class's constant pool.

## What the compile picture actually looks like now

`CRATONVM_DBG=jit-method-stats` on `SmokeConcProbe 2 200 5 1 full`:

```
1115 distinct methods tracked, 1110 ever invoked, 571293 total invocations
still-interpreted=39  c1=200  full-profile=0  c2=876
compiles: c1=1100 c2=879 osr=5 deopts=61 c2_bailouts=10 total_compile_time_ms=57
c1_threshold=500  hot_but_stuck_in_interpreter=34 (of which ineligible-by-policy=0, compile-failures=23)
```

`ineligible-by-policy=0` is the whole of the old page's "99.6% banned by
design". The remaining 23 are real refusals, and each now names the site that
refused it. The distribution:

| reason | count | note |
|---|---:|---|
| `unrecorded` | 39 | **a diagnostic gap, still open** — see below |
| `rbc6-handler-reads-unsafe-local(pc=…,op=…)` | 5 | five *different* blocking opcodes; one now fixed |
| `singlepass-codegen(pc=…,op=…)` | 2 | `0x58` pop2, `0xb8` invokestatic |

None of the old page's six named failures (`Instant.create(JI)`,
`ATNSimulator.getCachedContext`, `ATNDeserializer.stateFactory(II)`,
`IdentityHashMap.clone()`, `LinkedBlockingQueue.take()`/`.offer(Object)`) is in
the current set. Do not use that list.

### The RBC.6 refusals now name the bytecode that caused them

`reason=rbc6-handler-reads-unsafe-local` named a *policy*, not a cause. Five of
the hottest still-interpreted methods on this workload shared it, and nothing
said what they shared. With the pc and opcode attached they turn out to be
blocked by five different opcodes, needing five different pieces of work:

| opcode | what it is | status |
|---|---|---|
| `0xc1` | `instanceof` | **admitted** — see below |
| `0xbb` | `new` | open: allocation failure publishes no frame |
| `0x12` | `ldc` (String/Class constant) | open |
| `0xb2` | `getstatic` | open: `<clinit>` can throw |
| `0xbf` | `athrow` | open |

`instanceof` was a pure over-approximation, and an expensive one. Its x64
lowering emits a single call to `jit_instanceof`, which returns `0` or `1` on
every path — a null or implausible receiver, a non-UTF-8 class name, an address
the heap does not recognise and an unresolvable target class all fail soft to
`0` — and never stashes a pending exception. The codegen emits no post-call
exception check after it because there is nothing to check. A site that raises
no exception cannot hand a handler an unpublished frame, so the gate protected
nothing while refusing every method with an `instanceof` inside a `try`.

It had already cost this codebase a regression test:
`JitPreciseHandlerFrame.loopStep`'s first draft had an `instanceof` in its
protected range, which refused the whole method and made that test read 0
mismatches in **both** arms of its own A/B. The fixture's own comment records
it. There is now an `instanceofStep` shape that exercises the admission
directly, and its test asserts the method is neither RBC.6-refused nor
bail-listed — because `mismatches == 0` on its own is exactly the false pass
that comment describes.

Verified: `jit_local_exception_handler_tests` goes from **13 passed / 3 failed**
(every change in this branch stashed) to **14 passed / 3 failed**, the same
three failures both times — see the section on those below. A static-admission
twin lives in the `jit` crate
(`protected_instanceof_is_precise_exception_covered`) and holds the neighbour
line: `checkcast` (0xc0), which shared the old `0xbb..=0xc1` range arm, must
still withhold coverage and must still name its own pc.

### `reason=unrecorded` is the largest group and is still open

39 methods failed to compile with no site recorded at all, `java/time/Instant.now()`
and `Instant.ofEpochSecond(JJ)` among them. They emit no `compile-bail` line
under `CRATONVM_DBG_JITC` either, so they are failing somewhere that never calls
`note_jit_bail_site` — the most likely candidate being `try_compile`'s early
`compile_gate::admit` rejection, which returns `None` without recording
anything. Not fixed here; it is a diagnostics defect, not a compiler one, and it
is what to attack next if this population matters.

## Fixed under this investigation

### The JIT had no TLAB path for arrays at all

The old page's sub-defect 2 ("the JIT's array-allocation path does not scale
across threads") was real and is fixed. The mechanism was simpler than the page
implied: there was no TLAB arm on the JIT's array path *in any form*. Its object
sites bump inline (`emit_inline_tlab_new`) and miss into
`tlab_alloc_object_guarded_refill`; its array sites had neither, so
`jit_newarray` and `jit_anewarray_object` were not slow paths — they were the
*only* path a compiled array allocation had, and each took the global
`young_from` mutex twice, the second time across the bump, the zeroing and the
header init, so hold time scaled with the array's size. The interpreter grew its
TLAB arm on 2026-07-25 (`7888b80b6`) and the JIT's was simply left behind.

`tlab_alloc_array_guarded_refill` is the array twin of the object helper, called
first from both. Measured on `AllocScaleProbe`, interleaved A-B-B-A, `long[16]`
aggregate allocations/s:

| | 1 thread | 4 threads |
|---|---:|---:|
| before | 1 212 416 · 1 126 400 | 2 485 589 · 2 618 709 |
| after | 1 198 080 · 1 277 952 | 2 897 920 · 2 788 010 |

**+11% aggregate at 4 threads, unchanged at 1** — the shape a scaling fix has,
and the shape a general speed-up does not. `--nojit` is still slightly ahead
(3 013 973 at 4 threads), so the JIT's array path has not fully caught the
interpreter's; the remaining difference is the helper-call overhead an inline
bump would remove.

Canary: `BinTreesClassic 18` returns checksum **68332206** (the golden value)
on the fixed binary, so nothing about the header init or the humongous routing
moved. Its wall time is not evidence in either direction — this box swings it
2 100–4 200 ms with peer load, and the interleaved pairs here
(FINAL 6 526 / 5 063 ms, BASE 5 072 / 4 908 ms) sit inside that band.

### A short code buffer retired the method permanently

The old page's sub-defect 1 (the JIT code buffer overflowing on large Hibernate
methods) **no longer reproduces** — zero occurrences of either the
`try_patch_*: offset out of bounds` warning or the `code-buffer-estimate-too-small`
bail across every run in this investigation, including the default-JIT runs that
are the stronger form of its `CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/` repro.
The 2026-08-01 widening of the per-invoke allowance (512 → 1024 bytes) covers
this workload.

The *structural* half of it was still there and is now fixed. `try_compile`
treats any backend-attempted `None` as permanent (`mark_jit_bail_listed`), so a
single wrong estimate retired the method for the life of the process — the
estimate got exactly one chance, and the backend's own comment says it "has to
be right the first time here". It does not any more: the backend records the
size it actually wanted, `try_compile` exempts that one site from the bail-list,
and the next attempt sizes its buffer from the measurement rather than the
heuristic. The site name is a shared constant (`CODE_BUFFER_TOO_SMALL_SITE`)
because the two halves must not drift apart — the failure mode if they do is a
method silently never compiling again, which is the exact defect being fixed.

The recorded shortfall is kept at its maximum and doubled on read: `wanted`
under-reports (an out-of-bounds `try_patch_*` adds nothing to it), and a later
attempt can take a shorter path through the same method, so sizing from a
smaller observation would overflow again. Unit tests in
`jit/src/lib.rs::code_buffer_retry_tests` cover the max-keeping, the doubling,
the zero and empty-key cases, and that the exemption predicate matches the
constant and not a neighbouring bail.

## The probes the page promised — now they exist

All three are committed to `apps/hib-suite-runner/` (force-added past the
blanket `apps/` ignore, like `class-overrides.tsv`). Two of them had to be built
to *refuse a vacuous answer*, because both ways of measuring this workload
silently produce one.

* **`SmokeConcProbe.java`** — the workload, tunable
  `forks iterations threads warmupForks [mode]`, with
  `txn`/`session`/`jdbc`/`parse`/`exec`/`full` decomposition. The entity shape
  matches the fixture attribute for attribute, including both enum mappings and
  the nested embeddable; a shape short of those reads ~4x cheaper per unit and
  makes the extrapolation meaningless. It inspects **every** `Future` that
  `invokeAll` returns and counts returned rows, and exits non-zero if the row
  count does not match the unit count.
* **`AllocScaleProbe.java`** — allocation throughput against thread count for
  `Object` and `long[16]`, reporting the aggregate scaling factor.
* **`PrimCostProbe.java`** — per-op cost of the JDK primitives a session
  open/close leans on, 1-thread against N-thread, so a merely-slow primitive can
  be told apart from one that fails to scale. Its `ConcurrentHashMap.get` row is
  confounded by `Integer` autoboxing and a native→Java `hashCode` re-entry and
  must not be read as a CHM cost; the allocation and lock rows are clean.

## Two hazards this investigation found, neither of them the throughput gap

### The test can report success having executed nothing

`testQueryConcurrency` submits its 400 tasks per fork through
`ExecutorService.invokeAll` and **discards the returned list**. `invokeAll`
captures a task's exception in its `Future` rather than propagating it, so a run
in which every single task threw on its first statement completes in seconds and
JUnit reports it green.

This is not hypothetical. One 50-fork run reported `ok=1 failed=0` in
**14 965 ms** — faster than a 2-fork run of the same class — alongside a
`<clinit> failed … class=org/hibernate/grammars/hql/HqlLexer cause=java/lang/NullPointerException
Cannot read field "target" because "t" is null`. A poisoned `HqlLexer` fails
every subsequent `createQuery`, and nothing in the fixture notices.

Scoring such a run as a pass is worse than scoring it as a crash: a throughput
fix would appear to have worked. `hibernate.show_sql` gives a direct witness —
one `Hibernate: ` line per statement prepared, measured at **802 per fork and
identical on HotSpot and CratonVM** — so `run-smoke-repro.sh` now scores a green
verdict with too few statements as `VACUOUS` and exits non-zero. The fixture
itself is not in git (`apps/` is ignored wholesale), which is why the guard
lives in the harness; that is the right place for it regardless.

### Roughly 1 run in 13 does not survive at all, and it is a different bug

Twenty-six runs at `forks=1`, JIT on, in two batches (10 then 16), produced two
non-completions — one of each of these shapes:

* `EXCEPTION_ACCESS_VIOLATION`, read of `0x0000000000000005`,
  `rdi=0xFFFFFFFFFFFFFFFF`, in a compiled frame under `PooledConnections.poll` /
  `DriverManagerConnectionProvider.getConnection`, with **zero** GC cycles
  having run.
* a SessionFactory that failed to build at all —
  `PropertyNotFoundException: Could not locate getter method for property 'id'`
  — preceded by `gen_heap::read_slot: corrupt Value cell (out-of-range
  discriminant)` errors whose payload is a String's byte data
  (`0x006c6d782d6d6268` is `"hbm-xml"`).

Eight `--nojit` runs of the same command were clean, 8 for 8, so the JIT is
required.

**Do not fold this into the throughput story.** It is the
`ClassId(0)` / stale-address family, tracked OPEN at
[`../../../known-issues/h2/bug-h2-classid0-stale-address-family.md`](../../../known-issues/h2/bug-h2-classid0-stale-address-family.md),
and the old page's own header says as much about its predecessor. It is
recorded here only because it sets a floor on how reproducible *any*
measurement of this class can be, and because a crash and a timeout are easy to
confuse when both present as "the class did not finish".

## Three pre-existing failures in `jit_local_exception_handler_tests`, found here, not caused here

Adding the `instanceof` regression test above meant running that file, which is
**not green on dev tip**. Baselined by stashing every change in this branch and
re-running: **13 passed, 3 failed, identically before and after**, so none of
them is this work's.

| test | symptom |
|---|---|
| `test_precise_handler_frame_catches_a_throw_at_the_end_of_its_try` | `buildMismatches` returns **20000** — every single iteration wrong, not a sampling artefact |
| `test_jit_exception_in_handler_not_recaught_by_same_handler` | `throwsInHandlerChecksum` throws instead of returning |
| `test_jit_indy_after_side_effect_no_double_execution` | `LiquibaseScopeBisect.checksum` throws instead of returning |

`liquibase-scope-corruption-atomiclong-logservice-cast-FIXED.md` records this
same file at **16/16** when it was written, so these are regressions since, not
long-term known-red. The first one in particular is a correctness failure of the
precise-handler-frame machinery on its own regression fixture. Not investigated
here — it is unrelated to this page's subject — but it needs an owner, and
anyone re-running that file to check the `instanceof` work should expect
**13 passed, 3 failed** and compare against that, not against green.

### The stale-fixture trap that hid the first attempt

The `instanceof` test failed on its first run for a reason worth recording:
these tests load the **committed** `.class` files out of
`vm/tests/resources/cratonvm/` (`test_resources_dir()` is the whole classpath
they boot with), not the copies `build.rs` stages under
`CRATONVM_TEST_CLASSES_DIR`. Editing the `.java` therefore changes nothing until
the `.class` beside it is regenerated and committed too. The new method simply
did not exist in the bytecode the test loaded, and the test failed with
`Err(ExceptionThrown(..))` rather than anything that names the cause.

That is worth knowing in its own right, because the failure mode is worse than a
missing method: a fixture that is *changed* rather than extended goes on passing
against the version it was meant to replace, silently.

Noticed on the way, and fixed: `build.rs` declared
`cargo:rerun-if-changed=tests/resources/cratonvm/` — a DIRECTORY. Cargo stats
the directory itself, and on Windows editing a file inside it does not move the
directory's own mtime, so the staged `OUT_DIR` copies went stale for the tests
that *do* read them. It now emits a per-file trigger as well; the directory line
stays, because it is what catches a file being added or removed.

Verified with `javap` rather than assumed — `instanceofStep`'s exception table
is `from 38 to 60 target 63`, the range holds `39: instanceof` and
`52: invokeinterface` (already admitted), and the handler reads local 2 through
`aload_2; getfield`. So the `instanceof` is the ONLY thing that was refusing the
method, and the handler genuinely reads a non-parameter local — the fixture
exercises the gate rather than side-stepping it.

## Measurement conditions — read before quoting any absolute number

This box is shared and was compiling for most of this investigation (peer
`rustc` load, 69% of 32 logical CPUs at one sample). Absolute numbers here drift
up to ~50%; the 153 s projection above is an upper bound taken under load, and
the earliest probe run of the day — before any build started — projected
**111 250 ms**, which is *under* the 120 s cap. Every comparison in this
document is interleaved A-B-B-A on one host state, and only those comparisons
are load-safe. Never compare a number here against one from another session.

## Ruled out — do not re-investigate

* **The query-plan cache.** The workload issues the same three HQL strings for
  every unit; if the plan cache missed, every `createQuery` would re-parse
  through ANTLR, which the profile's ANTLR-heavy leaf frames make superficially
  plausible. It does not miss: `hit=24000 miss=0` on CratonVM and the same on
  HotSpot for a matched run. `SmokeConcProbe` prints
  `@@SMOKECONC-PLANCACHE hit=… miss=…` on every run so this cannot be
  re-derived. The ANTLR frames are bootstrap (`ATNDeserializer.deserialize`,
  `verifyATN`), not steady state.
* **Logging as a driver.** CratonVM emits **zero** `jdbc.bind`/`jdbc.extract`
  TRACE lines where HotSpot emits 4 817 for the same run. The old page's ~10%
  logging lever is not available because the cost is not being paid.
* **Interpreted-time profiling.** `--stack-sample-ms` cannot see this workload:
  87% of its samples have no interpreted leaf frame, because the time is in
  compiled code, which the sampler does not reach. The tool is the wrong one
  here, not the workload the wrong shape.
* Everything the old page ruled out and that re-measurement did not contradict:
  stop-the-world pauses (none ≥ 100 ms), lock convoying as the *primary* cost,
  and `CRATONVM_JIT_VIRTUAL_TIERUP` (still not a lever).

## What is actually left

A throughput margin of a few percent either side of the 120 s cap, decided by
host load — measured directly above as 2 passes and 2 timeouts in five attempts
on a box with ~17 peer `rustc` processes running. The cost is spread across
compiled code with no outlier, which is what the old page's own subsystem
decomposition showed and is the one thing in it that survived. There is no
single hot spot to remove and no gate left to lift; the
`ineligible-by-policy=0` line is what a fully-admitted workload looks like.

The next concrete pieces of work this investigation surfaced, in descending
order of expected value:

1. Close the `reason=unrecorded` diagnostic gap (39 methods, no site recorded).
2. Teach `new` / `ldc` / `getstatic` / `athrow` to publish a precise exceptional
   frame, which would admit the remaining RBC.6 refusals.
3. Emit an inline TLAB bump for `newarray`/`anewarray` the way `new` has, which
   is the rest of the array-allocation gap the helper-side fix above narrowed.

None of them is this page, and none of them is blocked on it.
