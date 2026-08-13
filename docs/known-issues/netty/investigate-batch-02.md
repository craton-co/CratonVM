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
| classes matching HotSpot's per-test result | **14 of 15** at the time of this page; **15 of 15** since 2026-08-13 |
| classes needing a new CratonVM fix | **0** at the time of this page |
| remaining blocker | none. `JfrEventsTest` was the one blocker (JFR streaming gap) and is **fixed** — see the update below |

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
unsafe-memory-access-property-flips-netty-to-unsafe-paths (retired: `unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812`)),
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
| `io.netty.buffer.JfrEventsTest` | FAIL | ✅ since 2026-08-13 **10/10**. Was ❌ 0/10, all 10 hitting the test's own `@Timeout(10)` blocking on a JFR event that was never delivered — see the update at the bottom of this page |
| `io.netty.buffer.LittleEndianCompositeByteBufTest` | HANG | ✅ ok=487 aborted=9 — **byte-identical to HotSpot** |
| `io.netty.buffer.LittleEndianDirectByteBufTest` | HANG | ✅ **412/412** |
| `io.netty.buffer.LittleEndianHeapByteBufTest` | HANG | ✅ **412/412** |
| `io.netty.buffer.LittleEndianUnsafeDirectByteBufTest` | HANG | ✅ **412/412** vs the Unsafe-enabled oracle 412/412 |
| `io.netty.buffer.PooledAlignedBigEndianDirectByteBufTest` | HANG | ✅ **417/417** vs the Unsafe-enabled oracle 417/417 (plain HotSpot starts 0) |
| `io.netty.buffer.PooledBigEndianDirectByteBufTest` | HANG | ✅ **417/417** |
| `io.netty.buffer.PooledBigEndianHeapByteBufTest` | FAIL/HANG | ✅ **417/417** |
| `io.netty.buffer.PooledByteBufAllocatorTest` | HANG | ⚠ 46 ok / 1 abort vs HotSpot 45 ok / 2 abort — no failures either side. HotSpot skips `testArenaMetrics{,No}CacheAlign` (unsafe property (retired: `unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812`)), CratonVM runs them and passes; CratonVM skips `shouldReuseChunks` ([ThreadMXBean](threadmxbean-not-com-sun-extension-20260812.md)) |
| `io.netty.buffer.PooledLittleEndianDirectByteBufTest` | HANG | ✅ **417/417** |
| `io.netty.buffer.PooledLittleEndianHeapByteBufTest` | HANG | ✅ **417/417** |
| `io.netty.buffer.ReadOnlyByteBufTest` | FAIL | ✅ **27/27** — repaired by the batch-07 `StackWalker$Option` fix, as that page predicted |
| `io.netty.buffer.ReadOnlyByteBufferBufTest` | FAIL | ✅ **62/62** |

## Filed from this page

* `jfr-recordingstream-delivers-no-events-20260812.md`
  — the only blocker. `RecordingStream.startAsync()` is a documented no-op, so
  the recording never goes active, `Event.isEnabled()` stays `false`, `commit()`
  returns early, and no consumer is ever called. Reproduced in 60 lines with no
  netty. **Fixed 2026-08-13; the write-up is retired.**
* `jfr-dumped-chunk-is-not-readable-by-the-jdk-parser-20260812.md`
  — independent defect found by the same probe: even an empty `Recording`
  dumps a `.jfr` that the JDK's own `RecordingFile` rejects with
  `IOException: Unknown string encoding 17`. **Fixed 2026-08-13; the write-up is
  retired.**

## Update 2026-08-13 — both JFR pages are closed

`io.netty.buffer.JfrEventsTest` is **10/10 on CratonVM**, matching stock HotSpot
JDK 25, so this batch is 15 of 15. Both JFR write-ups filed from this page are
fixed and retired to `docs/internal/fixed-suite-bugs/`; the durable facts live
in `native-builtins/src/jfr.rs` and `jfr/src/jdk_chunk.rs`.

Six defects, in the order the probes exposed them:

1. `RecordingStream.startAsync()`/`start()` never performed the recording state
   transition, so `Event.isEnabled()` and `shouldCommit()` answered `false` while
   a stream was running and `commit()` returned early.
2. Nothing connected `commit()` to the `Consumer` objects `onEvent(...)`
   registered.
3. `Event.commit()` captured no field values, so a delivered event would have had
   no `capacity` to read. It now walks the event's class chain — which netty's
   events need, since `AllocateChunkEvent` inherits four of its six fields.
4. A second `RecordingStream` in one process got a dead recording, because
   `jfr_begin_java_recording` reused a stopped one. With fixes 1–3 only the
   first test to run passed; this is what took it from 1/10 to 10/10.
5. Every `.jfr` CratonVM wrote used a CratonVM-specific chunk format, not the
   JDK's — the sibling page's defect.
6. Events committed before `Recording.stop()` never reached the dump, because
   the per-thread ring drain runs only for *running* recordings and `stop` did
   not drain.

Two more found by the same probe run and fixed with them: `JVM.getTypeId`
handed out ids inside the JDK's reserved range (so
`EventType.getEventType(myEvent)` answered with an unrelated built-in event
type), and a Java recording leaked its 100 000-event ring for the life of the
process.

Residual divergences from HotSpot, all documented at the code: stream delivery
is synchronous on the committing thread, `start()` does not block, dumped event
types carry no `eventThread`/`stackTrace`, and `Recording.enable(...)` settings
are still not consumed by the Rust recorder (so a Java recording captures every
event the VM emits, not the enabled subset).

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
