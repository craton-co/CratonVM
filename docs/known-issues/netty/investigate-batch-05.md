# netty — investigate batch 05 of 13

**Status: TRIAGED 2026-08-12. No CratonVM defect on this page.** Every
not-green result is a wall-clock timeout, and the one genuine failure fails
identically on HotSpot. This page is now the sharpest dataset for the
`io.netty` per-call throughput gap.

Part of a 184-class FAIL/HANG list split across 13 pages (see
[investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page
owns exactly the 15 classes below — do not touch classes listed in other batch
pages.

Originally found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite
run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`).
"status seen" is what that run recorded; "explained by" is the 2026-08-12
triage on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`747a7c433`.

## Outcome

**Zero assertion failures across all 14 compression classes.** Every failing
test is a JUnit *per-test* timeout — `testHugeDecompress()` at the 120 s
default in three classes, `testLargeRandom()` at 120 s in a fourth, and
`Lz4FrameEncoderTest.writingAfterClosedChannelDoesNotNPE()` at the test's own
`@Timeout(3000ms)`. The 15th class fails on HotSpot too.

**The "HANG"s are not hangs.** `Bzip2IntegrationTest` was re-run with a 3000 s
cap and **completed: 13/14 in 1 964 s** (the 1 being the same 120 s per-test
cap). Its hang stack, taken with `--nojit --stack-dump-on-timeout`, is ordinary
codec work — `Bzip2Decoder.decode → Bzip2BlockDecompressor.decodeHuffmanData →
Bzip2HuffmanStageDecoder.nextSymbol` — inside `testLargeRandom`'s 1 MiB payload.

Two things were ruled out along the way:

* **Not the JIT.** `--nojit` hangs identically. (With the JIT *on*, the main
  thread produced no frames at all in a stack dump — compiled frames never
  reach the dispatch loop — which is why `--nojit` is needed to see anything.)
* **Not payload generation.** `PlatformDependent.splittableRandomNextBytes`
  fills the 1 MiB test payload in 409 ms on CratonVM vs 5 ms on HotSpot, and
  produces byte-identical output. Real, but three orders of magnitude short of
  explaining the wall.
* **Not JNI.** Bzip2, FastLz, JZlib, JdkZlib, Lzf and Snappy are pure-Java
  codecs, and the family behaves the same whether or not a native library is
  involved.

  Batch 04's `BrotliIntegrationTest` turns out to be **the same finding, not a
  contrasting one**: `docs/internal/fixed-suite-bugs/netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md`
  measured it as 10 of 11 tests passing in 4.2 s with only `testHugeDecompress`
  unfinished, the thread `R` (running) at 100% of a core in JIT-compiled Java,
  and brotli4j's library loading fine in 31 ms. `testHugeDecompress` builds
  256 MB **one byte at a time** — 268 M `writeByte` plus 268 M `digest.update`
  calls — which is this page's wall in its purest form. (My batch-04 page had
  claimed the watchdog "never fires" and inferred a native block from it; the
  watchdog does fire, and its inability to dump a *running compiled* thread was
  itself the defect, since fixed.)

## The measurement

Byte- and bit-level codecs are the densest possible call workload, so they are
where the per-call cost documented in
[adaptive-bytebuf-allocator-throughput](adaptive-bytebuf-allocator-throughput-20260812.md)
shows worst:

| class | HotSpot | CratonVM | ratio |
| --- | --- | --- | --- |
| `Bzip2IntegrationTest` | 21.3 s | **1 964 s** (13/14) | **92×** |
| `LzfIntegrationTest` | 9.9 s | 873 s (10/11) | 88× |
| `ZstdIntegrationTest` | — | 779 s (10/11) | — |
| `JdkZlibIntegrationTest` | 8.8 s | 746 s (10/11) | 85× |
| `FastLzIntegrationTest` | 20.1 s | > 900 s | > 45× |

## Classes

Legend: ✅ matches HotSpot · ⚪ fails on HotSpot too · ⏱ wall-clock only, no
assertion failure

| class | status seen | explained by |
|---|---|---|
| `io.netty.handler.codec.compression.Bzip2IntegrationTest` | HANG | ⏱ **completes 13/14 in 1 964 s** at a 3000 s cap (HotSpot 21 s); the 1 is `testHugeDecompress` at JUnit's 120 s per-test cap |
| `io.netty.handler.codec.compression.FastLzIntegrationTest` | HANG | ⏱ `testLargeRandom` at the 120 s per-test cap |
| `io.netty.handler.codec.compression.JZlibIntegrationTest` | HANG | ⏱ same family |
| `io.netty.handler.codec.compression.JdkZlibIntegrationTest` | HANG | ⏱ **10/11 in 746 s**; `testHugeDecompress` at 120 s |
| `io.netty.handler.codec.compression.LengthAwareLzfIntegrationTest` | HANG | ⏱ same family |
| `io.netty.handler.codec.compression.Lz4DecompressorTest` | FAIL | ✅ **16/16** |
| `io.netty.handler.codec.compression.Lz4FrameEncoderTest` | FAIL | ⏱ 12/13; `writingAfterClosedChannelDoesNotNPE` at the test's own `@Timeout(3000ms)` |
| `io.netty.handler.codec.compression.Lz4FrameIntegrationTest` | HANG | ⏱ same family |
| `io.netty.handler.codec.compression.LzfIntegrationTest` | HANG | ⏱ **10/11 in 873 s**; `testHugeDecompress` at 120 s |
| `io.netty.handler.codec.compression.SnappyIntegrationTest` | HANG | ⏱ same family |
| `io.netty.handler.codec.compression.SnappyJumboSizeIntegrationTest` | HANG | ⏱ same family |
| `io.netty.handler.codec.compression.ZstdDecompressorTest` | HANG | ✅ **11/11** |
| `io.netty.handler.codec.compression.ZstdEncoderTest` | FAIL | ✅ **13/13** |
| `io.netty.handler.codec.compression.ZstdIntegrationTest` | HANG | ⏱ **10/11 in 779 s**; `testHugeDecompress` at 120 s |
| `io.netty.handler.codec.dns.NativeImageHandlerMetadataTest` | FAIL | ⚪ **fails identically on HotSpot** (0/1) — GraalVM native-image metadata fixture, same as batch 04's three |

## What would move this page

Nothing here needs a per-class fix. It moves when the per-call cost does —
see [adaptive-bytebuf-allocator-throughput](adaptive-bytebuf-allocator-throughput-20260812.md)
and [arraylist-native-overhead-and-the-view-carrier-class](arraylist-native-overhead-and-the-view-carrier-class-20260812.md).
Re-measure this page after any such change: with HotSpot at 9–21 s per class,
it is a sensitive and cheap regression signal, and several classes sit just
either side of the 120 s per-test cap.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.compression.Bzip2IntegrationTest\n' > /tmp/one.txt
# needs a cap well past the suite default — it takes ~33 minutes
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 3000 \
  --bin <cratonvm> --out /tmp/repro

# where it spends the time (compiled frames are invisible, so --nojit)
<cratonvm> --java-home <jdk25> --nojit --stack-dump-on-timeout 150 \
  @common.args -Dcraton.batch=1 CratonRunner \
  io.netty.handler.codec.compression.Bzip2IntegrationTest
```
