# `HttpResponseStatusTest` — the class does NOT pass, and the target is 120 s, not 180

**Status: OPEN, throughput.** This page's headline has to be withdrawn. The
previous revision said *"The class passes … it runs **13 of 13 tests, all `ok`,
in 193.8 s** … The only thing standing between this class and a green suite row
is the harness's own 180 s cap, and it is **7% away**."* Neither half is the
current state, and the second half was never the whole story.

Measured 2026-08-25, **Azure Linux host `vm1`, idle-to-light (load 4–16)**,
release build, real-JDK mode, one binary, six interleaved (ABBA) runs of the
whole class. **Every single run — 13 of 13 across this page's measurements and
two earlier batteries — reports `found=13 started=13 ok=12 failed=1`**, and the
failure is always the same:

```
@@TESTFAIL io.netty.handler.codec.http.HttpResponseStatusTest testHttpStatusClassValueOf() FAILED
java.util.concurrent.TimeoutException: testHttpStatusClassValueOf() timed out after 120 seconds
	at org.junit.jupiter.engine.extension.TimeoutExceptionFactory.create(TimeoutExceptionFactory.java:29)
	at org.junit.jupiter.engine.extension.SameThreadTimeoutInvocation.proceed(SameThreadTimeoutInvocation.java:62)
```

## The `@Timeout` claim was wrong, and it moved the goalposts

This page (and its sibling) recorded: *"The `@Timeout` does not fire at all —
Jupiter's default `SAME_THREAD` mode cannot preempt a synchronous
non-interruption-checking loop — so the method runs to completion and passes."*

Half of that is true and the conclusion does not follow.
`SameThreadTimeoutInvocation` does not **preempt** — it runs the method to
completion on the calling thread — but it then **checks the elapsed time and
throws**. So the method does run to the end, and it is still reported FAILED.
An earlier revision of this page recorded exactly that (`13 started, 12 ok`) and
a later one decided it was stale. It was not.

**The consequence is that the target is `-Djunit.jupiter.execution.timeout.default=120s`,
not the harness's 180 s wall.** `testHttpStatusClassValueOf` is essentially the
whole class — the other twelve tests are HotSpot-fast — so a green row needs
that ONE method under 120 s. It currently takes the bulk of a 192–375 s class
wall. Raising the harness cap in `class-overrides.tsv` would change `HANG` to
`FAIL`; it would not make the row green.

## Where the class stands

| | found | started | ok | failed | wall |
|---|---:|---:|---:|---:|---:|
| HotSpot 25 (load 10.4) | 13 | 13 | **13** | 0 | **5 s** (`ms=3415`) |
| CratonVM, default, 6 runs (load 4–16) | 13 | 13 | 12 | **1** | 192, 307, 310, 313, 315, 352 s |
| CratonVM, `CRATONVM_JIT_RECEIVER_DESPEC=0`, 6 runs | 13 | 13 | 12 | **1** | 225, 265, 303, 346, 349, 375 s |
| the JUnit `@Timeout` for the one method that matters | | | | | **120 s** |
| the harness wall | | | | | 180 s |

The 2026-08-25 receiver-guard fix
(`fixed-suite-bugs/jit/string-receiver-guard-speculated-with-no-evidence-FIXED-20260825.md`),
which is worth 6.6x on the sibling class, is **worth nothing here**: medians
324 s → 311 s, with the two arms' ranges overlapping. That is the expected
answer — this loop contains no `CharSequence`-declared String accessor — and it
is recorded so nobody re-runs it. In the 93-class `codec-http` regression run
this class is `HANG` in both arms, i.e. killed at the 180 s wall, identically.

## The finding that still stands: an OSR artifact splices nothing, ever

`compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`) hands
`x64::compile_with_param_slots` an **empty `inline_sites` map**. The inline
planner runs on the method-entry door only.

A `@Test` method is invoked exactly once, so an OSR artifact is the only
compiled form it will ever have, and **no call site in
`testHttpStatusClassValueOf` is ever a splice candidate** — not
`HttpStatusClass.valueOf`, not `Assertions.assertEquals`. Re-confirmed
2026-08-25 on `probes/HttpStatusClassLoopRate.java` with
`CRATONVM_DBG_JITC=1`: the trace carries **no `inline-plan` line naming
`HttpStatusClass.valueOf` at all**, and the only planned sites in the whole run
are inside `Assertions.assertEquals` one level down —

```
inline-plan pc=6 org/junit/jupiter/api/AssertEquals.assertEquals(...)V: DirectBind (cost=Some(43) budget_left=750)
inline-plan pc=6 org/junit/jupiter/api/AssertEquals.assertEquals(...)V: Refuse(CalleeTooLarge) (cost=None budget_left=750)
inline-planned org/junit/jupiter/api/AssertEquals.assertEquals(...)V @pc=6
```

That probe reads **104.38 ns/iter** at load 3.61, which extrapolates the full
2^32-iteration range to **448 s** — the same order as the measured class wall,
so the probe is usable here as more than a ratio.

### Wiring the planner into the OSR door is NOT the lever — measured

`probes/OsrVsEntryInlineProbe.java` runs one loop body through both doors in the
same process: `osrOnce` is called once, `entryMany` runs the identical body in
65536-iteration chunks so it crosses the invocation threshold and gets a
method-entry artifact — the door that DOES plan inline sites.

| round | `osrOnce` | `entryMany` |
|---|---:|---:|
| 1 | 47.16 | 47.43 |
| 2 | 49.76 | 53.99 |
| 3 | 46.44 | 47.23 |

No difference, and `CRATONVM_JIT_MAIN_INLINE=1` moves neither arm. There is no
17% hiding behind the OSR door.

*(The first cut of that probe accumulated `c.ordinal()` and read 143–146 ns/iter
with every arm inside every other arm's noise. `Enum.ordinal` is a registered
native on the ~160 ns funnel — the probe was measuring the one thing it was not
asking about. That is the third instrument on this family of pages to have
manufactured a result; the others are `DecomposeProbe`'s 43 ns baseline and
`HeaderValidationLoopRate`'s throw rate.)*

### `outer-splice-rolled-back=1` is not this loop either — measured

`try_emit_inline_site` names its rollbacks under `CRATONVM_DBG_JITC`, and on
this loop there is exactly one, every run:

```
[cratonvm-jitc] inline-rollback java/lang/StringUTF16.newBytesFor(I)[B at pc=206: callee_pc=4 op=0xbc
```

`0xbc` is `newarray`, and `StringUTF16.newBytesFor` is on the string-building
path the JUnit failure-message supplier reaches — not on this loop's hot path.

### `valueOf` is one virtual call, not five

In the checked-out tree (`4.2.18.Final-SNAPSHOT`):

```java
public static HttpStatusClass valueOf(int code) {
    if (UNKNOWN.contains(code)) {
        return UNKNOWN;
    }
    return statusArray[fast_div100(code)];
}
```

One `invokevirtual` on `HttpStatusClass$1` (the `UNKNOWN` constant's anonymous
subclass, whose body is `code < 100 || code >= 600`), plus a `fast_div100` that
is already spliced. For the two exhaustive loops `UNKNOWN.contains` is always
true, so the array read never runs. Any estimate that starts from "five calls"
is starting from the wrong method.

## What is fixed, and what it was worth

Kept for the record, because each of these was this page's headline at some
point:

1. **Compile ORDER** (2026-08-17). `try_jit_compile_callee_slow`'s callee
   resolver was lookup-only, so eager callee compilation was exactly one level
   deep. The resolver compiles transitively now. Proved by the counter, not the
   clock: `disp_calls` **2 003 538 → 3 926** on a fixed 1e6-iteration arm.
2. **The five-step inline chain** (2026-08-18) — multi-frame deopt resume, a
   multi-frame OSR-exit transfer with admission relaxed to match, the
   single-pass scope stack, a real call inside a spliced body, and nesting.
3. **Direct-binding a spliced call** (2026-08-18): 6 of 6 interleaved rounds,
   ~45.1 → ~39.1 ns/iter on `AssertChainProbe`, `disp_calls` back to ~3 870.
4. **Operand-stack merging in the inline emitter, and the devirtualisation it
   unblocked** (2026-08-18): 44.6 → 33.8 ns/iter, monotone over five rounds.
5. **Shipping any of it** (2026-08-20). Steps 2–4 all landed behind default-OFF
   flags and nothing turned them on. `CRATONVM_JIT_INLINE_CALLS`,
   `CRATONVM_JIT_INLINE_NEST` and `CRATONVM_JIT_INLINE_SPLICE_DEVIRT` are
   default-ON now, each with a `=0` opt-out. `CRATONVM_JIT_MAIN_INLINE` is NOT
   part of that set: it moves nothing and it is the one flag with a documented
   open miscompile against it.

Correctness for that flip, one binary, flags off and on: the whole netty
`codec-http` suite — identical result sets. Plus `regression-suite/run.sh`,
identical in three arms.

## What is left

**One method, and it needs roughly 1.6x.** `testHttpStatusClassValueOf` runs
4 294 967 296 iterations of `valueOf(code)` + `assertEquals(UNKNOWN, …)` and must
come in under 120 s — about **28 ns/iteration** — against the ~45 ns this loop
costs today. Three candidate levers are ruled out above. What has NOT been
priced:

* **The call frames themselves.** `probes/CallArgCostProbe.java` prices a
  compiled static call at **4.13 ns**, one taking a reference at **6.46**, a
  virtual one at **8.19–8.96**, against HotSpot's ~0. The loop body is `valueOf`
  (one static frame containing one virtual call) plus `assertEquals` (one static
  frame with the rest of the chain collapsed inside it) plus the `getstatic` —
  roughly 20–25 ns of pure call overhead in a 45 ns iteration. Removing it means
  splicing at the top level, which the OSR door cannot do and which the
  measurement above says would not pay even if it could. This is the same wall
  as [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md).

  **Narrowed 2026-08-26, and measured NOT to help this class.** The per-call
  14-store full-GPR blind spill is now elided where the caller frame is provably
  oop-clean, worth 1.4–2.2x on every shape of compiled call in
  `CallArgCostProbe`. `HttpStatusClassLoopRate` does not move (106.5–121.2 vs
  105.8–112.1 ns/iter, three interleaved rounds each), and the counter says why:
  `elided=1 ... ref-local-in-reg=46`, every refusal the same clause. See
  `performance/per-call-blind-gpr-spill-elided-on-oop-clean-frames-20260826.md`.
* **`MAX_INLINE_MERGE_DEPTH` (4) and `MAX_INLINE_NEST_DEPTH` (3)**, neither of
  which has been tuned against anything. Cheap to sweep, and the arm census
  (`nested-splice-refused`, `outer-splice-rolled-back`) says immediately whether
  either binds — on this loop the only rollback is the `newBytesFor` one above,
  so as of 2026-08-25 neither does.
* **The harness cap.** `class-overrides.tsv` exists for exactly this, and
  `DnsNameResolverTest` carries a 600 s entry because a class killed at the cap
  is recorded `HANG`, indistinguishable from a real deadlock. Raising this
  class's cap would make the suite row **honest** (`FAIL`, with a named
  `@Timeout`) rather than green, which is a smaller prize than the previous
  revision of this page believed. That is a harness decision, not a VM one, and
  it is deliberately not taken here.

## Repro

```bash
cd apps/netty-suite-runner
timeout 900 java @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.codec.http.HttpResponseStatusTest
```

```bash
timeout 900 cratonvm --java-home <jdk> -Xmx1500m @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.codec.http.HttpResponseStatusTest
```

The failure only appears on stderr — a run whose stderr is discarded reports
`ok=12 failed=1` with no reason:

```bash
timeout 900 cratonvm --java-home <jdk> -Xmx1500m @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.codec.http.HttpResponseStatusTest 2>&1 | grep -A3 '@@TESTFAIL'
```

The probes, and the trace that answers "which method got the splice":

```bash
cratonvm --java-home <jdk> @common.args HttpStatusClassLoopRate 2000000
```

```bash
CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk> @common.args HttpStatusClassLoopRate 2000000 2>&1 | grep -E 'inline-plan|inline-rollback'
```

## Two notes on this page's own probes, for the next reader

`DecomposeProbe` is **no longer in the tree** as of 2026-08-25; its numbers
survive only in this page's history, and they should not be quoted as
absolutes. Its `empty` arm measured **43 ns/iter** for `sink += c` on a
`static long`, so every row it printed carried a ~43 ns baseline that had
nothing to do with the rung it named, and its `valueOf` row additionally
included an `Enum.ordinal()` call, which is a registered native. Read
`probes/HttpStatusClassLoopRate.java`, `probes/StatusLoopArmsProbe.java` and
`probes/AssertChainProbe.java` for this loop's cost.

**`StatusLoopArmsProbe`'s `refcheck` arm was measuring the interpreter.** It
wrote `throw new IllegalStateException()` inline, which puts an `athrow` in the
method, and `RBC.6` (`has_athrow`) refused OSR for any method that `athrow`s —
so that one arm ran interpreted while its four siblings compiled. It read
**825.91 ns/iter against `full`'s 80.57**: the SUBSET arm ten times slower than
the superset it is a subset of, which is arithmetically impossible and is the
tell. Worked around 2026-08-17; the underlying refusal was narrowed 2026-08-20.
Any arm added here must still be checked against `CRATONVM_DBG_JITC=1` for
`OSR-compile FAILED` before its number is believed.

## Related

* [`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md)
  — the sibling. Its own deopt defect closed 2026-08-25; its remainder is the
  same per-iteration wall as this page's.
* `fixed-suite-bugs/jit/string-receiver-guard-speculated-with-no-evidence-FIXED-20260825.md`
  — the 2026-08-25 fix, and the measurement that it is worth nothing on THIS
  class.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — same family of finding, with per-component throughput measurements.
* `fixed-bugs/osr-refused-for-a-loop-inline-in-main-FIXED-20260818.md`
  — the shape this looks like and is not; OSR is entered here.
