# `action.queue` GRAPH-default tests — blocked by the flush planner's throughput, not by a config bug

**Status: OPEN.** One of the two root causes is FIXED (record `hashCode`/`equals`
ran interpreter-only — see §3); the remaining blocker is that essentially
nothing in this workload gets JIT-compiled at all (§4). Until that is addressed,
real-JDK CratonVM keeps `hibernate.flush.queue.type=legacy` as a compatibility
default and upstream's GRAPH-default tests cannot pass.

**This doc replaces an earlier WON'T-FIX version whose root cause was wrong and
whose impact was understated by 8x.** Both corrections are below. The
[superseded analysis](../../internal/fixed-suite-bugs/hibernate/joinedsubclassbatch-cyclebreaker-flush-hang-20260721-FIXED.md)
attributed the hang to the `CycleBreaker` DFS and to "systemic
per-call/hashCode/collection dispatch overhead", explicitly noting the
responsible function was never pinned down.

## 1. Impact — 19 classes, not 2

The old doc listed two failures:

| Class | Failure |
|---|---|
| `org.hibernate.orm.test.action.queue.ActionQueueDefaultTest` | `AssertionFailedError: expected: <GRAPH> but was: <LEGACY>` |
| `org.hibernate.orm.test.action.queue.proof.InsertOrderingReferenceSeveralDifferentSubclassTest` | `AssertionFailedError` (SQL batch shape/order) |

The same `results.tsv`
(`apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/`) also reports
**17 further `action.queue.*` classes as ABORTED**, every one for this same
reason — they self-abort rather than fail:

```java
if ( sfi.getActionQueueFactory().getConfiguredQueueType() != QueueType.GRAPH ) {
    Assumptions.abort("Skipping GRAPH test with non-GRAPH queue type");
}
```

`BatchSizeExceedingTest`, `NewEntityOrderColumnTest`, `UniqueConstraintOrderingTest`,
`callback.Post{Delete,Insert,Update}HandlingTest`,
`decomposer.{Delete,Insert,Update}DecomposerTest`, `decomposer.DirtyOptLockDebugTest`,
`integration.{DecomposerGraphPlanner,DeleteCascade,IdentityGeneration,InsertWithAssociations,MixedOperations,OptimisticLocking}*`.

So the `legacy` default is not costing 2 test failures — it is disabling
**19 of the 25 `action.queue` classes**, 17 of them silently. HotSpot runs all
27 classes in the family (including `joinedsubclassbatch`) green: 202 tests,
0 failed, 0 aborted.

## 2. Why the default cannot simply be removed

Verified individually with the GRAPH default in place:

* The aborted classes do pass once GRAPH is active —
  `integration.MixedOperationsTest` was checked directly: **9/9 ok in 15.9 s**.
* But `joinedsubclassbatch.{Identity,}JoinedSubclassBatchingTest` **hang**
  (>900 s each, on an idle box; HotSpot: 7.2 s and 6.5 s).

Removing the default therefore trades 19 gated classes for 2 hanging ones. A
hang costs a 300 s harness timeout per class and is worse than a skip, so the
default stays until §4 is fixed.

## 3. Root cause 1 — record `hashCode`/`equals` ran interpreter-only (FIXED)

Under the stack-dump watchdog, **3351 of 3352 samples** of the hung thread have
a record `hashCode`/`equals` as their deepest frame:

| leaf frame | samples |
|---|---|
| `internal/graph/GroupNode.hashCode` | 1004 |
| `spi/StatementShapeKey.hashCode` | 1003 |
| `internal/plan/FlushOperationGroup.hashCode` | 1003 |
| `internal/plan/FlushOperationGroup.equals` | 170 |
| `internal/graph/GroupNode.equals` | 170 |
| `CycleBreaker.depthFirstSearchForCycle` | 1 |

All three of `GroupNode`, `FlushOperationGroup` and `StatementShapeKey` are
`record`s, and the planner keys its dependency graph on them. A record's
`hashCode`/`equals` body is a bare `invokedynamic` against
`java.lang.runtime.ObjectMethods.bootstrap`, and `jit/src/x64.rs` (the `0xba`
arm of `jit_scan`) lowers `invokedynamic` to an **unconditional deopt** — so
those bodies never ran compiled. Measured from a JIT-compiled monomorphic
caller:

| call | CratonVM (before) | HotSpot |
|---|---|---|
| `Plain.hashCode()` — hand-written, inlined | 8.1 ns | ~1 ns |
| `Object.hashCode()` — existing intrinsic | 225 ns | — |
| `record P1(int).hashCode()` | **1576 ns** | **0.9 ns** |

The hand-written equivalent was 190x faster than the generated one on the same
VM, so this was the `invokedynamic` ban, not "records are slow".

**Fixed** by `InterpIntrinsic::{RecordHashCode, RecordEquals}` plus native
component fast paths (nested record, `String`, enum, `ArrayList`) — see
`native-builtins/src/intrinsics/record.rs`. Against the real Hibernate record
shape:

| op | before | after | HotSpot |
|---|---|---|---|
| `Node.hashCode` | 12606 ns | 3661 ns | 39.8 ns |
| `Node.equals` | 9966 ns | 3424 ns | 11.0 ns |
| `HashSet.contains` | 27698 ns | 5684 ns | 48.7 ns |
| DFS replica, per edge | 19.46 µs | 9.91 µs | 0.13 µs |

3-5x, and the record methods are no longer the dominant term. Not enough on its
own.

## 4. Root cause 2 — the workload is essentially never JIT-compiled (OPEN)

`CRATONVM_DBG_JIT_METHOD_STATS=1` on the hung class (iteration-capped so the
process exits and the summary prints):

```
1463 distinct methods tracked, 1456 ever invoked, 4760505 total invocations
still-interpreted=1369  c1=29  c2=65
compiles: c1=95 c2=104 osr=50 deopts=3  total_compile_time_ms=6
hot_but_stuck_in_interpreter=1362
```

**1362 of 1463 hot methods never compile.** The top offenders are the DFS's own
trivial accessors:

| method | invocations | state |
|---|---|---|
| `GraphEdge.isBroken()Z` | 515,572 | stuck, `tier_fail_count=3` |
| `GraphEdge.getTo()` | 442,420 | stuck, `tier_fail_count=3` |
| `GroupNode.stableId()J` | 36,084 | stuck, `tier_fail_count=3` |
| `Graph.outgoing()` | 28,596 | stuck, `tier_fail_count=3` |
| `FlushOperationGroup.operations()` | 20,852 | stuck, `tier_fail_count=3` |

`isBroken()` is `aload_0; getfield; ireturn` — 5 bytes, called half a million
times, and it never compiles. `tier_fail_count=3` means the compile task
returned `success=false` three times and hit `MAX_TIER_FAIL_RETRIES`, which bans
the method permanently (`jit/src/tiered.rs:503`). Only 199 compiles happened in
the whole process, totalling 6 ms — so the failure is systematic, not
per-method.

That is why the planner is still ~2400x off HotSpot after §3:

| measure | CratonVM | HotSpot | ratio |
|---|---|---|---|
| per DFS edge | 194 µs | 0.0796 µs | 2440x |
| per `HashSet.contains` | 257 µs | 0.109 µs | 2352x |
| `TarjanScc` (same records) | 704 ms | 4 ms | 176x |

Work volume matches HotSpot exactly (1488 vs 1516 `contains` per break
iteration, 1966 vs 2078 edges per iteration), so this is pure constant factor,
not algorithmic divergence. The same wholesale interpretation appears in the
passing control class (`MixedOperationsTest`: 164 of 196 methods stuck), so it
is not specific to the hanging class — it is the general ceiling, and this
workload is simply the one that cannot absorb it.

**This is the open blocker, and it is bigger than this doc.** It belongs with
[`tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md`](../tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md),
which tracks the same family (JIT admission bans, `invokedynamic` lowered to a
trap). Next step for whoever picks this up: find why `complete_task` reports
`success=false` for a 5-byte getter — that one answer probably unlocks a large
fraction of every suite, not just this one.

## 5. Hypotheses tested and REFUTED

Recorded so they are not re-run:

* **Identity-keyed DFS collections.** Swapping `CycleBreaker`'s two DFS-local
  collections for `IdentityHashMap`-backed ones (semantics-preserving here, since
  the nodes are the graph's own instances and `extractCycle` already compares
  them with `==`) gave identical counts and only ~2x — the record hash was not
  the DFS's collection use.
* **Record hash unstable across GC.** A hash that changed after a collection
  would break every `HashMap` lookup and would explain the `equals` share. It
  does not: hashes and 200-key map/set lookups survive 4 rounds of forced GC
  plus allocation churn, on both binaries.
* **Identity-hash distribution.** CratonVM's identity hashes are dense
  sequential integers (`89, 92, 94, …`) where HotSpot's are spread
  (`622488023, …`), so poor bucket distribution looked plausible. Measured, it
  is a non-issue: `31*h + x` spreads even dense inputs, and CratonVM's buckets
  come out marginally *better* than HotSpot's (avg probes 1.30 vs 1.34, max
  chain 3 both). Do not "fix" identity hashing for this.
* **Megamorphic inline-cache thrash** at the shared `HashMap.hash()` call site:
  7175 ns monomorphic vs 7034 ns megamorphic — no effect.
* **Host load.** The 900 s timeout reproduces on an idle box.

## 6. Repro

```bash
cd apps/hib-suite-runner
printf "org.hibernate.orm.test.action.queue.ActionQueueDefaultTest\n" > /tmp/aq1.txt

# Fails on the default (LEGACY); passes with the property forced to graph.
"<worktree>/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner /tmp/aq1.txt 0

# The two classes that block removing the default (>900 s each with graph):
printf "org.hibernate.orm.test.joinedsubclassbatch.IdentityJoinedSubclassBatchingTest\n" > /tmp/jsb1.txt
"<worktree>/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dhibernate.flush.queue.type=graph -Dcraton.batch=1 \
  --stack-dump-on-timeout 240 CratonRunner /tmp/jsb1.txt 0
```

Record semantics for the §3 fix are covered by
`vm/tests/intrinsic_diff.rs::record_object_methods_differential` over
`vm/tests/resources/cratonvm/IntrinsicRecordDiff.java` — 368 observations,
identical with intrinsics ON and with `CRATONVM_DISABLE_INTRINSICS=1`.

## 7. The LEGACY default also costs throughput (measured on dev, 2026-07-28)

Kept from the concurrent measurement another session added to this doc, because
it raises the value of removing the default and must not be lost: on an
insert-heavy fixture the LEGACY queue is also substantially *slower* than the
GRAPH default it replaces — the opposite of what the `CycleBreaker`-hang
rationale would suggest.

Measured on dev `d0a6c7987`, quiet host, one fresh process per sample,
`OracleInlineMutationStrategyIdTest#testDeleteFromPerson` (its `@BeforeEach`
persists 2200 JOINED-inheritance entities = 4400 INSERTs). Three samples per
configuration, interleaved round-robin so host drift cannot favour one side:

| configuration | samples (ms) | mean |
|---|---|---:|
| CratonVM default (LEGACY) | 255 999 / 241 995 / 216 814 | 238 269 |
| `-Dhibernate.flush.queue.type=graph` | 168 208 / 185 606 / 159 862 | **171 225 (−28 %)** |

The sample sets do not overlap, well clear of the ±8 % run-to-run spread. Full
context:
[`h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md),
residual section.

So the default currently pays three times: 19 gated classes, −28 % on
insert-heavy flushes, and a permanent divergence from upstream. It still cannot
be removed today — §2 — but the ledger against it is larger than the original
doc implied.

## Closure (2026-07-29)

**FIXED.** CratonVM no longer injects the `legacy` queue setting, restoring
Hibernate's upstream GRAPH default. The JIT code-buffer estimator now leaves
enough room for real graph-planner call shapes, and the interpreter serves the
verifier-exact `aload_0; getfield; return` accessor shape without a frame when
dispatch, linkage, redefinition, volatility, and JVMTI observation permit it.

Validated with the real JDK 25 Hibernate fixture and the uniquely built
`cratonvm-actionqueue-graph-20260729-019faddd.exe`: all 27 scoped classes and
202 tests passed in both JIT and `--nojit` modes (`found=started=ok`, zero
failed, zero aborted, `@@BATCHEND failed_classes=0`). In the full no-JIT sweep,
the original blockers passed too: `IdentityJoinedSubclassBatchingTest` 6/6 in
99,515 ms and `JoinedSubclassBatchingTest` 6/6 in 123,100 ms.
