# ES HANG family - vector HNSW bit classes exceed 120s on CratonVM

Status: PARTIALLY RESOLVED — one distinct correctness bug FIXED; the "hang"
itself is reclassified as a genuine-but-finite performance gap, not an
infinite hang/deadlock. Kept OPEN because the residual throughput gap can
still make these classes exceed a 120s suite timeout under host load.

## Summary (2026-07-16 investigation)

Original observation (2026-07-10): `ES815HnswBitVectorsFormatTests` and
`ES93HnswBitVectorsFormatTests` were killed by a 120s local suite timeout
with no Java exception, vs. 3.7s/5.7s HotSpot controls, and it was unclear
whether this was a real hang or just slow.

Investigated end-to-end on `/opt`/Azure Linux host, worktree
`/data/victor-worktrees/es-hnswbit-20260716` (branch
`fix/es-hnswbit-hang-20260716`, merged to `dev` at `0790e7b6`):

**This is NOT an infinite hang.** Both classes were run with the suite's
120s cutoff removed entirely (unbounded `timeout 2700s`, and separately
with `--stack-dump-on-timeout` at various thresholds) and both complete
every time:

- `ES815HnswBitVectorsFormatTests`: `OK (6 tests)`, 58-72s (single run,
  varies with `CRATONVM_JIT_THRESHOLD`) up to 68-72s at defaults.
- `ES93HnswBitVectorsFormatTests`: `OK (7 tests)`, 85-130s depending on
  host CPU contention from other concurrent sessions on this shared box.
- HotSpot/JDK25 control (same seed, same classpath): both classes in
  1.6-2.1s total.

Stack-dump sampling (`--stack-dump-on-timeout`, CratonVM's own frame-chain
dump) at multiple elapsed-time checkpoints shows the worker thread's
deepest frame genuinely advancing through
`HnswGraphBuilder.addGraphNodeInternal` -> `addDiverseNeighbors` ->
`updateNeighbor` -> `NeighborArray.addOutOfOrder`/`addAndEnsureDiversity`
-> `alertOnHeapMemoryUsageChange` (real HNSW graph-build/diversify work
during `testMergeStability`'s repeated `IndexWriter.addDocument`/
`forceMerge` calls), not stuck at a fixed PC. A `CRATONVM_DBG_JITC=1`
trace confirms these specific hot methods do NOT reach CratonVM's JIT
invocation-count threshold (default 500) during this workload's total
call volume, so they run fully interpreted for the whole test; lowering
`CRATONVM_JIT_THRESHOLD` to 20 only shaved ~20% off wall time (72s->58s),
so JIT tier-up is not the dominant factor — this reads as generic
interpreter-throughput overhead on an allocation/comparison-heavy hot
path, roughly 40-90x slower than HotSpot here, not a specific fixable
defect that this investigation could isolate further.

**Residual OPEN item:** this ~40-90x throughput gap is real and unfixed.
In isolation both classes stay comfortably under a 120s per-class
timeout, but the original 2026-07-10 observation (Windows host, `craton`
suite mode, presumably under more contention from parallel shards) did
exceed 120s, and this investigation's own ES93 runs varied 85s-130s
depending on concurrent host load on the shared Azure box — so a 120s
timeout for this specific pair of classes is not comfortably safe under
load and can still reproduce the original symptom. Not independently
root-caused to one fixable hot spot; would need dedicated JIT/interpreter
throughput work (or a raised per-class timeout for known-slow HNSW-bit
classes) to fully close.

## Fixed as part of this investigation: NoClassDefFoundError: java.lang.foreign.MemorySegment

While reproducing the above, `ES815HnswBitVectorsFormatTests.testMultiClose`
was found to genuinely FAIL (not hang) with:

```
java.lang.NoClassDefFoundError: java/lang/foreign/MemorySegment
```

Root cause: `java.lang.foreign.MemorySegment`'s own `<clinit>` builds the
`NULL` constant via `MemorySegment.ofAddress(0)`, before any user code runs
and before any module could have requested native access.
`native-builtins/src/panama.rs`'s `ofAddress` native unconditionally
denied every call via the native-access gate
(`IllegalCallerException: Native access is not enabled for this module`),
so this internal bootstrap call always threw — which, per JVMS 5.5,
permanently poisons the class: every subsequent reference to
`MemorySegment` anywhere in the same JVM then fails with
`NoClassDefFoundError`, regardless of `--enable-native-access`. Real
HotSpot never denies this internal bootstrap call (a zero-address,
zero-length segment can never be dereferenced), so the control run never
hit it.

**Fixed** (commit `58790cb8`, merged to `dev` at `0790e7b6`): `ofAddress`
now exempts address 0 from the native-access gate; every other address is
still denied exactly as before. Verified: both
`ES815HnswBitVectorsFormatTests` (6/6) and `ES93HnswBitVectorsFormatTests`
(7/7) now pass with zero failures (previously 1 failure each via this
bug), `cratonvm-native-builtins` full suite green (2999 passed), and
`cratonvm-vm --lib` shows only the pre-existing, unrelated release-build
residuals (debug-only `#[should_panic]` lock-order assertions and JIT
skip-list feature tests that cannot fire without `debug_assertions`).

## Relationship to other vector docs

- Confirmed distinct from the `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests`
  / `IVFKnnFloatVectorQueryTests` hang family
  (`ES-HANG-20260709-server-org-elasticsearch-search-vectors-*.md`), which is
  a genuine interpreter-level deadlock tied to the GC-audit STW/monitor-race
  finding — this family shows real, continuous forward progress instead, a
  different mechanism entirely. Not touched by this investigation.
- Not the `FloatBuffer` no-Code family or the vector exception/cause-object
  family, per the original doc's own note — still true.

## Evidence

- Repro worktree: `/data/victor-worktrees/es-hnswbit-20260716` (Azure host
  `20.83.144.174`), binary `cratonvm-es-hnswbit-postmerge`.
- ES fixture reused: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`.
- Repro command (seed arbitrary, not the original 2026-07-10 seed which
  was not recorded in that run):
```bash
ES=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch
CP=$(tr -d '\r' < "$ES/server/build/craton-testcp.txt" | tr '\n' ':' | sed 's/:$//')
"$EXE" --java-home /home/victor/jdk25 -Dtests.seed=B17AC9D3E1F2A0C4 -Dtests.asserts=false \
  -Des.path.home="$ES" -Djava.awt.headless=true \
  -cp "$CP" org.junit.runner.JUnitCore org.elasticsearch.index.codec.vectors.ES815HnswBitVectorsFormatTests
```
