# `HttpHeaderValidationUtilTest` — 8.6e9 iterations against a 180s wall, at ~11 calls each

**Status: OPEN, throughput. Sized 2026-08-17.** Original measurement
2026-08-16 on commit `3ef3eb744`; the sizing below is 2026-08-17 on
`cf141b8a8`, Windows host, release build, G1, real-JDK mode, against HotSpot 25
on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 5506 | 5506 | 0 | 39.9s |

Isolated and HotSpot passes the whole class (5506 sub-tests, almost all from
parameterization) in under 40 s. Not a contention artifact, not a harness gap.

## The budget

Two `@Test` methods, both annotated
`@DisabledForJreRange(max = JRE.JAVA_17) // This test is much too slow on older
Java versions`, iterate every possible 32-bit value:

```java
int i = Integer.MIN_VALUE;
do {
    buffer.putInt(0, i);
    try {
        oldHeaderValueValidationAlgorithm(asciiString);
    } catch (IllegalArgumentException ignore) {
        assertNotEquals(-1, validateValidHeaderValue(asciiString), failureMessageSupplier);
        assertNotEquals(-1, validateValidHeaderValue(charSequence), failureMessageSupplier);
    }
    i++;
} while (i != Integer.MIN_VALUE);
```

`headerValueValidationMustRejectAllValuesRejectedByOldAlgorithm` and its twin
`headerNameValidationMustRejectAllNamesRejectedByOldAlgorithm` over
`validateToken`: **4 294 967 296 iterations each, 8 589 934 592 in total**,
plus the ~5504 quick parameterized tests. HotSpot does all of it in 39.9 s, so
its whole-class rate is ~4.6 ns per exhaustive iteration.

To fit the 180 s wall CratonVM needs **~21 ns/iteration**, and each iteration
is not one operation:

* `ByteBuffer.putInt(0, i)` — a registered native on this VM, ~900 ns measured
  standalone (`probes/NioAccessorRate.java`);
* `oldHeaderValueValidationAlgorithm(asciiString)` — a 4-iteration inner loop
  calling `seq.length()` and `seq.charAt(index)` on an `AsciiString` plus a
  static state-machine step per character, i.e. ~9 more calls;
* on the ~5% of inputs containing `0x00`, `0x0b`, `0x0c` or a bad CR/LF
  sequence, a throw of a preallocated exception plus two
  `validateValidHeaderValue` calls.

So this class is the same wall as
`httpresponsestatustest-exhaustive-loop-timeout-20260816.md` with roughly twice
the iteration count and roughly 1.5x the work per iteration. It is the harder
of the two by a wide margin and should be attacked second.

## The two mechanisms it inherits

Both are measured on the sibling page and both apply here unchanged:

1. **Compile order.** A hot method that compiles before the library beneath it
   binds its call sites to the slow route and never re-binds:
   `probes/org/junit/jupiter/api/CompileOrderProbe.java` measures 382-399
   ns/iter cold against 82-84 prewarm — **4.6x**, deterministic, and 0.95x when
   the warming is done *after* the caller is compiled. A `@Test` method's loop
   crosses the OSR threshold long before JUnit's assertion chain is hot, which
   is exactly the cold arm.
2. **No inlining at the optimizing tier.** Every `tier=c2 path=optimizing`
   compile in these runs reports `inline_candidates=0` — the inliner runs only
   on the single-pass path, whose body is then replaced by the c2 one. So each
   of the ~11 calls per iteration stays a real call frame where C2 collapses the
   whole body.

Additionally, unlike the sibling, this loop *does* hit a registered native per
iteration (`ByteBuffer.putInt`), so the native-call floor sized in
`httpcontentdecompressortest-hang-20260816.md` applies to it directly:
`AtomicInteger.getAndIncrement` with a JIT intrinsic costs 5.3 ns where
`AtomicInteger.get` without one costs 160 ns, in the same process and the same
compile door.

## A timeout that did not fire is not itself evidence of a hang

`common.args` sets `-Djunit.jupiter.execution.timeout.default=120s`, which
should apply to both loops. No `TimeoutException` was printed before the
harness's own 180 s cap fired, which is consistent with JUnit Jupiter's default
`SAME_THREAD` timeout mode being unable to preempt a synchronous,
non-interruption-checking loop. (Contrast
`HttpContentDecompressorTest.testZipBomb`, where the same 120 s timeout *does*
fire — that one blocks in codec work rather than spinning.) Not read as a
separate CratonVM defect.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpHeaderValidationUtilTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

To localise which of the two exhaustive methods a run is inside, use a
per-test-progress launcher (`@@BEGIN` / `@@END` per test) — the suite harness
prints a line only for failing tests, which is why the 2026-08-16 page could
not say.

## Related

* `httpresponsestatustest-exhaustive-loop-timeout-20260816.md` — the same
  mechanism at half the iteration count; the per-iteration decomposition and
  the compile-order measurement live there.
* `httpcontentdecompressortest-hang-20260816.md` — the native-call floor this
  class's `ByteBuffer.putInt` pays once per iteration.
* `fastthreadlocal-2e9-iteration-throughput-wall-20260812.md` — the same family
  of finding, with per-component throughput measurements.
