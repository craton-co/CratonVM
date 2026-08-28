# `codec.compression.*IntegrationTest` — the shared `testHugeDecompress` wall, 2.4x closed 2026-08-28, 4 of 11 classes still over

## Status

**OPEN, but a different page than it was.** The shared per-byte native wall that
made all eleven classes HANG identically is fixed. One class now PASSES, six
COMPLETE in 149-203 s where they used to run past the suite's 180 s cap, and
four are still over 320 s with a residual that is not the one this page
describes.

The page's own retirement condition — *"no new investigation needed unless the
failure signature changes"* — has been met: the signature changed, from
`process-died rc=124 timeout=180s` to a class that finishes and reports
`TimeoutException: testHugeDecompress() timed out after 120 seconds`, which is
netty's own per-method timeout and not the harness's.

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
(~170 ns/call)" and judged closing it unjustified. The floor was real. What was
missing is that all three calls have a thin direct bind that skips it — and that
FOUR separate mechanisms were each independently keeping those binds away from
these sites (the inline splice, the OSR door, the IR tier's `is_intrinsic_site`
routing, and the same routing again for `VarHandle`).

## The fix

`perf(jit): thin direct binds for the three per-BYTE natives`, on
`fix/netty-longlong-npe-and-compression-wall-20260827`. Per operation, against
HotSpot on the same host:

| | before | after | HotSpot |
|---|---:|---:|---:|
| `MessageDigest.update(byte)` | 216 ns | **25 ns** | 8.6 ns |
| `ByteBuf.writeByte` | 700 ns | **297 ns** | 5.1 ns |
| `ByteBuf.forEachByte` | 349 ns | **122 ns** | 5.9 ns |

Same-binary A/B on `JdkZlibIntegrationTest`, B arm
`CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS=0 CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER=0`,
on merged `dev` (2026-08-28):

```
A  149 s / 150 s / 151 s          B  368 s / 227 s
```

The B arm reproduces the 369 s this class measured on `dev` immediately before
the work, which is what says the win is the binds and not the routing changes
that carry them.

## Where the eleven stand

Solo, uncapped, killed at 320 s, one run each on a quiet host, merged `dev`:

| class | wall | |
|---|---:|---|
| `LengthAwareLzfIntegrationTest` | 103 s | **PASSES**, `ok=11 failed=0` |
| `JdkZlibIntegrationTest` | 149-151 s | completes |
| `ZstdIntegrationTest` | 151 s | completes |
| `BrotliIntegrationTest` | 153 s | completes |
| `Lz4FrameIntegrationTest` | 154 s | completes |
| `JZlibIntegrationTest` | 184 s | completes |
| `LzfIntegrationTest` | 203 s | completes |
| `SnappyIntegrationTest` | > 320 s (500 s uncapped) | still over |
| `SnappyJumboSizeIntegrationTest` | > 320 s | still over |
| `Bzip2IntegrationTest` | > 320 s | still over |
| `FastLzIntegrationTest` | > 320 s | still over |

Against 455-528 s for `JdkZlibIntegrationTest` when this page was written, and
369 s for it on `dev` immediately before this work.

"Completes" means the class finishes and reports `ok=10 failed=1`: the class is
inside the harness's 180 s cap (or near it) but `testHugeDecompress` is still
over netty's own 120 s per-method timeout.

## The two residuals, which are NOT the same residual

**1. `testHugeDecompress` sits just over netty's 120 s method timeout.** The
seven that complete need roughly another 1.3x, not a new mechanism.
`LengthAwareLzfIntegrationTest` already has it. Run-to-run spread on this
workload is wide — `JdkZlibIntegrationTest` measured 85 s, 119 s, 125 s and
143 s on the pre-merge tree and a tight 149-151 s after — so a single run is not
evidence either way.

What is left in its profile after the binds is diffuse: 41 % in JIT-compiled
code, and the rest spread across field access through the collector
(`get_field_as` / `coerce_field_value_for_slot` / `read_value_cell_checked`),
`jit_checkcast`, `jit_invoke_virtual_mic`, and the digest accumulator's per-byte
`Mutex` + hash. No single item is above 8 %.

**2. Snappy / SnappyJumbo / Bzip2 / FastLz are a different wall.**
`SnappyIntegrationTest` runs 500 s uncapped for 15 tests, and its census has NO
concentrated funnel left — the largest native is 175 M `VarHandle.get` and its
profile's top entry is 3.8 %. Its cost is the platform's general per-operation
overhead against netty's pure-Java codec loops, which belongs to `the snappy
wall is VM runtime, not compiled code` and not to this page. Grouping these four
with the other seven was correct while the shared wall dominated everything; it
is not correct now.

## Not a bug to keep re-discovering

If one of the seven shows up as FAIL with `TimeoutException:
testHugeDecompress() timed out after 120 seconds`, this is why, and the number
to watch is the CLASS wall time: above ~250 s means something regressed, because
these now sit at 103-203 s. **A HANG at the 180 s cap from one of the seven IS
new** and worth investigating — that was the old signature and it should not
come back.

`CRATONVM_DBG=jit-method-stats` prints the bound-SITE and served/declined CALL
counts for the three helpers. `ByteBuffer.byteElement=0`, or a served count far
below the call count, is the shape of a bind that stopped reaching these sites —
which happened four times while this was being written, once per compile door.
A site count is not an engagement count; only the second one says the fast path
is running.

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
* The per-call native-dispatch floor, priced independently elsewhere: the
  `HashedWheelTimerTest` page and the KFusion FFM-segment page.
