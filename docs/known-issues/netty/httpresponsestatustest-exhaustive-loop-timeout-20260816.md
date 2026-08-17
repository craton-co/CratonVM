# `HttpResponseStatusTest` — `testHttpStatusClassValueOf`'s two full-`int`-range loops exceed the 180s budget on CratonVM

**Status: OPEN, throughput.** Measured 2026-08-16, commit `3ef3eb744`,
Windows host, `cratonvm.exe` release build, G1. Flagged HANG on generational,
G1, and ZGC in a same-day full 657-class 3-collector suite run; this page
isolates it (`--shards 1`, one class alone per process, no other collector
running concurrently) and cross-checks against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 13 | 13 | 0 | 4.6s |

Isolated and HotSpot passes cleanly (all 13 tests) in 4.6s. Not a
contention artifact, not a harness gap.

## The likely cause: one test with two full-`int`-range loops

Of the 13 tests in this class, 12 are single trivial assertions. The
remaining one, `testHttpStatusClassValueOf` (`HttpResponseStatusTest.java:116-146`),
contains two loops that between them cover almost every possible `int`:

```java
// status scope: [Integer.MIN_VALUE, 100).
for (int code = Integer.MIN_VALUE; code < 100; code ++) {
    HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
    assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
}
// status scope: [600, Integer.MAX_VALUE].
for (int code = 600; code > 0; code ++) {
    HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
    assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
}
```

The first loop runs from `Integer.MIN_VALUE` to 100 (~2.147 billion
iterations); the second starts at 600 and increments `code` until it
overflows past `Integer.MAX_VALUE` back through 0, at which point `code > 0`
becomes false (~2.147 billion iterations). Combined, roughly **4.3 billion**
calls to `HttpStatusClass.valueOf(int)` plus an `assertEquals` each — the
same "exhaustive-int-range netty test" shape as
`FastThreadLocalTest.testConstructionWithIndex` (2.1 billion iterations,
already characterized in
`fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`) and as
`HttpHeaderValidationUtilTest`'s two loops (also 2026-08-16, same batch).

HotSpot completing the *entire class* — including this loop — in 4.6s means
C2 is reducing `HttpStatusClass.valueOf` plus the loop overhead to well under
a nanosecond per iteration (likely helped by the method being small enough to
inline and largely loop-invariant after inlining). A CratonVM interpreter
cost on the order of tens of nanoseconds per iteration for the same call is
enough on its own to blow through both the 120s JUnit default and the 180s
harness cap for 4.3 billion iterations.

## The other 12 tests never get to report

Because JUnit runs all 13 tests in one fork and the harness kills the whole
process at the 180s cap before any `@@RESULT` is emitted, none of the other
12 (fast, trivial) tests in this class are known to fail — they simply never
get the chance to report, same as the pattern already noted for
`FastThreadLocalTest`.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpResponseStatusTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

## Related

* `fastthreadlocal-2e9-iteration-throughput-wall-20260812.md` — same shape of
  finding, with per-component throughput measurements.
* `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md` — the
  other `codec-http` class from the same batch with the identical shape.
