# All 11 `codec.compression.*IntegrationTest` classes hang — one shared, already-diagnosed cause

**Status: RETIRED 2026-08-17 — the cause this page named was half right, and the
half it got wrong has been FIXED.**

This page attributed the wall entirely to "the `ByteBuf`/interpreter↔JIT
per-call-cost workstream" and concluded there was "no per-class work to do here
beyond that". That is now known to be wrong for the larger share of it.
`AbstractByteBuf.writeByte` was not paying generic per-call cost; it was paying
**two `VarHandle.get` calls at ~2 µs each**, through `ensureAccessible()` →
`RefCnt.isLiveNonVolatile`, because a signature-polymorphic call site is
unresolvable by its own descriptor and the JIT's per-call-site native cache had
therefore cached a permanent refusal for every one of them. See
`fixed-suite-bugs/netty/varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`.

| | before | after |
| --- | ---: | ---: |
| `ByteBuf.writeByte` (microbenchmark) | 2 440–2 578 ns | **368–654 ns** |
| `JdkZlibIntegrationTest#testHugeDecompress`, solo, no per-method cap | 1 063 s | **455–528 s** |

**What that does and does not settle.** The root-cause analysis below stands in
every structural respect — one shared base-class method, `testHugeDecompress`,
inherited by all eleven classes, building 256 MiB one byte at a time — and the
per-iteration cost of that loop has come down 4.0–6.6× (the spread is host
load, not the change; both arms were interleaved in each measurement). It is
still not under the
suite's 180 s per-class cap. The residual is `MessageDigest.update(byte)` (268 M
single-byte SHA-256 updates on the compress side and 268 M more on the
decompress side, at the ~170 ns native-dispatch floor) plus the codec work
itself. That floor is not netty's and not this page's: it is
`docs/known-issues/perf/vm-per-call-dispatch-cost-20260813.md`, which was
reopened on 2026-08-17 because it is the named residual of these eleven classes
and of the sibling `io.netty.buffer` page. Its own conclusion — that
restructuring `invoke_or_native` cannot be justified by a lever too small to
measure after it lands — is unchanged.

So the eleven classes will still report HANG at a 180 s cap, and this page still
answers "why" for whoever sees that. It moves here because the one actionable
defect inside it has been found and fixed, and because the sentence that would
have sent the next reader to the wrong workstream is corrected above.

One adjacent fix landed from the same profile:
`MessageDigest.update(byte[], off, len)` was reading the array one element at a
time through the collector's generic accessor; it now bulk-copies
(1 519 ns → 315 ns for a 64-byte update), which is the `update` overload the
codecs themselves use.

---

*Original text follows, unedited.*

**Status: OPEN** (known cause, not a mystery — see "What's actually needed" below),
characterised 2026-08-16/17 against `dev` `3ef3eb7441c73b2e40e061e20de2fc2622478eee`.
Seen HANGing on generational, G1, **and** ZGC alike in today's full 657-class suite
run, timeout=180s per class:

```
io.netty.handler.codec.compression.BrotliIntegrationTest
io.netty.handler.codec.compression.Bzip2IntegrationTest
io.netty.handler.codec.compression.FastLzIntegrationTest
io.netty.handler.codec.compression.JZlibIntegrationTest
io.netty.handler.codec.compression.JdkZlibIntegrationTest
io.netty.handler.codec.compression.LengthAwareLzfIntegrationTest
io.netty.handler.codec.compression.Lz4FrameIntegrationTest
io.netty.handler.codec.compression.LzfIntegrationTest
io.netty.handler.codec.compression.SnappyIntegrationTest
io.netty.handler.codec.compression.SnappyJumboSizeIntegrationTest
io.netty.handler.codec.compression.ZstdIntegrationTest
```

**This is one bug, not eleven**, and it is not new: it is the same wall
[`netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md`](../../fixed-suite-bugs/netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md)
already characterised for `BrotliIntegrationTest` alone, now confirmed to
reproduce identically across every sibling class in the package because the
hang lives in code all eleven inherit, not in anything codec-specific.

## Root cause: a shared base-class test method, not per-codec logic

Every one of the eleven classes extends
`io.netty.handler.codec.compression.AbstractIntegrationTest`
(`apps/netty/codec-compression/src/test/java/.../AbstractIntegrationTest.java`),
directly or transitively (`LengthAwareLzfIntegrationTest extends
LzfIntegrationTest extends AbstractIntegrationTest`). That base class defines
`testHugeDecompress`, which builds **256 MiB one byte at a time** before any
compression happens:

```java
for (int i = 0; i <= numberOfChunks; i++) {          // numberOfChunks = 256
    ByteBuf in = compressChannel.alloc().buffer(chunkSize);   // chunkSize = 1 MiB
    for (int j = 0; j < chunkSize; j++) {
        in.writeByte(byteValue);      // <- 268,435,456 times
        digest.update(byteValue);     // <- 268,435,456 times
    }
    ...
}
```

The Brotli investigation measured this exact loop at ~3,054 ns/iteration on
CratonVM against ~10 ns/iteration on HotSpot (the `ByteBuf.writeByte` call
chain alone is ~300x, because the JIT cannot inline through
`AbstractByteBuf.writeByte` → `ensureWritable0` → `_setByte` →
`HeapByteBufUtil`/`PlatformDependent`, paying two JIT↔interpreter transitions
per byte) — **~820 s to build the input**, against the harness's 180 s
per-class cap and even against Brotli's own 60s `@Timeout`-free budget. No
codec ever gets exercised; the class times out inside data setup.

Since `testHugeDecompress` is defined once in the shared base class and none
of the eleven subclasses override, `@Disabled`, or otherwise touch it
(`grep -l testHugeDecompress *.java` in the package matches only
`AbstractIntegrationTest.java`), **every class that inherits it pays the same
wall regardless of which compression algorithm it wraps.** This is expected to
generalize to any other `AbstractIntegrationTest` subclass not in today's list
(e.g. `FastLzIntegrationTest$TestWithChecksum`/`$TestRandomChecksum`, which
aren't independently selected by the suite runner today).

## Confirmed: it's specifically `testHugeDecompress`, not an earlier test

`ProgressRunner` (ad-hoc `apps/netty-suite-runner/ProgressRunner.java`, prints
`@@START`/`@@FINISH` per test method, unlike the suite's `CratonRunner` which
only reports at class end) against `Bzip2IntegrationTest` on G1:

```
@@START test5Tables()        ... FINISH SUCCESSFUL  (6.3s)
@@START test4Tables()        ... FINISH SUCCESSFUL  (0.3s)
@@START test3Tables()        ... FINISH SUCCESSFUL  (0.2s)
@@START testLargeRandom()    ... FINISH SUCCESSFUL  (73.6s)
@@START testLongBlank()      ... FINISH SUCCESSFUL  (1.8s)
@@START testRegular()        ... FINISH SUCCESSFUL  (0.05s)
@@START testSequential()     ... FINISH SUCCESSFUL  (0.1s)
@@START testEmpty()          ... FINISH SUCCESSFUL  (0.03s)
@@START testPartRandom()     ... FINISH SUCCESSFUL  (0.5s)
@@START testLongSame()       ... FINISH SUCCESSFUL  (1.5s)
@@START testOneByte()        ... FINISH SUCCESSFUL  (0.05s)
@@START testTwoBytes()       ... FINISH SUCCESSFUL  (0.04s)
@@START testCompressible()   ... FINISH SUCCESSFUL  (0.4s)
@@START testHugeDecompress() ... never finishes
```

All 13 other test methods (Bzip2's own `test{3,4,5}Tables` plus every
inherited `AbstractIntegrationTest` method) pass in ~85 s combined.
`testHugeDecompress` starts last and was still running, unfinished, when the
probe was killed — exactly the shape the Brotli doc already recorded for that
class.

## Isolation: genuine hangs, not host contention

These 11 were first seen HANGing inside a full 657-class 3-collector-in-parallel
run. Re-run in isolation, one class per process (`--gc g1 --shards 1`, so no
concurrent forks):

| class | isolated result |
|---|---|
| `BrotliIntegrationTest` | HANG, rc=124, 180s |
| `Bzip2IntegrationTest` | HANG, rc=124, 180s |
| `FastLzIntegrationTest` | HANG, rc=124, 180s |
| `JZlibIntegrationTest` | HANG, rc=124, 180s |
| `JdkZlibIntegrationTest` | HANG, rc=124, 180s |
| `LengthAwareLzfIntegrationTest` | HANG, rc=124, 180s |
| `Lz4FrameIntegrationTest` | HANG, rc=124, 180s |
| `LzfIntegrationTest` | HANG, rc=124, 180s |
| `SnappyIntegrationTest` | HANG, rc=124, 180s |
| `SnappyJumboSizeIntegrationTest` | HANG, rc=124, 180s |
| `ZstdIntegrationTest` | HANG, rc=124, 180s |

`HANG=11` of 11, total wall 33m4s = 11 x 180s exactly — see
`apps/netty-suite-runner/runs/agent-compression-20260816/run-20260816-235922-passed/on-real/shard-0/results.tsv`
on the `cratonvm` checkout. Each class ran alone in its own forked process
(`--shards 1`), so no other CratonVM fork was contending with it while it hung.

**Not a contention artifact.** The isolated runs (single fork, one class at a
time) reproduce the identical 180s HANG for every class, matching the original
full-suite observation exactly. The host was carrying unrelated load at the
time (other `cratonvm.exe`/`java.exe` processes from other sessions, ~70%
average CPU) — but the HotSpot cross-check below ran on the *same* host at the
*same* time and passed every class in under a minute, which is the strongest
argument that the CratonVM-side gap is a real per-call cost problem and not
noise from a shared box.

## HotSpot cross-check: clean pass, seconds not minutes

Same 11-class list, `--hotspot` (JDK 25.0.3):

| class | found | ok | failed | ms |
|---|---|---|---|---|
| `BrotliIntegrationTest` | 11 | 11 | 0 | 15,014 |
| `Bzip2IntegrationTest` | 14 | 14 | 0 | 25,754 |
| `FastLzIntegrationTest` | 11 | 11 | 0 | 23,033 |
| `JZlibIntegrationTest` | 11 | 11 | 0 | 16,718 |
| `JdkZlibIntegrationTest` | 11 | 11 | 0 | 15,297 |
| `LengthAwareLzfIntegrationTest` | 11 | 11 | 0 | 15,950 |
| `Lz4FrameIntegrationTest` | 11 | 11 | 0 | 13,043 |
| `LzfIntegrationTest` | 11 | 11 | 0 | 42,611 |
| `SnappyIntegrationTest` | 15 | 15 | 0 | 24,393 |
| `SnappyJumboSizeIntegrationTest` | 11 | 11 | 0 | 17,627 |
| `ZstdIntegrationTest` | 11 | 11 | 0 | 16,903 |

All 11 classes, 11 processes, total wall 4m2s — less than one CratonVM class's
timeout budget. `testHugeDecompress` is not disproportionately slow on HotSpot;
it's the CratonVM-specific ~300x `ByteBuf.writeByte` per-call gap that turns a
sub-second loop into an 800+ second one.

## `BrotliIntegrationTest`: not a reopening, not a regression

The existing `netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md` is
titled FIXED, but its "FIXED" scope was narrow and explicit: it fixed a
**watchdog misdiagnosis** (the stack-dump-on-timeout hook blaming "blocked in
native code" for a thread that was actually RUNNING JIT-compiled code) and
**characterised** the throughput wall. It never claimed `testHugeDecompress`
itself would finish, and said so directly in its own repro section:
`testHugeDecompress` will finish only "when a per-byte `ByteBuf.writeByte`
costs on the order of 20 ns instead of 2,500" — i.e., that page left the hang
itself explicitly open, tracked as a VM-wide performance item, not a
`BrotliIntegrationTest`-specific defect. Today's HANG on `BrotliIntegrationTest`
is that same predicted, still-unfixed behavior recurring — **not a
regression**.

## What's actually needed

Nothing codec- or class-specific. The fix belongs entirely to the
`ByteBuf`/interpreter↔JIT per-call-cost workstream, already tracked at
[`../../performance/netty-per-call-throughput-20260813.md`](../../performance/netty-per-call-throughput-20260813.md)
(RETIRED, kept as a measurement record) and
[`docs/known-issues/perf/vm-per-call-dispatch-cost-20260813.md`](../../../known-issues/perf/vm-per-call-dispatch-cost-20260813.md).
When a per-byte `ByteBuf` call chain inlines (or the JIT↔interpreter transition
cost drops well below the current ~300-700 ns/entry), all eleven classes
should clear `testHugeDecompress` together — there is no per-class work to do
here beyond that. This page exists so the next suite run doesn't re-open
eleven separate investigations for one already-understood wall.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' \
  io.netty.handler.codec.compression.BrotliIntegrationTest \
  io.netty.handler.codec.compression.Bzip2IntegrationTest \
  io.netty.handler.codec.compression.FastLzIntegrationTest \
  io.netty.handler.codec.compression.JZlibIntegrationTest \
  io.netty.handler.codec.compression.JdkZlibIntegrationTest \
  io.netty.handler.codec.compression.LengthAwareLzfIntegrationTest \
  io.netty.handler.codec.compression.Lz4FrameIntegrationTest \
  io.netty.handler.codec.compression.LzfIntegrationTest \
  io.netty.handler.codec.compression.SnappyIntegrationTest \
  io.netty.handler.codec.compression.SnappyJumboSizeIntegrationTest \
  io.netty.handler.codec.compression.ZstdIntegrationTest \
  > compression-classes.txt
./run-netty-suite.sh --list compression-classes.txt --gc g1 --shards 1
# expect: every class HANG, rc=124, 180s
./run-netty-suite.sh --list compression-classes.txt --hotspot --shards 1
# expect: every class PASS in well under 180s (oracle)
```

## Related

* [`netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md`](../../fixed-suite-bugs/netty-brotli-huge-decompress-not-a-hang-FIXED-20260812.md) — the original per-class diagnosis (watchdog fix + throughput measurement), whose predicted outcome this doc confirms across the whole package.
* [`../../performance/netty-per-call-throughput-20260813.md`](../../performance/netty-per-call-throughput-20260813.md) — retired, kept as the measurement record for the underlying `ByteBuf` per-call cost.
* [`docs/known-issues/perf/vm-per-call-dispatch-cost-20260813.md`](../../../known-issues/perf/vm-per-call-dispatch-cost-20260813.md) — the VM-wide per-call dispatch cost this belongs to.
