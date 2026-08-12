# netty — investigate batch 01 of 13

**Status: TRIAGED, and the CratonVM defects behind it are FIXED (2026-08-12).**
Every class on this page has been reproduced on Linux, cross-checked against
stock HotSpot JDK 25, and explained. **After the fixes there is no test on this
page that fails on CratonVM and passes on HotSpot.**

Part of a 184-class FAIL/HANG list split across 13 pages (see
[investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page
owns exactly the 15 classes below — do not touch classes listed in other batch
pages.

Originally found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite
run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`).
"status seen" is what that run recorded; "explained by" is the 2026-08-12
triage on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`1c4ce7d3a`.

## Outcome

Three CratonVM JDK-contract defects accounted for **every** assertion failure
on this page. They are fixed in
`docs/internal/fixed-suite-bugs/netty-batch01-timed-wait-and-bytebuf-contract-FIXED-20260812.md`:

1. **Timed `java.util.concurrent` waits read the `TimeUnit` ordinal from object
   slot 0**, which on the real JDK enum holds the `name` String — so nine
   natives silently fell back to MILLISECONDS and `await(30, SECONDS)` waited
   30 ms. This is the whole `CyclicBarrier await timed out` /
   `BrokenBarrierException` cluster: 22 of the 34 failing test methods. Reach
   goes far past netty (`poll`, `tryLock`, `tryAcquire`, `Future.get`,
   `Exchanger.exchange`).
2. **`ByteArrayInputStream.read(byte[],int,int)` checked `len == 0` before
   EOF**, answering `0` where the JDK answers `-1` —
   `AbstractByteBufTest.testStreamTransfer1` on all 10 concrete ByteBuf classes.
3. **A zero-length bulk copy at a direct buffer's limit threw
   `IllegalStateException`** instead of being a no-op —
   `AbstractByteBufTest.writerIndexBoundaryCheck4`.

| | before | after |
| --- | --- | --- |
| classes matching HotSpot's per-test result exactly | 1 | **12** of 15 |
| distinct failing test methods | 34 | **2** (both fail on HotSpot too) |
| `io.netty.buffer` classes with an assertion failure | 10 | **0** |

The remaining three classes differ from HotSpot only in which tests are
*skipped* (a failed `Assumptions.assumeTrue`), never in a failure.

**"HANG" was mostly an artefact of the wall cap.** These classes were finishing
in 130–180 s against the Windows run's 180 s cap. Run solo they all complete:
`AdaptiveByteBufAllocatorGrowthTest` passes in 829 s,
`AdaptiveByteBufAllocatorTest` in 500 s,
`AdaptiveBigEndianDirectByteBufTest` in 70 s. The gap is throughput, not a
deadlock — filed separately.

**Collector-independent.** Re-run under `-XX:+UseG1GC`:
`BigEndianHeapByteBufTest` 414/414, `BigEndianDirectByteBufTest` 413/413,
`AdaptiveBigEndianHeapByteBufTest` 415/417 (2 skipped),
`AdvancedLeakAwareByteBufTest` 426/426 — identical to the default collector and
to HotSpot.

## Classes

Legend: ✅ matches HotSpot · ⚪ not a CratonVM bug · ⚠ differs only in what is
skipped · ⏱ wall-clock only, passes solo

| class | status seen | explained by |
|---|---|---|
| `io.netty.bootstrap.BootstrapTest` | FAIL | ⚪ `mustCallInitializerExtensions()` fails identically on stock HotSpot JDK 25 (`expected: <[id: 0x…]> but was: <null>`) — a ServiceLoader/classpath artefact of this runner. CratonVM matches HotSpot exactly (16/17). |
| `io.netty.bootstrap.ServerBootstrapTest` | FAIL | ⚪ same as above; matches HotSpot exactly (5/6). |
| `io.netty.buffer.AdaptiveBigEndianDirectByteBufTest` | HANG | ✅ fixes 1+2+3 → **415/417 solo in 70 s**, same as HotSpot. Its `testInternalNioBuffer()` only trips JUnit's 120 s per-test cap under shard contention → [throughput](adaptive-bytebuf-allocator-throughput-20260812.md) |
| `io.netty.buffer.AdaptiveBigEndianHeapByteBufTest` | HANG | ✅ **415/417** (2 skipped) — same as HotSpot |
| `io.netty.buffer.AdaptiveByteBufAllocatorGrowthTest` | HANG | ⏱ **400/400 solo** in 829 s vs HotSpot 11 s → [throughput](adaptive-bytebuf-allocator-throughput-20260812.md) |
| `io.netty.buffer.AdaptiveByteBufAllocatorTest` | HANG | ⏱ **126/127, 0 failures, solo** in 500 s vs HotSpot 6 s → [throughput](adaptive-bytebuf-allocator-throughput-20260812.md). ⚠ the 1 skip is [ThreadMXBean](threadmxbean-not-com-sun-extension-20260812.md) |
| `io.netty.buffer.AdaptiveByteBufAllocatorUseCacheForNonEventLoopThreadsTest` | HANG | ⏱ **127/128, 0 failures, solo** in 437 s; same 1 skip as above |
| `io.netty.buffer.AdaptiveLittleEndianDirectByteBufTest` | HANG | ✅ **415/417** — same as HotSpot |
| `io.netty.buffer.AdaptiveLittleEndianHeapByteBufTest` | HANG | ✅ **415/417** — same as HotSpot |
| `io.netty.buffer.AdvancedLeakAwareByteBufTest` | HANG | ✅ **426/426** — same as HotSpot |
| `io.netty.buffer.AdvancedLeakAwareCompositeByteBufTest` | HANG | ✅ ok=497 aborted=9 — **byte-identical to HotSpot** |
| `io.netty.buffer.AlignedPooledByteBufAllocatorTest` | HANG | ⚠ no failures, but runs 28 tests HotSpot skips → [unsafe-property](unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md) |
| `io.netty.buffer.BigEndianCompositeByteBufTest` | HANG | ✅ ok=487 aborted=9 — **byte-identical to HotSpot** |
| `io.netty.buffer.BigEndianDirectByteBufTest` | HANG | ✅ **413/413** — same as HotSpot |
| `io.netty.buffer.BigEndianHeapByteBufTest` | FAIL/HANG | ✅ **414/414** — same as HotSpot |

## Residual pages spun off this triage

* [`adaptive-bytebuf-allocator-throughput-20260812.md`](adaptive-bytebuf-allocator-throughput-20260812.md)
  — the remaining not-green results are wall-clock only, no assertion failure.
  10× broadly across `io.netty.buffer`, ~90× through `AdaptivePoolingAllocator`,
  with an A/B that localises the extra ~9× to CratonVM's `sun.misc.Unsafe`
  natives.
* [`unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md`](unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md)
  — CratonVM pins `sun.misc.unsafe.memory.access=allow`, so netty takes its
  Unsafe fast paths on CratonVM and its safe paths on HotSpot 25. **Read this
  before triaging any other netty batch page**: the two VMs are not running the
  same netty code.
* [`memorysegment-asbytebuffer-unimplemented-20260812.md`](memorysegment-asbytebuffer-unimplemented-20260812.md)
  — `MemorySegment.asByteBuffer()` throws `AbstractMethodError`, which is why
  netty's HotSpot-25-default allocator (`CleanerJava25`, FFM-`Arena`-backed)
  cannot allocate at all on CratonVM, and why the property above cannot simply
  be removed.
* [`cyclicbarrier-native-drops-barrier-action-20260812.md`](cyclicbarrier-native-drops-barrier-action-20260812.md)
  — `new CyclicBarrier(parties, Runnable)` never runs the Runnable. Found in
  the same native; not a batch-01 failure.
* [`threadmxbean-not-com-sun-extension-20260812.md`](threadmxbean-not-com-sun-extension-20260812.md)
  — `ManagementFactory.getThreadMXBean()` / `getOperatingSystemMXBean()` do not
  implement their `com.sun.management` extensions, so feature-detecting callers
  take their fallback path.

## Repro

The Linux runner has no `--gc` / `--hotspot` flags (that is the Windows
harness); select the collector with `-XX:+UseG1GC` and run HotSpot by invoking
`CratonRunner` under `/data/toolchain/jdk-25/bin/java` with the same `-cp`.

**Run one class at a time (`--shards 1`).** This box carries many concurrent
agents; at load ~20 the same class takes 2–7× longer than at load ~3, which is
enough to turn a pass into a HANG or a 120 s per-test JUnit timeout. Every
"HANG" on this page was reproduced as a pass once given a solo run.

```bash
cd apps/netty-suite-runner
printf 'io.netty.buffer.BigEndianHeapByteBufTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 900 \
  --bin <cratonvm> --out /tmp/repro

# HotSpot oracle, same classpath
CP=$(sed -n 2p common.args)
/data/toolchain/jdk-25/bin/java -cp "$CP" -Duser.timezone=UTC \
  -Djunit.jupiter.execution.timeout.default=120s -Dcraton.batch=1 \
  CratonRunner io.netty.buffer.BigEndianHeapByteBufTest
```
