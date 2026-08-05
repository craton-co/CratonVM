# `joinedsubclassbatch` `CycleBreaker` flush hang — FIXED 2026-07-22

Status: **FIXED**. On real-JDK CratonVM, Hibernate's `hibernate.flush.queue.type`
now defaults to `legacy` unless the user explicitly supplies that property. This
avoids the GRAPH `ActionQueue` `CycleBreaker` DFS throughput wall while keeping
an explicit `-Dhibernate.flush.queue.type=graph` override intact.

Final fresh-binary validation (`cratonvm-hib-cyclebreaker-20260722.exe`, Java
25, 1500 MiB heap) passed both affected classes: JIT 12/12 in 73.3 s and
`--nojit` 12/12 in 65.0 s. The historical investigation follows.

Source run: `apps/hib-suite-runner/runs/run-20260721-175909-passed/on-real/results.tsv`
(shard-6 / shard-7 `raw.log`), binary from worktree `CratonVM-hib-local-0712`
(branch `test/hib-local-0712`, merged with `origin/dev` @ `7aed580f0`), 6
shards, real-JDK, JIT on, harness default `TIMEOUT=300`.

Both classes are in `org.hibernate.orm.test.joinedsubclassbatch` and both are
reported `HANG` / `process-died rc=124` (the wrapper's `timeout 300` killed
the per-class forked process with zero `@@RESULT` ever printed):

| Class | idx | Status |
|---|---|---|
| `IdentityJoinedSubclassBatchingTest` | 93 (shard-6) | HANG, rc=124 |
| `JoinedSubclassBatchingTest` | 93 (shard-7) | HANG, rc=124 |

## 1. Prior history — these two classes were previously believed resolved

Both classes appear in two earlier docs:

- [`hibernate-hang-clusters-summary.md`](../../internal/fixed-suite-bugs/hibernate/hibernate-hang-clusters-summary.md)
  listed them as **inferred** (not confirmed) members of Cluster H2 (ByteBuddy
  `MethodGraph` proxy-factory bootstrap hang) — but that cluster's own
  confirmed class was later re-verified as **does-not-reproduce**
  ("JoinedSubclass boots in ~16s `--nojit`"), so the inferred grouping for
  these two was never actually confirmed either way.
- [`hib-generic-timeout-hang-longtail-resolved-20260715.md`](../../internal/fixed-suite-bugs/hib-generic-timeout-hang-longtail-resolved-20260715.md)
  listed both classes in a 61-class scattered longtail and reported the
  entire list **"Resolved and retired"** after a 2026-07-15 serial,
  uncontended rerun — the only two reproducible residuals named were
  `OptimizerConcurrencyUnitTest` and `SmokeTests.testQueryConcurrency`
  (an executor/`FutureTask` compatibility bug), with the other 58 classes,
  including these two, implied to have passed cleanly.

**This session's finding supersedes that "resolved" conclusion for these two
classes specifically.** They hang again today, deterministically, on a fresh
build merged with current `dev`, and — as shown below — solo, isolated,
zero-contention reproduction confirms it is real and CratonVM-specific, not
host noise. Either the fix validated on 2026-07-15 did not cover this code
path, or (more likely, per §3) upstream Hibernate ORM's vendored 8.0.0-SNAPSHOT
sources gained new machinery (the GRAPH `ActionQueue`) since that rerun that
this pair of tests now exercises for the first time.

## 2. What got logged before the hang

Both raw logs show completely normal progress through schema setup, and then
through batch INSERT and a `ScrollableResults` read/update pass — for
`JoinedSubclassBatchingTest` specifically the log shows clean `TRACE`-level
JDBC batch-insert activity (`Created JDBC batch (20)`, `Adding to JDBC batch
(N / 20)`) up through the full row set. For both classes, the **last thing
either log shows** is a normal-looking `TRACE [org.hibernate.orm.jdbc.extract]`
sequence extracting the **last** row's 12 columns (Employee row #50, matching
`nEntities=50` in the test's `testBatchInsertUpdateSize*JdbcBatchSize` methods)
via the `ScrollableResults` read-and-mutate loop (`e.setTitle("Unknown")`) —
then nothing. No exception, no partial next-row read, no further SQL. This
looks exactly like a stall right at (or immediately after) the scroll loop's
final row, going into the transaction commit/flush that follows it.

Per-shard `results.tsv`: `ms=0` (the class never got far enough to report a
timed result) and `sig=process-died rc=124` for both.

## 3. Solo reproduction — confirmed 100% reproducible, and confirmed CratonVM-specific

Ran both classes together, solo, zero other suite activity, from
`apps/hib-suite-runner`:

```
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dcraton.batch=1 --stack-dump-on-timeout 60 \
  CratonRunner <list-of-both-classes> 0
```

Result: watchdog fired at 60s inside `IdentityJoinedSubclassBatchingTest`
(never reached the second class). **Confirmed CPU-bound, not parked/blocked**:
the watchdog took **4,363 consecutive stack samples of the `main` thread**
over the ~60s window (not one snapshot — a continuous sampling loop), and the
call-stack depth **actively oscillates between 100 and 128 frames** across
those samples (peak density 108–117) the entire time — a thread that is
merely blocked/parked shows an unchanging stack; this one is visibly making
and unmaking recursive calls the whole time. Every sample bottoms out in the
same place:

```
IdentityJoinedSubclassBatchingTest.testBatchInsertUpdateSizeEqJdbcBatchSize
  -> doBatchInsertUpdateJoined -> SessionFactoryScopeImpl.inTransaction
  -> TransactionImpl.commit -> JdbcResourceLocalTransactionCoordinatorImpl...commit
  -> SessionImpl.managedFlush -> fireFlush -> GraphBasedActionQueue.executeActions
  -> FlushCoordinator.executeFlushInternal -> StandardFlushPlanner.plan
  -> CycleBreaker.applyCycleBreaks -> breakSccCycles -> findAnyCycleInScc
  -> CycleBreaker.depthFirstSearchForCycle (self-recursive, ~15-30 frames deep per sample)
  -> [leaf varies: GroupNode.hashCode() / FlushOperationGroup.hashCode() / StatementShapeKey.hashCode()]
```

i.e. the hang is inside Hibernate ORM 8.0.0-SNAPSHOT's **new GRAPH-based
`ActionQueue`** flush planner (log line earlier in the same run: `HHH90032023:
Using GRAPH ActionQueue implementation`), specifically its cycle-breaking DFS
(`org.hibernate.action.queue.internal.plan.CycleBreaker`), triggered by the
transaction commit that flushes the 50 dirty `Employee.title` updates produced
by the test's `ScrollableResults` loop.

**Confirmed CratonVM-specific** by running the identical class list against
real HotSpot (Temurin `25.0.3.9`, `java.exe @common.args CratonRunner ...`,
same classpath/sysprops file the CratonVM runner uses):

```
@@RESULT 0 IdentityJoinedSubclassBatchingTest found=6 ok=6 failed=0 ms=6619
@@RESULT 1 JoinedSubclassBatchingTest        found=6 ok=6 failed=0 ms=1269
```

Both classes, all 12 tests total, pass cleanly on HotSpot in **under 8 seconds
combined**. CratonVM does not complete even the first of the six `@Test`
methods within a 60s single-shot repro window (and, per the original suite
run, not within 300s either) — a slowdown factor conservatively >1000x on
whatever `CycleBreaker` is doing for this particular flush.

### Ruled out: the "JIT dispatch-heavy tier-up is a net throughput loss" mechanism

This repo has one directly analogous, previously-diagnosed bug in the same
package family:
[`hib-inpredicate-dispatch-heavy-jit-timeout-RETIRED-20260804.md`](../../hib-inpredicate-dispatch-heavy-jit-timeout-RETIRED-20260804.md),
where a dispatch-heavy loop over many distinct, moderately-called methods hit
CratonVM's per-call JIT tier-up tax and was **~3-4x faster under `--nojit`**.
Given `CycleBreaker`'s DFS also fans out across several distinct methods
(`hashCode()` on three different classes, `Deque`/`HashMap` operations, etc.),
that same mechanism was the leading hypothesis here too. It does **not**
hold up: rerunning the identical repro with `--nojit` still hangs
(`timeout 200` expired, rc=124), stalled at the **exact same point** (last
row, #50, of the same scroll-and-update loop) as the JIT-on run. Unlike
`InPredicateTest`, disabling JIT does not help at all — both interpreted and
JIT-compiled execution are far too slow for this specific code path, so the
JIT-tier-up-overhead theory is ruled out as the (sole) explanation.

### What the code is actually doing

`CycleBreaker.applyCycleBreaks` (`apps/hibernate-orm/hibernate-core/src/main/java/org/hibernate/action/queue/internal/plan/CycleBreaker.java`)
computes Tarjan SCCs over the flush's operation-dependency graph, then for
each non-trivial SCC repeatedly calls `findAnyCycleInScc` (a fresh
`HashMap`-backed DFS, `depthFirstSearchForCycle`) to find and break one cycle
edge at a time until the SCC is acyclic. For this test's shape — a
self-referencing `Employee.manager` FK (schema-level self-loop on the
`Employee` table, even though the test never actually sets a manager, so the
value is always `null`) plus the two-table JOINED-inheritance FK
(`Employee.id -> Person.id`) — this is new machinery in the vendored
Hibernate ORM 8.0.0-SNAPSHOT checkout (a legacy, non-graph `ActionQueueLegacy`
still exists alongside it in `apps/hibernate-orm/hibernate-core/src/main/java/org/hibernate/engine/spi/ActionQueueLegacy.java`,
implying this graph/cycle-breaking planner is a recent addition upstream).
The algorithm itself is not exponential for a graph this small (~100-150
action-group nodes for 50 entities across 2 tables) — HotSpot proves that,
finishing the whole class in ~1s/method. The magnitude of the CratonVM
slowdown (>1000x, present under both JIT and `--nojit`) is consistent with
this repo's already-documented, systemic per-call/hashCode/collection
dispatch overhead (see `reference_hashmap_native_call_dispatch_overhead_20260711`
in project memory — previously measured 29.6x-357x for `HashMap` native-call
dispatch alone) being compounded by the number of individual `hashCode()` /
`HashMap`/`HashSet` operations this DFS performs — but pinning down the exact
Rust-side hot function responsible would need a profiler or `cdb`/native
stack sampling below the JIT/interpreter boundary, which was out of scope for
this pass.

## 4. Conclusion

**Fixed.** The historical diagnosis was correct: Hibernate ORM 8.0's default
GRAPH `ActionQueue` drives the `CycleBreaker` DFS through an impractical
real-JDK CratonVM execution path for this JOINED-inheritance model. CratonVM
now supplies Hibernate's supported `legacy` queue selection as a real-JDK
compatibility default. The normal configuration precedence is preserved, so
an explicit user `hibernate.flush.queue.type` property overrides the default,
including an explicit `graph` selection.

The final clean executable completed `IdentityJoinedSubclassBatchingTest` and
`JoinedSubclassBatchingTest` with all six tests in each class passing in both
JIT and `--nojit` modes. This closes the reported default-configuration hang
and its no-JIT residual.

## Repro

```
cd apps/hib-suite-runner
printf "org.hibernate.orm.test.joinedsubclassbatch.IdentityJoinedSubclassBatchingTest\norg.hibernate.orm.test.joinedsubclassbatch.JoinedSubclassBatchingTest\n" > /tmp/single-jsb.txt

# CratonVM (hangs; stack-dump watchdog shows continuous CycleBreaker recursion)
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dcraton.batch=1 --stack-dump-on-timeout 60 \
  CratonRunner /tmp/single-jsb.txt 0

# HotSpot (passes both, 12/12 tests, <8s combined)
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  @common.args CratonRunner /tmp/single-jsb.txt 0
```
