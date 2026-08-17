# `HttpContentDecompressorTest` hangs past 180s with zero test progress; HotSpot passes in 12s

**Status: OPEN.** Measured 2026-08-16, commit `3ef3eb744`, Windows host,
`cratonvm.exe` release build, G1. Flagged HANG on generational, G1, and ZGC in
a same-day full 657-class 3-collector suite run; this page isolates it
(`--shards 1`, one class alone per process, no other collector running
concurrently) and cross-checks against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 8 | 8 | 0 | 11.8s |

Isolated (no other class or collector running concurrently) and HotSpot
passes clean — this is not a full-suite contention artifact and not a harness
gap.

## No test-progress evidence at all

Between the JUnit launcher's startup log ("Discovered 4
'junit-platform.properties' configuration files...") and the process being
killed at the 180s cap, the raw log has **zero** further lines — no
`@@TESTFAIL`, no application log output, nothing. The harness only prints a
line per test on *failure*; a passing test is silent. That means this run
gives no way to tell whether it hung on the class's first test or its last —
only that it never reached `@@RESULT`.

## What the class contains

Four test methods (`HttpContentDecompressorTest.java`):

* `testInvokeReadWhenNotProduceMessage` — small `EmbeddedChannel` pipeline
  test, no real I/O.
* `testFlowControlHandlerEmitsOneMessagePerRead` — same shape.
* `testZipBomb(String encoding)` — parameterized over up to 5 encodings
  (`gzip`, `deflate`, `br` if brotli4j is available, `zstd` if available,
  `snappy`); per parameterization it compresses 256 `MiB` (256 × 1 `MiB`
  chunks) through `HttpContentCompressor`, then decompresses it through
  `HttpContentDecompressor(0)` while a `ZipBombIncomingHandler` enforces a
  128 `MiB` memory cap.
* `testBrotliDecodingHonorsMaxAllocationAsOutputCap` — requires
  `Brotli.isAvailable()`; compresses a 128 KiB payload and decompresses it
  with a deliberately tiny `maxAllocation` to check the decoder emits many
  small chunks rather than a few large ones.

Unlike the other two `codec-http` classes flagged in the same batch
(`HttpHeaderValidationUtilTest`, `HttpResponseStatusTest`), nothing here is a
literal billion-iteration loop — the `testZipBomb` parameterizations move
real megabytes through real gzip/deflate/brotli/zstd/snappy codec paths, and
the memory-cap enforcement itself is exactly the kind of logic that could
spin or block if the cap check doesn't trip correctly. This class's shape
does not obviously predict "throughput cliff" the way the other two do — it
is presented here as an observed hang, not a diagnosed one.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpContentDecompressorTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

To pin down which method/parameterization is stuck, the next step is running
each of the four tests individually (JUnit `-Dtest=` style selection via a
small dedicated runner, or a short per-method `-Djunit.jupiter.execution.timeout.default`)
rather than the whole class at once.

## Related

* `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`,
  `httpresponsestatustest-exhaustive-loop-timeout-20260816.md` — the other two
  `codec-http` HANGs from the same batch; both of those *do* have an obvious
  billion-iteration-loop explanation, which this one lacks.
