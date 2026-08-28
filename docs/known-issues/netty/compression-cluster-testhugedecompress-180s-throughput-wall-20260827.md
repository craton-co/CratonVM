# `codec.compression.*IntegrationTest` — the shared `testHugeDecompress` wall, 2.5-4x closed 2026-08-28, 4 of 11 classes still over

## Status

**OPEN, but a different page than it was.** The shared per-byte native wall that
made all eleven classes HANG identically is fixed. Seven of the eleven now
COMPLETE, in 125-191 s where they used to run past the suite's 180 s cap; four
still do not, and their residual is a different one. No class is green yet: the
test method itself carries netty's own 120 s JUnit timeout, and only the fastest
`JdkZlibIntegrationTest` runs land under it.

Supersedes the "one shared, already-diagnosed throughput wall" framing. The
page's own retirement condition — *"no new investigation needed unless the
failure signature changes"* — has been met: the signature changed.

## What the eleven share

All eleven inherit `AbstractIntegrationTest.testHugeDecompress`, which builds a
256 MiB buffer one byte at a time:

```java
in.writeByte(byteValue);      // 268,435,456 times
digest.update(byteValue);     // 268,435,456 times, and again on the decompress side
```

## What was actually costing the time

A `--dump-native-registry` census of `JdkZlibIntegrationTest` — which the
earlier investigation never took — named it exactly. **1.07 billion generic
native dispatches**, three call sites:

```
536 870 912  java/security/MessageDigest.update(B)V
269 768 030  java/lang/invoke/VarHandle.get(...)        <- ensureAccessible -> RefCnt
268 435 456  java/nio/DirectByteBuffer.put(IB)...       <- PooledDirectByteBuf._setByte
```

`perf` put the funnel plus its receiver validation at ~45 % of the run. The
previous page attributed the residual to "the native-dispatch floor
(~170 ns/call)" and judged closing it unjustified. The floor was real; what was
missing is that all three calls had, or could have, a thin direct bind that skips
it — and that four separate mechanisms were each independently preventing those
binds from reaching these sites.

## The fix

`perf(jit): thin direct binds for the three per-BYTE natives`, on
`fix/netty-longlong-npe-and-compression-wall-20260827`. Two new thin helpers
(`ByteBuffer.put(int,byte)` / `get(int)`, `MessageDigest.update(byte)`), the
existing `VarHandle` read bind routed to where it was needed, and a per-GC-epoch
receiver memo. Per operation, against HotSpot on the same host:

| | before | after | HotSpot |
|---|---:|---:|---:|
| `MessageDigest.update(byte)` | 216 ns | **25 ns** | 8.6 ns |
| `ByteBuf.writeByte` | 700 ns | **297 ns** | 5.1 ns |
| `ByteBuf.forEachByte` | 349 ns | **122 ns** | 5.9 ns |

Same-binary A/B on `JdkZlibIntegrationTest`, B arm
`CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS=0 CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER=0`:

```
A  119 s  ok=11 failed=0        B  176 s  ok=10 failed=1
A   85 s  ok=11 failed=0        B  172 s  ok=10 failed=1
```

## Where the eleven stand now

Solo, uncapped, killed at 300 s, one run each on a quiet host:

| class | wall | |
|---|---:|---|
| `JdkZlibIntegrationTest` | 85 / 119 / 125 s | PASSES on the faster runs |
| `ZstdIntegrationTest` | 148 s | completes |
| `BrotliIntegrationTest` | 151 s | completes |
| `Lz4FrameIntegrationTest` | 157 s | completes |
| `JZlibIntegrationTest` | 179 s | completes |
| `LengthAwareLzfIntegrationTest` | 181 s | completes |
| `LzfIntegrationTest` | 191 s | completes |
| `SnappyIntegrationTest` | 500 s | still over |
| `SnappyJumboSizeIntegrationTest` | > 300 s | still over |
| `Bzip2IntegrationTest` | > 300 s | still over |
| `FastLzIntegrationTest` | > 300 s | still over |

Against 455-528 s for `JdkZlibIntegrationTest` before, and 369 s for it on
2026-08-28 `dev` immediately before this work.

## The two residuals, which are NOT the same residual

**1. `testHugeDecompress` is on netty's own 120 s boundary.** The harness cap is
180 s, but the test method carries `@Timeout`-equivalent JUnit configuration of
120 s, so a class can complete inside the harness and still report
`ok=10 failed=1` with `TimeoutException: testHugeDecompress() timed out after
120 seconds`. `JdkZlibIntegrationTest` straddles it: 85 s and 119 s runs pass,
125 s and 143 s runs do not. Run-to-run spread on this workload is ±25 %, so
this needs roughly another 1.3x of headroom to be reliable, not a new mechanism.

What is left in its profile after the binds is diffuse: 41 % in JIT-compiled
code and the rest spread across field access through the collector
(`get_field_as` / `coerce_field_value_for_slot` / `read_value_cell_checked`),
`jit_checkcast`, `jit_invoke_virtual_mic`, and the accumulator's per-byte
`Mutex` + hash. No single item is above 8 %.

**2. Snappy / Bzip2 / FastLz are a different wall.** `SnappyIntegrationTest`
runs 500 s for 15 tests, and its census has NO concentrated funnel left — the
largest native is 175 M `VarHandle.get`, and its profile's top entry is 3.8 %.
Its cost is the platform's general per-operation overhead against netty's
pure-Java codec loops, which is the subject of `the snappy wall is VM runtime,
not compiled code` and not of this page. Attributing these three to the shared
`testHugeDecompress` wall was correct while the wall dominated everything; it is
not correct now.

## Not a bug to keep re-discovering

If one of the seven shows up as FAIL with `TimeoutException:
testHugeDecompress() timed out after 120 seconds`, this is why, and the number
to watch is the CLASS wall time: above ~200 s means something regressed, because
these now sit at 125-191 s. A HANG at the 180 s cap from one of the seven IS new
and worth investigating.

`CRATONVM_DBG=jit-method-stats` prints the bound-site and served/declined counts
for the three helpers. `ByteBuffer.byteElement=0` or a served count far below the
call count is the shape of a bind that stopped reaching these sites — which
happened four times while this was being written, once per compile door.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.codec.compression.JdkZlibIntegrationTest
```

## Related

* Full investigation and fix history of the earlier rounds:
  `fixed-suite-bugs/netty/compression-testhugedecompress-shared-timeout-20260816.md`
* `fixed-suite-bugs/netty/varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`
* The per-call native-dispatch floor, priced independently elsewhere:
  the `HashedWheelTimerTest` page and the KFusion FFM-segment page.
