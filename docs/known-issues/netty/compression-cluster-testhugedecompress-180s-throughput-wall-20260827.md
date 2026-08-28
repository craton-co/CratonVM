# `codec.compression.*IntegrationTest` — three classes now PASS, four are inside the cap, four are not

## Status

**OPEN, and a different page from the one that was written.** Every one of the
eleven used to HANG at the suite's 180 s cap. Three now PASS outright, four more
COMPLETE inside the cap and fail only netty's own 120 s per-method timeout, and
four are still over. Two distinct defects were behind the original wall and both
are fixed; what remains is a third thing, named at the bottom, that is not a
defect at all.

The page's own retirement condition — *"no new investigation needed unless the
failure signature changes"* — has been met twice over.

## What the eleven share

All eleven inherit `AbstractIntegrationTest.testHugeDecompress`, which builds a
256 MiB buffer one byte at a time and pushes it through the class's codec:

```java
in.writeByte(byteValue);      // 268,435,456 times
digest.update(byteValue);     // 268,435,456 times, and again on the decompress side
```

## Defect 1 — 1.07 billion generic native dispatches

A `--dump-native-registry` census of `JdkZlibIntegrationTest`, which the earlier
investigation never took, named it exactly:

```
536 870 912  java/security/MessageDigest.update(B)V
269 768 030  java/lang/invoke/VarHandle.get(...)        <- ensureAccessible -> RefCnt
268 435 456  java/nio/DirectByteBuffer.put(IB)...       <- PooledDirectByteBuf._setByte
```

`perf` put the funnel and its receiver validation at ~45 % of the run. The
previous page priced this as an irreducible "~170 ns native-dispatch floor". The
floor was real; what was missing is that all three calls have a thin direct bind
that skips it, and that FOUR mechanisms were each independently keeping those
binds away from these sites. Fixed by `perf(jit): thin direct binds for the
three per-BYTE natives`:

| | before | after | HotSpot |
|---|---:|---:|---:|
| `MessageDigest.update(byte)` | 216 ns | **25 ns** | 8.6 ns |
| `ByteBuf.writeByte` | 700 ns | **297 ns** | 5.1 ns |
| `ByteBuf.forEachByte` | 349 ns | **122 ns** | 5.9 ns |

Same-binary A/B on `JdkZlibIntegrationTest`, B arm
`CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS=0 CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER=0`:
**A 149/150/151 s against B 368/227 s**, where the B arm reproduces the 369 s
this class measured immediately before the work.

## Defect 2 — one `iinc_w` made a whole method permanently uncompilable

The four slowest classes did not have defect 1's shape at all, and a per-TEST
breakdown is what said so: `testHugeDecompress` was 618 s of FastLz's 658 s, and
`testLargeRandom` — one megabyte through the same codec — took 33.8 s.

`FastLz.compress` operates on `ByteBuf` directly and is 1617 bytes whose entire
job is one loop, so the method-entry door never sees it hot and OSR is its only
route to compiled code. It contains three `iinc_w` instructions — of which
`iinc_w 18, -255` is nothing more exotic than an increment too big for a signed
byte — and `jit_scan` had no arm for the `wide` prefix. Its catch-all `None`
calls `mark_jit_bail_listed`, which bans the method from EVERY compile door for
the life of the process. 256 MiB of compression ran in the **interpreter**.

Fixed by `fix(jit): implement the wide prefix`:

    FastLz encode, 1 MiB, steady state:   5115 ms  ->  844 ms   (6.1x)

## Where the eleven stand

Solo, uncapped, one run each on a quiet host, merged `dev` 2026-08-28.
`regression-suite/run.sh` 72/72 on the same binary.

| class | wall | |
|---|---:|---|
| `JdkZlibIntegrationTest` | 94 s | **PASSES** `ok=11 failed=0` |
| `ZstdIntegrationTest` | 109 s | **PASSES** `ok=11 failed=0` |
| `LengthAwareLzfIntegrationTest` | 110 s | **PASSES** `ok=11 failed=0` |
| `JZlibIntegrationTest` | 142 s | completes |
| `BrotliIntegrationTest` | 142 s | completes |
| `Lz4FrameIntegrationTest` | 146 s | completes |
| `LzfIntegrationTest` | 148 s | completes |
| `FastLzIntegrationTest` | 322 s | over |
| `SnappyIntegrationTest` | 334 s | over |
| `SnappyJumboSizeIntegrationTest` | 408 s | over |
| `Bzip2IntegrationTest` | 489 s | over |

Against 455-528 s for `JdkZlibIntegrationTest` when this page was written, and
369 s for it on `dev` immediately before this work. "Completes" means the class
finishes inside the harness's 180 s cap and reports `ok=10 failed=1`:
`TimeoutException: testHugeDecompress() timed out after 120 seconds`, which is
netty's own per-method timeout, not the harness's.

## What is left, and why it is not a third defect

The four that remain, and the four that are 20-30 s over their method timeout,
are all now limited by the same thing: **the `ByteBuf` per-byte accessor chain in
compiled code**. `FastLz.compress` reads and writes its input through
`ByteBuf.getByte(int)` / `setByte(int,int)`, and each one is

```
invokevirtual AbstractByteBuf.getByte  ->  checkIndex  ->  ensureAccessible
    ->  RefCnt VarHandle read  ->  _getByte (virtual)  ->  ByteBuffer.get  ->  helper
```

— four virtual calls and a helper call where HotSpot inlines the whole chain to
about two instructions. With `compress` compiled, FastLz encode is 844 ms/MiB
against HotSpot's 37 ms: **23x**, and the profile behind it is flat. The largest
single entry is 5.9 %, and the top ten are the receiver memo, the `VarHandle`
read, the collector's field accessors and `jit_checkcast` — none of them a
funnel, a stuck method, or anything else this page can name and fix.

Closing that is inlining depth and devirtualisation through netty's `ByteBuf`
hierarchy. It is a project, not a residual of this page, and it is the same
ceiling `the snappy wall is VM runtime, not compiled code` describes —
`SnappyIntegrationTest` has no `wide` at all and never had defect 1's census.

## Not a bug to keep re-discovering

`TimeoutException: testHugeDecompress() timed out after 120 seconds` on one of
the seven that complete is the EXPECTED failure today. The number to watch is
the CLASS wall time; above ~250 s for one of the seven means something
regressed. **A HANG at the 180 s cap is new** — that was the old signature and
it should not come back.

Three instruments earn their keep here, because each one was the only thing that
could see its own defect:

* `CRATONVM_DBG=jit-method-stats` prints bound-SITE and served/declined CALL
  counts for the three thin helpers. A site count is not an engagement count:
  `ByteBuffer.byteElement=2` with `served=1636` against five million funnel
  invocations is what named the IR-tier routing gap.
* `[cratonvm-jitc] OSR-compile FAILED … stage=` names which region of
  `compile_osr_artifact` refused. An OSR refusal is silent AND permanent — the
  loop just runs interpreted forever while `OSR-recompile
  reason=no-cached-artifact` repeats — and that function has ~25 bare
  `return None`s.
* `[cratonvm-jitc] scan-bail op=0x…` names the byte `jit_scan` refused. It is
  what turned "OSR failed" into "opcode 0xc4 at pc 960".

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.codec.compression.FastLzIntegrationTest
```

`TimedRunner` (same directory, same arguments) prints a per-TEST breakdown,
which is what separated `testHugeDecompress` from the rest of a class and made
`testLargeRandom` usable as a 30-second stand-in for a 600-second one.

## Related

* `fixed-suite-bugs/netty/compression-testhugedecompress-shared-timeout-20260816.md`
* `fixed-suite-bugs/netty/varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`
* The per-call native-dispatch floor, priced independently elsewhere: the
  `HashedWheelTimerTest` page and the KFusion FFM-segment page.
