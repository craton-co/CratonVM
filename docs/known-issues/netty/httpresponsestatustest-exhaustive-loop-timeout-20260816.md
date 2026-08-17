# `HttpResponseStatusTest` — `testHttpStatusClassValueOf` needs 42 ns/iteration and gets 120

**Status: OPEN, throughput. The compile-ORDER mechanism this page was about is
FIXED (2026-08-17); the remaining gap is ~2.9x and has a different cause.**
Original measurement 2026-08-16 on `3ef3eb744`; per-iteration decomposition
2026-08-17 on `cf141b8a8`; the fix and the numbers below 2026-08-17 on
`perf/netty-exhaustive-loop-walls-20260817`. Windows host, release build, G1,
real-JDK mode, against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated), 2026-08-16 | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 13 | 13 | 0 | 4.6s |

Per-test progress instrumentation confirms the process is inside
`testHttpStatusClassValueOf` when the cap fires, and nowhere else. On HotSpot that
single method is **2.607 s** (`ProgressRunner`, `@@RESULT ... ms=2607`), so the
180 s per-class wall leaves CratonVM an allowance of **~69x HotSpot** on this
class. Contrast the sibling
[`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md),
where the same wall allows only ~2.1x: the two pages are not one problem at two
scales, and treating them as one mis-sized both.

## The budget, exactly

`testHttpStatusClassValueOf` (`HttpResponseStatusTest.java:116-146`) runs three
loops; the two exhaustive ones cover almost every `int`:

```java
for (int code = Integer.MIN_VALUE; code < 100; code ++) {   // ~2.147e9
    HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
    assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
}
for (int code = 600; code > 0; code ++) {                   // ~2.147e9
    HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
    assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
}
```

4 294 967 296 iterations. To fit the 180 s wall the loop body must cost
**42 ns/iteration**.

`probes/HttpStatusClassLoopRate.java` is the test method verbatim with the two
exhaustive loops bounded by `n`, called exactly ONCE so the loop is reachable
only through OSR — the shape a `@Test` method has:

| | CratonVM ns/iter | extrapolated full run |
|---|---:|---:|
| 2026-08-16 / 08-17, as first measured | 283-308 | 1214-1321 s |
| this branch's binary, `-eager-callee-chain` (the control) | **413.6** | 1776 s |
| **this branch's binary, default** | **120.3** | **517 s** |
| budget | 42 | 180 |

So the compile-order mechanism is worth **3.4x** on the real loop, and the class
is still **2.9x** over.

Both rows are one interleaved pair on the shipping binary. Take the ratio and not
the absolute: this host was running four other release builds throughout, and the
same pair measured earlier in the session gave 477.6 against 109.4 (4.4x). The
number that does not move with load is the dispatch counter below.

## FIXED: compile ORDER — a body compiled before its callee never re-binds

`probes/org/junit/jupiter/api/CompileOrderProbe.java` runs the same hot loop with
one switch: whether the JUnit callees are exercised from a *different* method
before the hot method is ever compiled.

| arm | `-eager-callee-chain` | default |
|---|---:|---:|
| cold (hot method compiles first) | 249 / 233 ns/iter | **57 / 61** |
| prewarm (callees compile first) | 78-95 ns/iter (unchanged; nothing to fix) | |

Interleaved, two rounds, one binary. The cold arm is now *faster* than the
prewarm arm used to be, which is the point: after the fix there is no cold arm.

**The mechanism, and the measurement that named it.** `CRATONVM_DBG_MIC_PROF=1`
reports the generic dispatch helper's call count, and in the cold arm it was
almost exactly one per iteration:

| arm | `disp_calls` (2e6 iterations) | `cyc_disp_total` |
|---|---:|---:|
| cold | **2 003 478** | 897 989 034 (≈448 cycles each) |
| prewarm | 4 028 | 1 237 484 |

`CRATONVM_DBG_MIC_TRACE=1` names the one site:
`org/junit/jupiter/api/AssertEquals.assertEquals(Object,Object,String)`, reached
from the two-argument overload — which the cold arm had compiled *first*.

`try_jit_compile_callee_slow`'s callee resolver (`direct_callee_lookup`) was
**lookup-only**: it bound an already-compiled callee to a raw `CALL` and
otherwise answered `callee-not-yet-compiled`, leaving the site on the generic
`jit_invoke_dispatch` round trip **for the life of the compiled body**. So eager
callee compilation was exactly ONE level deep — the mutator door compiled a
direct callee, but that callee was itself compiled through this function, whose
own statically bound sites then fell back to the helper. Whether a chain ran at
~450 ns/iteration or ~80 depended, permanently, on the order the tiered manager
happened to reach the methods in.

The resolver now compiles the callee transitively (depth ≤ 6, 96 compiles per
top-level compile, cycle-guarded through
`cratonvm_jit::jit_active_compile_contains`), so the bind is order-independent.
`CRATONVM_JIT='-eager-callee-chain'` is the same-binary A/B control.

**Proved by the counter, not the clock.** Same binary, cold arm, 1e6 iterations:
`disp_calls` **2 003 538 → 3 926**. That matters on this host, whose run-to-run
spread on a fixed configuration reaches 3x, and every absolute number on this
page moved by 1.5-2x between rounds while the counter did not move at all.

Three explanations this page carried are therefore superseded. The compile records
being identical between the arms, both of the hot method's call sites binding
`direct`, and the caller and callee bodies being byte-identical apart from baked
addresses were all *true* and all irrelevant: the differing bind was one level
DEEPER than either body, in a method neither dump covered. The MIC/PIC
"per-site runtime state" hypothesis the page ended on is not the answer either.

## What the remaining 120 ns is

`probes/CallCostProbe.java`, same binary, after the fix:

| | CratonVM |
|---|---:|
| no call | 0.66 ns/iter |
| 1 static call | 0.84 |
| 2 static calls | 1.86 |
| 1 virtual call | 6.20 |
| 1 interface call | 5.74 |

Compiled-to-compiled calls are no longer the story; **the number of them is**.
One iteration is ~10 real call frames:

* `HttpStatusClass.valueOf(int)` is five `invokevirtual contains(int)` calls, one
  per enum constant, each a distinct anonymous subclass (`HttpStatusClass$1..$5`)
  — five monomorphic sites at ~6 ns, so ~30 ns;
* `Assertions.assertEquals` → `AssertEquals.assertEquals(Object,Object)` →
  `(Object,Object,String)` → `AssertionUtils.objectsAreEqual` → `Enum.equals`,
  four more frames;
* plus the `getstatic HttpStatusClass.UNKNOWN` the assert needs (3.67 ns).

HotSpot collapses all of it: `contains` inlines to a pair of compares and
`assertEquals` to one reference comparison and a branch.

**Neither compile door can do that, and the reason is not a missing flag.** The
single-pass emitter's `try_emit_inline_body` bails on any callee invoke that is
not a resolver-proven elidable super-`<init>` — it splices LEAF bodies only. Both
`valueOf` (five invokes) and every rung of the assertion chain (one invoke each)
are therefore ineligible at every door. Measured, same binary, on the real-loop
probe: `CRATONVM_JIT_MAIN_INLINE=1` 96.8 ns/iter,
`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=1` 94.9, both 108.9, neither 92.8 — all
inside each other's noise. Turning inlining knobs on cannot help while the
inliner cannot nest.

That also restates the `inline_candidates=0` observation precisely. It is not
that the optimizing tier declines to inline: the inline planner runs on the
single-pass path only, and the ONE emitter both tiers share cannot represent a
non-leaf inline, so there is nothing for the planner to admit here either way.

## What is left

Closing 120 → 42 ns/iteration on this shape needs an inliner that can splice a
callee containing calls — one that nests. Two smaller items are worth doing
first, because each is measurable on its own and generalises well beyond this
class:

* `Enum.ordinal()` and `Object.equals` are registered natives on the ~160 ns
  funnel (222 ns and 190 ns per
  [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)),
  so any assertion chain that reaches one pays it. `Enum.equals` is bytecode and
  measures 37 ns, already six times a compiled virtual call.
* `probes/StatusLoopArmsProbe.java` decomposes the loop by replacing one rung at
  a time (bare / getstatic / valueOf / plain reference compare / full assert), so
  the split between `valueOf` and the JUnit chain becomes a measurement rather
  than an estimate. Use it before touching either.

## A note on this page's own probe, for the next reader

`probes/DecomposeProbe.java`'s `empty` arm measures **43 ns/iter** for
`sink += c` on a `static long` — so every row of that probe carries a ~43 ns
baseline that has nothing to do with the rung it names, and its `valueOf` row
(312 ns) additionally includes an `Enum.ordinal()` call, which is a registered
native. Read `HttpStatusClassLoopRate` and `StatusLoopArmsProbe` for this loop's
cost; `DecomposeProbe`'s rows are only comparable to each other.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpResponseStatusTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

```bash
cratonvm --java-home <jdk> @common.args HttpStatusClassLoopRate 10000000
cratonvm --java-home <jdk> @common.args StatusLoopArmsProbe 10000000
cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 3000000 40 cold
cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 3000000 40 prewarm
# the A/B control for the fix, and the counter that proves it
CRATONVM_JIT_EAGER_CALLEE_CHAIN=0 CRATONVM_DBG_MIC_PROF=1 \
  cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 1000000 40 cold
```

The per-method HotSpot wall — the number the 42 ns budget should be compared
against — comes from the progress launcher, not the suite harness, which prints a
line only for failing tests:

```bash
java @common.args ProgressRunner \
  'io.netty.handler.codec.http.HttpResponseStatusTest#testHttpStatusClassValueOf'
```

## Related

* [`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`](httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md)
  — sized as "the same mechanism at twice the iteration count". It is not the same
  mechanism: those loops never compile at all. That page carries the correction.
* [`../jit/osr-refuses-any-method-with-an-exception-table-20260817.md`](../jit/osr-refuses-any-method-with-an-exception-table-20260817.md)
  — the defect the sibling page turned out to be. This loop has no `try`, which is
  why it is compiled and merely slow.
* [`httpcontentdecompressortest-hang-20260816.md`](httpcontentdecompressortest-hang-20260816.md)
  — the per-call floor for anything reaching a registered native, which is what
  prices `Enum.ordinal`/`Object.equals` above.
* [`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`](fastthreadlocal-2e9-iteration-throughput-wall-20260812.md)
  — same family of finding, with per-component throughput measurements.
* [`../jit/osr-refused-for-a-loop-inline-in-main-20260810.md`](../jit/osr-refused-for-a-loop-inline-in-main-20260810.md)
  — the shape this looks like and is not; OSR is entered here
  (`osr_entered` non-zero, `osr_refused_entry=0`, `deopts=0`).
