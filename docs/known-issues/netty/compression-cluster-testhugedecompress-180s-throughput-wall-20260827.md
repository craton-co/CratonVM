# 11 `codec.compression.*IntegrationTest` classes HANG at the 180s cap — one shared, already-diagnosed throughput wall, partially fixed

## Status
**Known throughput characteristic, partially fixed, expected to still HANG at
the harness's 180s cap.** Pulled back from
`fixed-suite-bugs/netty/compression-testhugedecompress-shared-timeout-20260816.md`
(full investigation and fix history) — this page is the public-facing summary.
Reproduces every run this session, including the 2026-08-27 quiet single-shard
ZGC rerun.

## The 11 classes

`JdkZlibIntegrationTest`, `SnappyIntegrationTest`, `SnappyJumboSizeIntegrationTest`,
`JZlibIntegrationTest`, `BrotliIntegrationTest`, `Bzip2IntegrationTest`,
`FastLzIntegrationTest`, `LengthAwareLzfIntegrationTest`, `LzfIntegrationTest`,
`Lz4FrameIntegrationTest`, `ZstdIntegrationTest` — all `process-died rc=124
timeout=180s`.

## Root cause

All eleven inherit one shared base-class test method, `testHugeDecompress`,
which builds a **256 MiB** buffer **one byte at a time** before compression
even starts:

```java
int chunkSize = 1024 * 1024, numberOfChunks = 256;
for (int i = 0; i <= numberOfChunks; i++) {
    ByteBuf in = compressChannel.alloc().buffer(chunkSize);
    for (int j = 0; j < chunkSize; j++) {
        in.writeByte(...);       // <- 268,435,456 times
        digest.update(byteValue); // <- 268,435,456 times
    }
```

Not a hang in the "stuck" sense — the process is CPU-bound the whole time,
confirmed via `/proc/<pid>/status` showing `R` (running) state and climbing
`utime` on the original Linux investigation. It's a plain per-iteration
throughput wall on `ByteBuf.writeByte` and `MessageDigest.update(byte)`, times
536 million calls total (compress + decompress sides).

## What's already been fixed here

`AbstractByteBuf.writeByte` was paying two `VarHandle.get` calls at ~2µs each
through `ensureAccessible()` -> `RefCnt.isLiveNonVolatile` — a
signature-polymorphic call site the JIT's per-call-site native cache had
permanently refused to resolve (see
`varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`). Fixing that:

| | before | after |
|---|---:|---:|
| `ByteBuf.writeByte` (microbenchmark) | 2440–2578 ns | **368–654 ns** |
| `JdkZlibIntegrationTest#testHugeDecompress`, solo, uncapped | 1063 s | **455–528 s** |

A 2–4x class-level win. **Still nowhere near the suite's 180s cap.**

## What's left, and why it isn't worth chasing further right now

The residual is `MessageDigest.update(byte)` — 268M single-byte SHA-256 updates
per side, at the native-dispatch floor (~170ns/call) — plus the codec work
itself. That floor isn't specific to this cluster; it's the same per-call
native-dispatch cost this session's testing keeps running into elsewhere (the
`HashedWheelTimerTest` page and the KFusion FFM-segment page are two other
instances of the same underlying floor, each priced independently). Restructuring
the dispatch path to close it has been judged, separately, not justified by a
lever too small to measure once it lands.

`MessageDigest.update(byte[], off, len)` (the bulk-copy overload, which is what
the actual codec bodies use, not the single-byte one `testHugeDecompress` hits)
was also fixed on the way: it was reading the array one element at a time
through the collector's generic accessor; now bulk-copies (1519ns -> 315ns for
a 64-byte update).

## Not a bug to keep re-discovering

If any of these 11 classes shows up as HANG in a future run, this is why — no
new investigation needed unless the failure signature changes (e.g. a real
FAIL instead of a timeout-HANG, or a wall time that's gotten *worse* rather
than the expected "still over 180s, better than before").

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.codec.compression.JdkZlibIntegrationTest
# any of the 11 reproduces the same wall
```

## Related
- Full investigation: `fixed-suite-bugs/netty/compression-testhugedecompress-shared-timeout-20260816.md`
- `fixed-suite-bugs/netty/varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`
