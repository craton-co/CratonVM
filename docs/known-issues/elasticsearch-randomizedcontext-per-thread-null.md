# Elasticsearch RandomizedContext per-thread state is null

Status: open (partial fix landed — see "Confirmed root cause #1 (fixed)"; the
exact repro below still fails via a residual cause, not yet root-caused)

Date observed: 2026-07-02
Date partially fixed: 2026-07-02 (branch `fix/es-randomizedcontext-per-thread-null`,
worktree `C:\craton\CratonVM-randctx-perthread`, NOT YET MERGED)

## Summary

Several Elasticsearch randomized tests fail under CratonVM because
`RandomizedContext.getPerThread()` unexpectedly returns null after tests have
already run. HotSpot passes the same classes.

Failure signature:

```text
java.lang.NullPointerException: Cannot read field "randomnesses" because the
return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null
```

This points at thread-local or per-thread state lifetime handling in CratonVM.
`RandomizedContext.perThreadResources` is a real-JDK
`WeakHashMap<Thread, PerThreadResources>`; `getPerThread()` is
`perThreadResources.get(Thread.currentThread())` — so this is fundamentally a
question of why a live thread's own WeakHashMap entry disappears out from
under it.

## Confirmed root cause #1 (FIXED): non-moving young-sweep pointer_map
incompleteness incorrectly clears weak/soft/phantom references to
kept-in-place survivors

**This is a real, general GC correctness bug, independently confirmed and
fixed with regression tests — not specific to RandomizedContext.** It does
not fully explain the residual failure below, but it is a genuine defect
worth fixing regardless.

### Mechanism

CratonVM's generational GC runs a **non-moving** young-gen sweep whenever any
thread is in JIT (`gc_quiescence::is_active()` — true for essentially any
JIT-on workload, including this whole ES suite). Survivors that are not
promoted to old gen this cycle (either PINNED — directly reachable as a root,
e.g. a thread's own `java.lang.Thread` mirror — or simply not yet aged past
`PROMOTION_AGE`, which is the common case for most survivors on any given
cycle) are kept **in place at their original address**. Because nothing
moved, selective promotion's evacuation map records no entry for them.

Post-GC reference processing
(`vm/src/runtime/interpreter.rs::process_references_after_gc`) decides
whether a Weak/Soft/Phantom reference's referent survived via:

```rust
let is_marked = |addr: usize| -> bool {
    pointer_map.contains_key(&addr) || shared.heap.is_addr_live(addr)
};
```

`is_addr_live` for a **young**-gen address always returns `false` (only
old-gen addresses get the "trivially still allocated" pass). So a
kept-in-place young survivor is invisible to *both* disjuncts —
`is_marked` returns `false` even though the object is provably alive (it may
even be a GC root itself), and `process_weak_refs`/`process_soft_refs`
(`gc/src/reference.rs`) incorrectly clears the reference. This is correct
behavior for a *moving* collector (every survivor gets a pointer_map entry
by construction) but wrong for the non-moving sweep.

Confirmed via `CRATONVM_DBG_NOCODE`-style instrumentation
(`CRATONVM_DBG_WATCHREF`, added as part of this fix, kept as a permanent
gated diagnostic): reproduced a `WeakHashMap<Thread,...>` entry for a still
very-much-alive, still-running thread getting cleared purely because its
mirror happened to be a young, not-yet-promoted survivor at the moment a
non-moving sweep ran.

### Fix

- `gc/src/gc_quiescence.rs`: new thread-local "watched referent" set
  (`set_watched_referents` / `is_watched_referent`), mirroring the existing
  `PINNED_JIT_ROOTS` pattern — a side channel for the VM to tell the GC crate
  "these addresses currently back a live Weak/Soft/Phantom reference" without
  threading a new parameter through the whole `GarbageCollector` trait.
- `vm/src/runtime/interpreter.rs::weakref_null_referents_pre_gc`: publishes
  the current weak+phantom referent addresses to the watch-list immediately
  before every collection (unconditionally, including an empty list, so a
  stale watch-list can never leak into the next cycle).
- `gc/src/gen_heap.rs::sweep_young_non_moving`: for every kept-in-place
  survivor, if its address is currently watched, records an **identity**
  `pointer_map` entry (`addr -> addr`) — enough for `is_marked` to recognize
  it as having survived. Bounded by the number of live Weak/Soft/Phantom
  references VM-wide, not by the size of the young generation, so this does
  not reintroduce the O(live-set) cost selective promotion exists to avoid
  (see the module's own bt18 tuning comments).
- `gc/src/old_gen.rs::OldGen::compact()`: the analogous gap exists for a live
  old-gen object that happens not to move during sliding compaction
  ("objects that stay in place are NOT included in the map" was already an
  explicit, deliberate part of the design) — same watch-list check, same
  identity-entry fix. NOTE: in practice this path is largely redundant with
  `is_addr_live`'s coarse "any address within old-gen's allocated bounds is
  live" region check for the *specific* `is_marked` consumer — it is real,
  general hardening for every *other* pointer_map consumer (`update_after_gc`
  remapping, JNI global-ref remap, thread-registry remap, etc.) that needs to
  know whether an object moved, not just whether it's "in old gen somewhere".
- `gc/src/reference.rs::process_weak_refs`: added a `CRATONVM_DBG_WATCHREF`
  trace of every KEEP/CLEAR decision (referent address + outcome), kept as a
  permanent gated diagnostic for this class of bug.

### Regression tests (all passing — `cargo test --release -p cratonvm-gc`:
748 passed, 0 failed)

- `gc/src/gen_heap.rs::tests::non_moving_sweep_records_identity_map_for_watched_survivor`
  — a watched, kept-in-place young survivor gets an identity `pointer_map`
  entry; an unwatched one does not (bounded-cost check).
- `gc/src/gen_heap.rs::tests::non_moving_sweep_when_jit_active` (pre-existing)
  — still passes unchanged: an unwatched non-moving sweep still returns an
  empty `pointer_map`, confirming the fix does not start recording every
  survivor.
- `gc/src/old_gen.rs::tests::compact_records_identity_map_for_watched_stationary_survivor`
  — same shape, for the old-gen compactor.

## Residual: the exact repro below still fails — root cause NOT yet found

With both fixes applied, `MappingStatsTests` (see Repro) still fails
**deterministically** with the identical NPE signature, now reported as a
JUnit **class-level** failure (`1) org.elasticsearch.action.admin.cluster.stats.MappingStatsTests`,
not a specific `@Test` method) after all 14 test methods pass — i.e. during
`RandomizedRunner`/`ThreadLeakControl` teardown, on whichever thread runs
that teardown.

**`CRATONVM_DBG_WATCHREF` evidence rules out an incorrect clear-decision as
the cause of this specific failure**: in the final relevant GC cycle(s)
before the crash, every `[watchref] weak CLEAR`/`"was DEAD"` event
cross-references to an address that was independently confirmed dead
elsewhere in the same trace — none correlate to an object that should have
survived. The fix above is doing its job; something else is producing this
NPE.

A **separate, non-deterministic** failure was also observed in some (not
all) runs: `testConcurrentSerialization` fails mid-test via

```text
java.util.concurrent.ExecutionException: java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null
```

— i.e. the identical NPE, but thrown from inside a worker thread's task
(`Future.get()` unwrapping it), not from suite teardown. This did not
reproduce on every run with the same seed, consistent with a genuine timing
race rather than the deterministic pointer_map gap fixed above.

### Leads for further investigation (not yet pursued)

- `com.carrotsearch.randomizedtesting.ThreadLeakControl.forkTimeoutingTask`
  appeared in a class-init stack trace during this investigation — each test
  method's `Statement` chain (and possibly the whole suite) may run on a
  **forked** thread, distinct from the thread JUnitCore started on. If the
  suite/teardown thread spends most of its life **blocked** (`Future.get()`,
  `join()`) waiting on these forked/worker threads, its own `java_thread_obj`
  root depends on the blocking-native root-snapshot deposit path
  (`NativeContextImpl::deposit_root_snapshot`, `vm/src/vm/vm_exec.rs`) — the
  exact mechanism [[reference_thread_mirror_snapshot_root]] fixed for a
  different symptom (Tomcat `TestDigestAuthenticator`, commit 34f9f68b /
  merge b109248e). That fix should already cover this, but has not been
  independently re-verified for the RandomizedContext/ThreadLeakControl
  threading shape specifically — worth confirming rather than assuming.
- The non-deterministic `testConcurrentSerialization` worker-thread failure
  suggests a **newly-spawned** thread mid-task, not a long-lived one — worth
  checking whether a just-`Thread.start()`ed worker can observe a transient
  state (e.g. the pre-GC referent-null / post-GC restore window in
  `weakref_null_referents_pre_gc` / `process_references_after_gc`) if it is
  not yet fully participating in the STW barrier when GC begins.
- `RandomizedContext$PerThreadResources` entries are seeded via a
  `cloneFor(Thread)`-shaped method (confirmed via `javap` disassembly of
  `com.carrotsearch.randomizedtesting.RandomizedContext.class` from
  `randomizedtesting-runner-2.8.2.jar`) called from the *parent* thread
  before a worker starts — `getPerThread()`/`push()` themselves have **no**
  lazy-creation fallback, so a null return always means either (a) the entry
  was cleared after being legitimately created, or (b) `cloneFor` was never
  called for that thread in the first place. Worth confirming which.

### Related symptom: duplicate `createTempDir()` paths → node-lock cascade

After the JDK-NIO `AbstractMethodError`s were fixed (see
`docs/internal/elasticsearch-jdk-nio-no-code-attribute.md`),
`InternalEngineFieldInfoCachingTests` and `NoOpEngineTests` still fail
deterministically (no stale lock files involved) with:

```text
java.lang.IllegalStateException: failed to obtain node locks, tried [X, X]
Caused by: org.apache.lucene.store.LockObtainFailedException: Lock held by this virtual machine
```

`ESTestCase.tmpPaths()` calls `createTempDir()` 1-3 times
(`TestUtil.nextInt(random(), 1, 3)`) to build `path.data`; in this run it
returned the SAME path twice instead of two distinct temp directories, so
`NodeEnvironment` tries to lock the same physical directory twice in one
process. `createTempDir()`'s naming is per-thread/RandomizedContext-scoped
state, so this is very likely the same underlying per-thread-state defect
tracked by this doc, manifesting as silent name collision rather than an
outright NPE. Not yet re-verified against the fixes above — flagging here
rather than opening a duplicate doc.

## Current full-suite result (pre-fix baseline)

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300` found 6 CratonVM-only failures with this signature.

Representative row:

```text
index=334
module=server
class=org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
CratonVM=FAIL, 58.011s
HotSpot=PASS, 15.924s
```

Other examples:

```text
org.elasticsearch.index.codec.vectors.es93.ES93BinaryQuantizedVectorsFormatTests
org.elasticsearch.index.codec.vectors.es816.ES816HnswBinaryQuantizedVectorsFormatTests
org.elasticsearch.index.codec.vectors.es94.ES94ScalarQuantizedVectorsFormatTests
org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests
org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests
```

NOTE: the class-list index above drifted between runs (gradle/filesystem test
discovery order is not guaranteed stable) — `MappingStatsTests` was at index
314, not 334, by the time of the fix/verification work on 2026-07-02. Target
the class by name via a direct `JUnitCore` invocation (see Repro) rather than
relying on `-Start <index>` matching across runs.

## Repro

Direct invocation (index-independent, used throughout the 2026-07-02
investigation):

```powershell
$cp = Get-Content C:\craton\CratonVM\apps\elasticsearch\server\build\craton-testcp.txt -Raw
& <cratonvm.exe> --java-home "C:\Program Files\Java\jdk-25" --stack-dump-on-timeout 0 --Xmx 2g `
  -Dtests.seed=B17AC9D3E1F2A0C4 -Djava.awt.headless=true -Djna.nosys=true `
  -Dtests.logger.level=WARN -Dio.netty.noUnsafe=true -Dtests.testfeatures.enabled=true `
  -Dtests.security.manager=false -Dtests.asserts=false -Dtests.timeoutSuite=580000! `
  --add-opens=java.base/java.util=ALL-UNNAMED --add-opens=java.base/java.lang=ALL-UNNAMED `
  --add-opens=java.base/java.security.cert=ALL-UNNAMED --add-opens=java.base/java.nio.channels=ALL-UNNAMED `
  --add-opens=java.base/java.nio=ALL-UNNAMED --add-opens=java.base/java.net=ALL-UNNAMED `
  --add-opens=java.base/javax.net.ssl=ALL-UNNAMED --add-opens=java.base/java.nio.file=ALL-UNNAMED `
  --add-opens=java.base/java.time=ALL-UNNAMED --add-opens=java.management/java.lang.management=ALL-UNNAMED `
  --add-opens=java.base/jdk.internal.misc=ALL-UNNAMED --enable-native-access=ALL-UNNAMED `
  --add-modules=jdk.incubator.vector `
  -cp $cp org.junit.runner.JUnitCore org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
```

Add `CRATONVM_DBG_WATCHREF=1` (env var) to trace every non-moving-sweep
survivor decision and every Weak/Soft/Phantom KEEP/CLEAR decision.

Suite-runner form (index may need re-resolving — see NOTE above):

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 314 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-randomizedcontext-perthread-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir <workdir> `
  -Exe <cratonvm.exe>
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.admin.cluster.stats.MappingStatsTests.out.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
```

Fix work (2026-07-02), worktree `C:\craton\CratonVM-randctx-perthread`,
branch `fix/es-randomizedcontext-per-thread-null` (NOT merged):

```text
docs/internal/repros/randomizedcontext-perthread-null/README.md
```
