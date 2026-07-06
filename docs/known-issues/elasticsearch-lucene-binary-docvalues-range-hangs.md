# Elasticsearch Lucene binary doc-values range query hangs

Status: open (root causes #1 and #2 FIXED; a narrower residual stall in
`testAllEqual` remains — confirmed NOT JIT-related, reproduces identically
under `--nojit`; strong new evidence points to `ReentrantReadWriteLock`'s
read-lock hold-count bookkeeping, not yet isolated to a specific fix)

Date observed: 2026-07-02
Date updated: 2026-07-05 (fourth update)

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

2. **FIXED** (two commits, both merged to dev — `d6f26b56` on
   `investigate/es-lrucache-rrwl-contention` and a follow-up `96656a8e` on the
   same branch): root-caused the `ReentrantReadWriteLock`/`LRUQueryCache`
   contention from #2 above to a genuine permanent-hang bug, not just
   slowness, requiring two rounds to fully close:

   - **Round 1** (`d6f26b56`): JDK 25's `ReentrantReadWriteLock$Sync` extends
     the newer 64-bit-state `AbstractQueuedLongSynchronizer`, not the classic
     int-state `AbstractQueuedSynchronizer`. CratonVM has a hand-maintained
     JIT skip-list (`vm/src/jit/skip_list.rs`) banning compilation of the
     classic class's `acquire`/`release`/`signalNext`/`ConditionObject`
     methods — allocate-then-putfield register-allocation miscompile,
     corrupting the waiter linked list and losing wakeups permanently.
     `AbstractQueuedLongSynchronizer` is a near-line-for-line port with the
     identical hazard, but being a textually distinct class name matched none
     of the existing entries. Added the missing sibling entries plus
     `ReentrantReadWriteLock$Sync`/`HoldCounter`.
   - **Round 2** (`96656a8e`): round 1 did not actually take effect. An
     unrelated concurrent commit (`a4913d8b`, "disable callee-saved GPR local
     homes") had gated the *entire* `is_known_miscompile` skip-list —
     including round 1's brand-new entries — behind
     `callee_saved_gpr_local_homes_enabled()`, an env var that defaults to
     off on x86_64. That commit's premise (every skip-list entry belonged to
     one now-closed regalloc family) does not hold for this family: a
     16-thread heavy-contention repro reproduces a permanent hang for BOTH
     `ReentrantReadWriteLock` *and plain `ReentrantLock`* with the gate at
     its default value, and `CRATONVM_DBG_JITC` tracing pinned the actually-
     compiled culprits as `AbstractQueued(Long)?Synchronizer$Node`'s
     `getAndUnsetStatus`/`clearStatus` and the synchronizer's own
     `tryInitializeHead` (allocates the CLH queue's sentinel node and
     `casHead`s it in — same hazard, but reachable only once per lock
     instance's first contended acquire, hence invisible to light-contention
     repros) — none of which were ever on the historical list at all, for
     either AQS variant. Fixed with a new, unconditional
     `is_known_miscompile_aqs_family` covering the `Node` class and every
     queue-management helper for both variants.

   Verified after round 2: a 16-thread heavy-contention repro for both
   `ReentrantReadWriteLock` and `ReentrantLock` now completes with the exact
   correct result (was an unconditional, unbounded hang before). `cargo test
   -p cratonvm-jit`/`-p cratonvm-vm` (115 test groups) green on both rounds
   and after merging. `bt18` checksum unchanged (`68332206`).

3. **OPEN, narrower residual — now root-caused to read-lock hold-count
   bookkeeping**: with #1 and #2 (both rounds) fixed and merged, the real
   `LongRandomBinaryDocValuesRangeQueryTests` now makes *substantial,
   concrete* progress — it used to stall permanently on the very first test
   method (`testRandomTiny`); it now runs through most of the class and
   reaches `testAllEqual` before stalling. At that point two `TaskExecutor`
   pool worker threads are permanently parked at the identical bytecode
   offset (bci 368, the `LockSupport.park(this)` call site) inside
   `AbstractQueuedLongSynchronizer.acquire`'s 6-arg overload, contending for
   the same `LRUQueryCache` write lock via `LRUQueryCache.putIfAbsent` →
   `ReentrantReadWriteLock$WriteLock.lock()`. No thread holds/uses the lock
   at dump time — a genuine lost wakeup, not slowness.

   **Re-ran on 2026-07-05 against current `dev` tip** with a direct
   `-Dtests.method=testAllEqual` `org.junit.runner.JUnitCore` invocation and
   a 90s `--stack-dump-on-timeout`: the hang reproduces identically, and
   **reproduces byte-for-byte identically with `--nojit`** (interpreter
   only) — this rules out the JIT/skip-list entirely. One specific
   hypothesis (the `ThreadRegistry::thread_obj_to_park` reverse-index keyed
   by a raw, GC-move-sensitive pointer) was checked and ruled out —
   `update_thread_objs_after_gc` (`vm/src/threading/thread_registry.rs`)
   already re-keys it on every moving GC, wired up at
   `vm/src/memory/gc.rs:568`. Likewise `MonitorTable::cas_locks`
   (`vm/src/threading/monitor.rs`) has an equivalent, well-tested
   `remap_after_gc`. A hypothesis that `Unsafe.compareAndSetReference`'s
   argument-recovery path (`recover_object_arg`,
   `native-builtins/src/lib.rs`) silently coerces a corrupted CAS argument to
   `null` was also checked with temporary diagnostic logging and did **not**
   fire during the repro — ruled out.

   **Breakthrough**: adding temporary `tracing::warn!` calls to `park()`/
   `unpark()` (`vm/src/vm/vm_exec.rs`) to trace the exact call sequence
   changed the race's timing enough that, instead of hanging, the exact same
   repro threw a **real, concrete exception**:
   `java.lang.IllegalMonitorStateException: attempt to unlock read lock, not
   locked by current thread` — thrown by
   `ReentrantReadWriteLock$Sync.tryReleaseShared` (via its private
   `unmatchedUnlockException()` helper) when a thread's read-lock hold count,
   as tracked by the `firstReader`/`firstReaderHoldCount` fast-path fields or
   the `cachedHoldCounter`/`readHolds` (`ThreadLocal<HoldCounter>`) fallback,
   doesn't show a positive count for the releasing thread. `testAllEqual`
   hits the cache via the **read** lock (`LRUQueryCache.get`) on 999 of its
   1000 identical searches (only the first is a miss going through the write
   lock), so this points squarely at read-lock hold-count tracking under
   heavy read contention — a substantially more tractable lead than "silent
   hang, cause unknown". Leading theory: `firstReader`/`firstReaderHoldCount`
   are plain (non-volatile) fields in real JDK source, whose correctness
   relies on a subtle happens-before relationship established by the
   surrounding CAS on `state` — if CratonVM's interpreter/JIT does not
   preserve that exact plain-write-then-CAS ordering relationship for
   cross-thread visibility, this specific fast-path optimization could
   observe stale/wrong counts under contention even though `ThreadLocal`
   and CAS each work correctly in isolation. Not yet fixed — the temporary
   diagnostic tracing was reverted (not committed) after confirming the
   finding; needs a fix targeting that ordering guarantee, then
   reconfirmation that the hang (not just the exception under perturbed
   timing) is resolved.

   `testAllEqual`'s shape: `Arrays.fill()` of `atLeast(1000)` range entries
   with a single random range repeated identically, then `verify()` — every
   leaf search during verification uses an `equals()`-identical query,
   hammering the *same* `LRUQueryCache` entry far more intensely than the
   varied-bounds tests (more contended acquire/release cycles → higher
   chance of hitting the race). Two increasingly targeted standalone
   Lucene-only repros built earlier to isolate this outside the full
   ES/randomizedtesting harness did NOT reproduce it standalone — consistent
   with this being a narrow, heavy-contention-only ordering race rather than
   a structural bug reachable by any read/write mix.

   Separately and NOT believed related (no code in this investigation
   touches GC header-writing, only JIT compile-eligibility): a `bt18` run
   during round-2 verification logged transient `GC: inconsistent header`/
   `mark_young: rejecting object ... corrupt header` warnings that hadn't
   appeared in earlier runs, before the GC's own defensive re-sync path
   recovered and the checksum still matched exactly. Likely from an
   unrelated concurrent commit on `dev`; flagged here in case it resurfaces
   elsewhere, but out of scope for this investigation.

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
