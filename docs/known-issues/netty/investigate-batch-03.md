# netty — investigate batch 03 of 13

**Status: TRIAGED 2026-08-12. 14 of 15 classes match HotSpot; the 15th was
root-caused to a VM-wide `java.util.ArrayList` cost and is fixed, though it
still needs a solo run to clear JUnit's own per-test cap.**

Part of a 184-class FAIL/HANG list split across 13 pages (see
[investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page
owns exactly the 15 classes below — do not touch classes listed in other batch
pages.

Originally found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite
run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`).
"status seen" is what that run recorded; "explained by" is the 2026-08-12
triage on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`95a156234`.

## Outcome

Nine of the ten `io.netty.buffer` classes and all five `io.netty.channel`
classes matched HotSpot with **no change needed** — the batch-01 and batch-07
fixes already on `dev` had cleared them.

The one real defect was not a netty problem at all. `SearchProcessorTest`'s
Aho-Corasick case builds a 256-entry-per-node `ArrayList<Integer>` trie —
**6.05 M `ArrayList` native calls** — and each of those cost ~1 µs, where the
*same method body written in plain Java* costs 27 ns on the same VM. Fixed in
`docs/internal/fixed-suite-bugs/arraylist-slot-layout-rederived-per-call-FIXED-20260812.md`:
the receiver's slot layout was re-derived per call through two string-keyed
class-manager lookups and a hierarchy walk, and is now memoized per `ClassId`.

| | before | after |
| --- | --- | --- |
| `ArrayList.size()` | 1014 ns/op | **337 ns/op** |
| `ArrayList.get()` | 1107 ns/op | **428 ns/op** |
| Aho-Corasick trie build, 2016 factories | 217 s | **118 s** (HotSpot 0.21 s) |
| `SearchProcessorTest` solo | 255 s, 14/15 | **106–111 s, 15/15** |

**It still fails under the suite's 3-way sharding** (135 s vs JUnit's 120 s
per-test cap). The remaining ~12× over plain Java is structural and filed as
[arraylist-native-overhead-and-the-view-carrier-class](arraylist-native-overhead-and-the-view-carrier-class-20260812.md).

## Classes

Legend: ✅ matches HotSpot · ⚠ differs only in what is skipped · ⏱ passes solo,
fails under shard contention

| class | status seen | explained by |
|---|---|---|
| `io.netty.buffer.ReadOnlyDirectByteBufferBufTest` | FAIL | ✅ **60/60** |
| `io.netty.buffer.ReadOnlyUnsafeDirectByteBufferBufTest` | FAIL | ✅ **60/60** vs the Unsafe-enabled HotSpot oracle 60/60 (plain HotSpot starts 0 of 57 — see the unsafe property (retired: `unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812`) page for why, and use `--sun-misc-unsafe-memory-access=allow` on the oracle) |
| `io.netty.buffer.RetainedDuplicatedByteBufTest` | FAIL/HANG | ✅ **416/416** |
| `io.netty.buffer.RetainedSlicedByteBufTest` | FAIL | ✅ 410 ok / 6 aborted — **byte-identical to HotSpot** |
| `io.netty.buffer.SimpleLeakAwareByteBufTest` | HANG | ✅ **425/425** |
| `io.netty.buffer.SimpleLeakAwareCompositeByteBufTest` | HANG | ✅ 497 ok / 9 aborted — **byte-identical to HotSpot** |
| `io.netty.buffer.SizeClassedChunkCacheTest` | FAIL | ✅ **30/30** |
| `io.netty.buffer.SlicedByteBufTest` | FAIL | ✅ 410 ok / 6 aborted — **byte-identical to HotSpot** |
| `io.netty.buffer.WrappedCompositeByteBufTest` | HANG | ✅ 487 ok / 9 aborted — **byte-identical to HotSpot** |
| `io.netty.buffer.search.SearchProcessorTest` | HANG | ⏱ **15/15 solo** after the ArrayList fix (was 14/15); still fails at 3 shards → [ArrayList overhead](arraylist-native-overhead-and-the-view-carrier-class-20260812.md) |
| `io.netty.channel.AbstractChannelTest` | FAIL | ✅ 5 ok / 1 skipped — same as HotSpot |
| `io.netty.channel.AdaptiveRecvByteBufAllocatorTest` | FAIL | ✅ **9/9** |
| `io.netty.channel.ChannelInitializerTest` | FAIL | ✅ **9/9 — better than HotSpot**, which fails 1 of 9 here. That failure is a netty test-isolation flake, not a VM difference: two tests bind the same `LocalAddress("addr")` and one loses with `ChannelException: address already in use`. Re-run twice on HotSpot and the *victim changes* (`lastHandler…` then `firstHandler…`) |
| `io.netty.channel.DelegatingChannelPromiseNotifierTest` | FAIL | ✅ **1/1** |
| `io.netty.channel.ManualIoEventLoopTest` | FAIL | ✅ **17/17** |

## Filed from this page

* `docs/internal/fixed-suite-bugs/arraylist-slot-layout-rederived-per-call-FIXED-20260812.md`
  — the fix, with the full measurement chain (probe → stack sampler → native
  census → scaling probe → plain-Java control).
* [`arraylist-native-overhead-and-the-view-carrier-class-20260812.md`](arraylist-native-overhead-and-the-view-carrier-class-20260812.md)
  — the residual. CratonVM returns `map.values()` as an object whose class **is
  exactly `java.util.ArrayList`**, which both misreports `getClass()` and forces
  every ArrayList call to run a view-discrimination chain. Includes why the
  obvious exact-class fast path is *not* safe today.

## Repro

Run one class at a time (`--shards 1`) — see the INDEX note on wall caps; this
page has a concrete case where 3-way sharding alone changes the verdict.

```bash
cd apps/netty-suite-runner
printf 'io.netty.buffer.search.SearchProcessorTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 900 \
  --bin <cratonvm> --out /tmp/repro

# HotSpot oracle, same classpath
CP=$(sed -n 2p common.args)
/data/toolchain/jdk-25/bin/java -cp "$CP" -Duser.timezone=UTC \
  -Djunit.jupiter.execution.timeout.default=120s -Dcraton.batch=1 \
  CratonRunner io.netty.buffer.search.SearchProcessorTest
```
