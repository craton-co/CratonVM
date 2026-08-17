# `HttpResponseStatusTest` — `testHttpStatusClassValueOf` needs 42 ns/iteration and gets 285

**Status: OPEN, throughput. Sized and partly diagnosed 2026-08-17.** Original
measurement 2026-08-16 on commit `3ef3eb744`; the per-iteration decomposition
below is 2026-08-17 on `cf141b8a8`, Windows host, release build, G1, real-JDK
mode, against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 13 | 13 | 0 | 4.6s |

Isolated and HotSpot passes cleanly. Not a contention artifact, not a harness
gap. Per-test progress instrumentation confirms the process is inside
`testHttpStatusClassValueOf` when the cap fires, and nowhere else.

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

4 294 967 296 iterations. To fit the harness's 180 s per-class wall the loop
body must cost **42 ns/iteration**.

`probes/HttpStatusClassLoopRate.java` is the test method verbatim with the two
exhaustive loops bounded by `n`, called exactly ONCE so the loop is reachable
only through OSR — the shape a `@Test` method has:

| n | CratonVM ns/iter | extrapolated full run |
|---|---|---|
| 10 M | 307.67 | 1321 s |
| 20 M | 282.68 | 1214 s |

HotSpot on the same probe at n=100 M: **6.09 ns/iter**, i.e. ~26 s extrapolated
(and 1.8 s for the real unbounded method, where C2 has the whole range
statically). So CratonVM is **~285 ns/iter and needs 42** — a **6.8x** gap, not
the "tens of nanoseconds" the 2026-08-16 page estimated.

## What the 285 ns is NOT

Ruled out by measurement, each in one run:

* **OSR refusal.** `osr_entered` is non-zero and `osr_refused_entry=0`,
  `osr_compile_declined=0`, `deopts=0` for this loop. The loop compiles. (This
  is worth stating because `osr-refused-for-a-loop-inline-in-main-20260810.md`
  describes exactly this shape and does *not* apply here.)
* **`getstatic` of another class's field.** Hoisting
  `HttpStatusClass.UNKNOWN` out of the loop, or reading it from the probe's own
  `static final`, moves 288 -> 276 -> 266 ns/iter. `getstatic` alone measures
  3.67 ns/iter.
* **The call cost of a compiled call.** `probes`-measured compiled-to-compiled
  calls are cheap: 4.1 ns for a one-int-arg static, 9.1 ns for two, 5.5 ns
  virtual, 5.8 ns interface, 13.3 ns for `(Object,Object)->boolean`. Seven such
  calls would be ~40 ns, not 285.
* **A slow native under `assertEquals`.** With the real operands (an enum
  constant) the chain does not reach a registered native:
  `AssertionUtils.objectsAreEqual` dispatches to `java.lang.Enum.equals`, which
  is bytecode and measures 37 ns. (A *plain* `Object` receiver would reach the
  `java/lang/Object.equals` native at 190 ns — but that is not this test.)
* **`valueOf` itself.** 18.4 ns/iter including `HttpStatusClass$1.contains`
  (14.1 ns).

## What it IS, as far as it has been narrowed: compile ORDER

`probes/org/junit/jupiter/api/CompileOrderProbe.java` runs the same hot loop
with one switch — whether the JUnit callees (`Assertions.assertEquals`,
`AssertEquals.assertEquals`, `AssertionUtils.objectsAreEqual`) are exercised
from a *different* method before the hot method is ever compiled:

| arm | ns/iter | extrapolated |
|---|---|---|
| cold (hot method compiles first) | 382, 399 | ~1650 s |
| prewarm (callees compile first) | 82, 84 | ~355 s |

**4.6x, deterministic across interleaved repeats.** And it is permanent, not
transient: in a single process, timing the loop cold, then warming the callees,
then timing again, gives 437 -> 460 ns/iter — ratio **0.95x**. Once the caller
is compiled it never re-binds.

This matters here because it is exactly the shape a JUnit test has: the `@Test`
method's loop crosses the OSR threshold within its first few hundred
iterations, long before the assertion framework beneath it is hot.

**The mechanism is still open**, and three plausible explanations are already
excluded:

* the compile *records* are identical between the arms — same methods, same
  requested tiers, same backends, same outcomes;
* `CRATONVM_DBG_OSR_BIND=1` (added with this page) reports both of the hot
  method's own call sites bound as **direct** machine-code calls in *both* arms,
  with `requires_dispatch=false declares_handlers=false indy_trap=false`;
* the callee's emitted optimizing-tier body is the same 703 bytes in both arms,
  differing only in three baked rel32 addresses and two single bytes.

## Even with that 4.6x recovered, the class does not fit

82 ns/iter x 4.295e9 is **352 s**, still ~2x over the 180 s wall. The second
factor is the one `httpcontentdecompressortest-hang-20260816.md` sizes: the
per-call floor for anything that reaches a registered native, and the absence
of inlining at the optimizing tier — every `tier=c2 path=optimizing` compile in
these runs reports `inline_candidates=0`, so the JUnit assertion chain is four
real call frames per iteration where C2 collapses it to a compare and a branch.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpResponseStatusTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

```bash
cratonvm --java-home <jdk> @common.args HttpStatusClassLoopRate 20000000
cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 3000000 40 cold
cratonvm --java-home <jdk> @common.args org.junit.jupiter.api.CompileOrderProbe 3000000 40 prewarm
```

## Related

* `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md` — same
  mechanism, larger budget (8.6e9 iterations against the same 180 s).
* `httpcontentdecompressortest-hang-20260816.md` — the third `codec-http` wall
  from the same batch; native-call cost rather than compiled-call cost, with the
  intrinsic-vs-native sizing (5.3 ns against 160 ns for the same method).
* `fastthreadlocal-2e9-iteration-throughput-wall-20260812.md` — same family of
  finding, with per-component throughput measurements.
* `osr-refused-for-a-loop-inline-in-main-20260810.md` — the shape this looks
  like and is not; OSR is entered here.
