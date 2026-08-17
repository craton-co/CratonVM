# `HttpHeaderValidationUtilTest` — two full-32-bit-int-range loops exceed the 180s budget on CratonVM

**Status: OPEN, throughput.** Measured 2026-08-16, commit `3ef3eb744`, Windows
host, `cratonvm.exe` release build, G1. Flagged HANG on generational, G1, and
ZGC in a same-day full 657-class 3-collector suite run; this page isolates it
(`--shards 1`, one class alone per process, no other collector running
concurrently) and cross-checks against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 5506 | 5506 | 0 | 39.9s |

Isolated and HotSpot passes the whole class (5506 sub-tests, almost all from
parameterization) cleanly in under 40s. Not a contention artifact, not a
harness gap.

## The likely cause: two exhaustive full-`int`-range loops

`HttpHeaderValidationUtilTest.java` has two `@Test` methods, both marked
`@DisabledForJreRange(max = JRE.JAVA_17) // This test is much too slow on
older Java versions`, that iterate every possible 32-bit value:

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

(`headerValueValidationMustRejectAllValuesRejectedByOldAlgorithm`, and its
twin `headerNameValidationMustRejectAllNamesRejectedByOldAlgorithm` over
`validateToken`) — each is **4 294 967 296 iterations**, cross-checking the
new validation routine against netty's old byte-by-byte state machine over
every possible 4-byte buffer content. The class's own annotation already
flags these as too slow for older JDKs; netty's authors clearly expect them
to be tight, JIT-friendly loops on a modern JIT.

This is the same shape already characterized for
`FastThreadLocalTest.testConstructionWithIndex` (a 2.1-billion-iteration
loop) in `fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`: a
netty test that is legitimately slow even on HotSpot's interpreter, made to
fit HotSpot's per-class wall only because C2 gets it down to a low
per-iteration cost that CratonVM's current JIT does not yet match.

## No test-progress evidence to localize further

The raw log has no lines at all between JUnit's launcher startup and the
180s kill — no `@@TESTFAIL`, nothing. The harness only logs failing tests, so
this does not by itself prove *which* of the two exhaustive loops (or the
other ~5504 quick parameterized tests) is where the process is stuck — only
that whichever one runs first among the slow methods is not finishing.

## A timeout that did not fire is not itself evidence of a hang

`common.args` sets `-Djunit.jupiter.execution.timeout.default=120s`, which
should apply to any `@Test` without its own explicit `@Timeout` — including
both loops above. No `TimeoutException` was printed before the harness's own
180s process cap fired. This is consistent with JUnit Jupiter's default
`SAME_THREAD` timeout mode, which cannot preempt a synchronous, non-blocking,
non-interruption-checking loop — the same would be true on HotSpot if
HotSpot were slow enough to hit it (it isn't, here, at 39.9s for the whole
class). It is not read as a separate CratonVM-specific defect on its own.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpHeaderValidationUtilTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

## Related

* `fastthreadlocal-2e9-iteration-throughput-wall-20260812.md` — the same
  shape of finding (a netty test with a multi-billion-iteration loop that
  fits HotSpot's per-class wall only because of C2, not because the loop is
  intrinsically short), with per-component throughput measurements that
  would be the starting point for sizing this class's gap too.
* `httpresponsestatustest-exhaustive-loop-timeout-20260816.md` — the other
  `codec-http` class from the same batch with the identical shape (a single
  test with ~4.3 billion total loop iterations).
