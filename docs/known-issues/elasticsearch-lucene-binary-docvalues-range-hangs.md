# Elasticsearch Lucene binary doc-values range query hangs

Status: open (root causes #1 and #2 FIXED; a third, unidentified blocker remains)

Date observed: 2026-07-02
Date updated: 2026-07-04 (later same day)

## Summary

Two Lucene binary doc-values range query tests hang under CratonVM until the
suite runner kills the process at the requested 300-second timeout. HotSpot
passes the same classes.

Root-caused to **two independent bugs** stacked in the same test run:

1. **FIXED** (branch `fix/es-binary-docvalues-range-hang`, merged to dev):
   CratonVM's JIT permanently blacklisted ANY method containing an
   `invokedynamic` instruction ANYWHERE in its bytecode, even on a dead code
   path — the classic case being `assert cond : "msg" + var;`, whose message
   string-concatenation compiles to `invokedynamic` behind a
   `$assertionsDisabled` guard. `org.apache.lucene.util.fst.NodeHash.add`,
   `NodeHash$PagedGrowableHash.nodesEqual`, and
   `FSTCompiler$UnCompiledNode.addArc`/`.replaceLast` — hot, per-FST-node
   methods exercised heavily while merging the term dictionary for
   `int_range_dv_field`/`long_range_dv_field` — all contain such an assert and
   were permanently stuck in the interpreter, a ~100-300x slowdown that alone
   was enough to blow the suite's 300s timeout for the Integer/Long range
   variants (Integer and Long share `RangeType.LONG`'s encode/query path,
   which produces far more distinct FST nodes than Float/Double, explaining
   why only these two classes hung). Fixed by making the JIT scanner accept
   `invokedynamic` and lowering it to an unconditional jump to the existing
   "uncommon trap" deopt stub (`DeoptReason::UnreachedCode`) instead of
   vetoing the whole method — see the commit on this branch for the full
   design and the standalone `FSTCompiler`/`NodeHash` stress repro used to
   isolate it without needing the ES harness.

2. **FIXED** (branch `investigate/es-lrucache-rrwl-contention`, merged to
   dev): root-caused the `ReentrantReadWriteLock`/`LRUQueryCache` contention
   from #2 above to a genuine permanent-hang bug, not just slowness. JDK 25's
   `ReentrantReadWriteLock$Sync` extends the newer 64-bit-state
   `AbstractQueuedLongSynchronizer`, not the classic int-state
   `AbstractQueuedSynchronizer`. CratonVM already has a hand-maintained JIT
   skip-list (`vm/src/jit/skip_list.rs`) banning compilation of the classic
   class's `acquire`/`release`/`signalNext`/`ConditionObject` methods — they
   allocate a fresh `ExclusiveNode`/`ConditionNode` and immediately `putfield`
   `prev`/`next`/`waiter` into it, a known allocate-then-putfield
   register-allocation miscompile that corrupts the waiter linked list's
   next-pointer, permanently losing wakeups. `AbstractQueuedLongSynchronizer`
   is a near-line-for-line port with the identical hazard in its own
   identically-shaped node types, but being a textually distinct class name it
   matched none of the existing (bare `(class, method)` tuple, no inheritance)
   skip-list entries, so its hot path stayed JIT-eligible and hit the same
   miscompile. Confirmed and fixed via a minimal, Lucene-free 4-thread
   `ReentrantReadWriteLock` stress repro that went from an unbounded, CPU-idle
   hang (proving parked threads, not a spin/livelock) to completing correctly
   in ~10-12s once the sibling skip-list entries were added.

3. **OPEN, now the sole blocker**: with both #1 and #2 fixed (and both
   individually verified — `cargo test -p cratonvm-jit`/`-p cratonvm-vm`
   green, `bt18` checksum unchanged, and the standalone RRWL stress repro now
   passing), the actual `Integer`/`LongRandomBinaryDocValuesRangeQueryTests`
   classes **still do not complete** within a generous window (tested up to
   480s; HotSpot passes in ~15s). Process CPU-time sampling during a run shows
   real work happening for the first ~30-40s (JIT warmup, early random
   iterations) followed by the process going essentially idle (near-zero CPU
   growth) for the remainder — the same "parked with no wakeup" signature as
   bug #2, but occurring later in the run and in a path not yet identified.
   The stack dump at timeout still shows contention on
   `LRUQueryCache.putIfAbsent` → `ReentrantReadWriteLock$WriteLock.lock()` →
   `AbstractQueuedLongSynchronizer.acquire`, which IS now interpreter-only
   (per fix #2) — so this is either (a) a third, similar
   allocate-then-putfield-class miscompile in a *different* synchronization
   primitive somewhere in Lucene's concurrent `IndexSearcher`/`TaskExecutor`/
   `LRUQueryCache` machinery that hasn't been identified yet, or (b) severe
   but finite cumulative slowdown from forcing `AbstractQueuedLongSynchronizer`
   permanently into the interpreter under heavy concurrent contention (many
   leaf searches all serializing through one now-interpreted lock). Not yet
   distinguished — needs either a longer soak run to see if it eventually
   completes, or a fresh stack-dump-based investigation of whatever hot path
   is active during the still-time-CPU window right before the stall.

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 2 CratonVM-only `HANG` rows in this family.

Representative row:

```text
index=2163
module=server
class=org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests
CratonVM=HANG, 300.200s
HotSpot=PASS, 15.336s
```

Affected classes:

```text
org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests
org.elasticsearch.lucene.queries.LongRandomBinaryDocValuesRangeQueryTests
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 2163 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-binary-docvalues-range-hang-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```
