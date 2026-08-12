# netty `io.netty.buffer` throughput gap — 10× broadly, ~90× through `AdaptivePoolingAllocator`

**Status:** OPEN (2026-08-12). Quantified while triaging
[netty investigate-batch-01](investigate-batch-01.md) on the Azure Linux host
(`20.80.105.49`), binary built from `origin/dev` `1c4ce7d3a` plus that batch's
three correctness fixes.

Supersedes the one-sample estimate in
`../../internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md` ("~12x throughput gap"): that figure was
taken before the `TimeUnit`-ordinal defect was found, so part of what it
attributed to slowness was actually `await(30, SECONDS)` waiting 30 ms.

## The numbers

Same classpath, same box, CratonVM solo (`--shards 1`) so the measurement is
not competing with its own forks. HotSpot JDK 25 is the oracle.

| class | tests | HotSpot | CratonVM solo | ratio | result |
| --- | --- | --- | --- | --- | --- |
| `BigEndianHeapByteBufTest` | 414 | 5.2 s | 54 s | **10×** | PASS |
| `AdaptiveBigEndianHeapByteBufTest` | 417 | 6.0 s | 237 s | 40× | PASS |
| `AdaptiveByteBufAllocatorGrowthTest` | 400 | 11.3 s | 829 s | **73×** | PASS |
| `AdaptiveByteBufAllocatorTest` | 127 | 5.8 s | 500–535 s | **~90×** | PASS (1 aborted) |

**None of these hang.** Every one completes and passes when given room; they
were recorded as HANG only because they cross the suite's 180 s (Windows) /
900 s (here, 3-way sharded on a box at load 20) wall cap. Two per-test JUnit
timeouts have the same cause:

* `AdaptiveByteBufAllocatorTest.purgeScanShouldEvictIdleChunks` and
  `AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest`'s equivalent —
  JUnit's default 120 s per-test cap. Both pass in a solo run.
* `AdaptiveBigEndianDirectByteBufTest.testInternalNioBuffer` — same 120 s cap.
  It reads 64 MiB out of a direct buffer one `ByteBuffer.get()` at a time,
  i.e. ~100 M single-byte native calls.

Host caveat: this box runs many concurrent agents; load average during these
runs varied between 2 and 24 and the CratonVM column moves with it (the same
class measured 862 s at load ~20 and 500 s at load ~4). The ratios are
order-of-magnitude, not benchmark-grade. HotSpot's column is stable because
its absolute times are seconds.

## The `AdaptivePoolingAllocator` classes are ~9× worse than the baseline gap

10× is the general `io.netty.buffer` interpreter/JIT gap. The
`AdaptiveByteBufAllocator*` classes are 73–90×, so something specific to
`AdaptivePoolingAllocator` costs another ~9× on top.

An ABBA-interleaved A/B isolates where it goes. Arm B forces netty onto the
code path HotSpot 25 uses by default (`-Dio.netty.noUnsafe=true`), which
bypasses `PlatformDependent0`'s `sun.misc.Unsafe` accessors:

| arm | round 1 | round 2 | ok | failed |
| --- | --- | --- | --- | --- |
| A — default (`hasUnsafe=true`) | 535 s | 500 s | 126 | 0 |
| B — `-Dio.netty.noUnsafe=true` | 77 s | 77 s | 17 | 109 |

**Arm B is 7× faster.** The same 127 test methods, the same allocator, the
only difference being whether netty reaches memory through CratonVM's
`sun.misc.Unsafe` natives or through `ByteBuffer`/FFM. That points the
investigation squarely at the Unsafe natives on the bulk-access path
(`UnsafeByteBufUtil`, `copyMemory`, `setMemory`, unaligned `getLong`/`putLong`)
rather than at the interpreter in general.

Arm B is not a usable configuration — its 109 failures are all
`MemorySegment.asByteBuffer()` being unimplemented, see
[memorysegment-asbytebuffer-unimplemented](memorysegment-asbytebuffer-unimplemented-20260812.md)
— but as an instrument it is clean and perfectly reproducible.

## Suggested next step

Census the Unsafe natives reached during `AdaptiveByteBufAllocatorTest` (a
native-invocation census, not a stack sample — native calls do not appear in
the sampler) and compare the per-call cost of the bulk accessors against the
`ByteBuffer` equivalents that arm B uses. A 7× swing on identical Java work
means the two implementations of the same operation are far apart, and the
faster one is already in the tree.

## Repro

```bash
cd apps/netty-suite-runner
CP_ARGS=@common.args
for arm in default nounsafe; do
  flag=""; [ $arm = nounsafe ] && flag=-Dio.netty.noUnsafe=true
  /usr/bin/time -f "$arm %e s" <cratonvm> --java-home <jdk25> --Xmx 1500m $flag \
      $CP_ARGS -Dcraton.batch=1 CratonRunner io.netty.buffer.AdaptiveByteBufAllocatorTest
done
# HotSpot oracle
CP=$(sed -n 2p common.args)
/usr/bin/time -f "hotspot %e s" /data/toolchain/jdk-25/bin/java -cp "$CP" \
    -Djunit.jupiter.execution.timeout.default=120s -Dcraton.batch=1 \
    CratonRunner io.netty.buffer.AdaptiveByteBufAllocatorTest
```
