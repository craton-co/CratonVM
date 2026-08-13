# netty — investigate batch 02 of 13

**Status: TRIAGED 2026-08-12. 14 of 15 classes match HotSpot; the 15th is
blocked by a known JFR feature gap.** No CratonVM fix was needed on this page —
its assertion failures were already cleared by the batch-01 and batch-07 fixes
that landed on `dev` first.

Part of a 184-class FAIL/HANG list split across 13 pages (see
[investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page
owns exactly the 15 classes below — do not touch classes listed in other batch
pages.

Originally found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite
run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`).
"status seen" is what that run recorded; "explained by" is the 2026-08-12
triage on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`6d1bfd531` — i.e. carrying the batch-01 `ByteArrayInputStream`/zero-length-copy
fixes and the batch-07 `StackWalker$Option` fix. Re-measured on `e042d699f` after
the 34-lane jdk-only campaign landed (it touches `native-io` and
`util_concurrent_ext` heavily): **every count below is unchanged**, and both JFR
probes still reproduce.

## Outcome

| | |
| --- | --- |
| classes matching HotSpot's per-test result | **14 of 15** |
| classes needing a new CratonVM fix | **0** |
| remaining blocker | `JfrEventsTest` — [JFR streaming gap](jfr-recordingstream-delivers-no-events-20260812.md) |

Ten of these fifteen were `HANG` in the original run and are now clean passes
in 11–173 s. That is the pattern the INDEX now warns about: the 180 s cap plus
the `io.netty.buffer` throughput gap, not a deadlock.

### Three of these classes do not run at all on stock HotSpot

`BigEndianUnsafeDirectByteBufTest` and `LittleEndianUnsafeDirectByteBufTest`
gate every test on `assumeTrue(PlatformDependent.hasUnsafe())`;
`PooledAlignedBigEndianDirectByteBufTest` gates its `@BeforeAll` on
`assumeTrue(PooledByteBufAllocator.isDirectMemoryCacheAlignmentSupported())`.
On HotSpot JDK 25 both predicates are false, so the plain baseline reports
`aborted=413`, `aborted=412`, and `started=0` — no oracle at all. On CratonVM
they are true (see
[unsafe-memory-access-property-flips-netty-to-unsafe-paths](unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md)),
so ~1240 tests execute here that the baseline never ran.

**Get a real oracle by enabling Unsafe on HotSpot**, which puts netty on the
same code path CratonVM takes:

```bash
java --sun-misc-unsafe-memory-access=allow -cp "$CP" CratonRunner <class>
```

With that, HotSpot runs all three and they pass 413/413, 412/412, 417/417 —
and CratonVM matches all three exactly. Without it these classes would have
looked untestable.

## Classes

Legend: ✅ matches HotSpot · ⚠ differs only in what is skipped (cause already
filed) · ❌ real blocker

| class | status seen | explained by |
|---|---|---|
| `io.netty.buffer.BigEndianUnsafeDirectByteBufTest` | HANG | ✅ **413/413** vs the Unsafe-enabled HotSpot oracle 413/413 (plain HotSpot skips all 413) |
| `io.netty.buffer.DuplicatedByteBufTest` | FAIL/HANG | ✅ **416/416** |
| `io.netty.buffer.JfrEventsTest` | FAIL | ❌ **0/10**, all 10 hit the test's own `@Timeout(10)` blocking on a JFR event that is never delivered → [JFR streaming gap](jfr-recordingstream-delivers-no-events-20260812.md) |
| `io.netty.buffer.LittleEndianCompositeByteBufTest` | HANG | ✅ ok=487 aborted=9 — **byte-identical to HotSpot** |
| `io.netty.buffer.LittleEndianDirectByteBufTest` | HANG | ✅ **412/412** |
| `io.netty.buffer.LittleEndianHeapByteBufTest` | HANG | ✅ **412/412** |
| `io.netty.buffer.LittleEndianUnsafeDirectByteBufTest` | HANG | ✅ **412/412** vs the Unsafe-enabled oracle 412/412 |
| `io.netty.buffer.PooledAlignedBigEndianDirectByteBufTest` | HANG | ✅ **417/417** vs the Unsafe-enabled oracle 417/417 (plain HotSpot starts 0) |
| `io.netty.buffer.PooledBigEndianDirectByteBufTest` | HANG | ✅ **417/417** |
| `io.netty.buffer.PooledBigEndianHeapByteBufTest` | FAIL/HANG | ✅ **417/417** |
| `io.netty.buffer.PooledByteBufAllocatorTest` | HANG | ⚠ 46 ok / 1 abort vs HotSpot 45 ok / 2 abort — no failures either side. HotSpot skips `testArenaMetrics{,No}CacheAlign` ([unsafe property](unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md)), CratonVM runs them and passes; CratonVM skips `shouldReuseChunks` ([ThreadMXBean](../../internal/fixed-bugs/mxbean-not-com-sun-extension-FIXED-20260813.md) — FIXED 2026-08-13) |
| `io.netty.buffer.PooledLittleEndianDirectByteBufTest` | HANG | ✅ **417/417** |
| `io.netty.buffer.PooledLittleEndianHeapByteBufTest` | HANG | ✅ **417/417** |
| `io.netty.buffer.ReadOnlyByteBufTest` | FAIL | ✅ **27/27** — repaired by the batch-07 `StackWalker$Option` fix, as that page predicted |
| `io.netty.buffer.ReadOnlyByteBufferBufTest` | FAIL | ✅ **62/62** |

## Filed from this page

* [`jfr-recordingstream-delivers-no-events-20260812.md`](jfr-recordingstream-delivers-no-events-20260812.md)
  — the only blocker. `RecordingStream.startAsync()` is a documented no-op, so
  the recording never goes active, `Event.isEnabled()` stays `false`, `commit()`
  returns early, and no consumer is ever called. Reproduced in 60 lines with no
  netty.
* [`jfr-dumped-chunk-is-not-readable-by-the-jdk-parser-20260812.md`](jfr-dumped-chunk-is-not-readable-by-the-jdk-parser-20260812.md)
  — independent defect found by the same probe: even an empty `Recording`
  dumps a `.jfr` that the JDK's own `RecordingFile` rejects with
  `IOException: Unknown string encoding 17`.

## Repro

The Linux runner has no `--gc` / `--hotspot` flags (that is the Windows
harness); select the collector with `-XX:+UseG1GC` and run HotSpot by invoking
`CratonRunner` under `/data/toolchain/jdk-25/bin/java` with the same `-cp`.
Prefer `--shards 1` — see the INDEX note on wall caps.

```bash
cd apps/netty-suite-runner
printf 'io.netty.buffer.JfrEventsTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 900 \
  --bin <cratonvm> --out /tmp/repro

# HotSpot oracle, same classpath; add --sun-misc-unsafe-memory-access=allow
# for the three Unsafe/aligned classes or HotSpot will skip them entirely
CP=$(sed -n 2p common.args)
/data/toolchain/jdk-25/bin/java -cp "$CP" -Duser.timezone=UTC \
  -Djunit.jupiter.execution.timeout.default=120s -Dcraton.batch=1 \
  CratonRunner io.netty.buffer.JfrEventsTest
```
