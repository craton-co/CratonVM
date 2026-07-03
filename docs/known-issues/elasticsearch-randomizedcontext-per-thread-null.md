# Elasticsearch RandomizedContext per-thread state is null

Status: open (one real GC bug found + fixed, see "Confirmed root cause #1
(fixed)"; the exact repro below still fails via a SEPARATE, JIT-related
residual — narrowed to 4 suspect JIT-compiled `WeakHashMap` methods, see
"Residual: root cause NARROWED to a JIT bug — not GC")

Date observed: 2026-07-02
Date partially fixed / JIT lead identified: 2026-07-02 (branch
`fix/es-randomizedcontext-per-thread-null`, worktree
`C:\craton\CratonVM-randctx-perthread`, NOT YET MERGED)

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

## Residual: root cause NARROWED to a JIT bug — not GC (2026-07-02, later session)

With both GC fixes applied, `MappingStatsTests` (see Repro) still fails
**deterministically** with the identical NPE signature, reported as a JUnit
**class-level** failure (`1) org.elasticsearch.action.admin.cluster.stats.MappingStatsTests`,
not a specific `@Test` method) after all 14 test methods pass.

`javap` disassembly of `RandomizedRunner.class` (from
`randomizedtesting-runner-2.8.2.jar`) confirms the exact threading shape:
`runSuite(RunNotifier)` creates ONE dedicated `RunnerThreadGroup` + suite
thread (`new Thread(threadGroup, runnable, name); .start(); .join();`),
which runs `RandomizedContext.create(...)` (→ `perThreadResources.put(
Thread.currentThread(), new PerThreadResources())`, ONE call, at the very
start), then all 14 test methods (each independently confirmed passing),
then `popAndDestroy()` (→ `getPerThread()` → `perThreadResources.get(
Thread.currentThread())`, the call that returns null) at the very end. The
`put()` and the failing `get()` are on the *same* thread, `Thread.
currentThread()` in between.

### GC is now fully exonerated for this specific failure

1. **No incorrect clear.** `CRATONVM_DBG_WATCHREF` tracing showed every
   `weak CLEAR`/`"was DEAD"` event in a full run correlates only to
   independently-confirmed-dead objects — never to something that should
   have survived.
2. **Thread mirror identity is stable.** New tracing added to
   `vm/src/vm/vm_exec.rs::current_thread_object` (`[watchref] THREAD MIRROR
   IDENTITY CHANGED`, gated by `CRATONVM_DBG_WATCHREF`) logs whenever a given
   OS thread's own `Thread.currentThread()` mirror address or identity hash
   changes between calls. Across a full failing run (20 distinct threads,
   including the suite thread), **zero** changes were observed — every
   thread's own mirror is address- and hash-stable for its entire life. This
   also indirectly confirms `next_hash()` (the monotonic-counter identity
   hash stamped once at allocation in `alloc_object`) is correctly preserved
   across every relocation path checked (moving-young Cheney copy —
   explicit field-by-field preservation at `gen_heap.rs` `forward_object_impl`
   line ~6066; selective-promotion evacuation and old-gen compaction — both
   full-object `std::ptr::copy`/`copy_nonoverlapping`, which preserves the
   header byte-for-byte).
3. **No GC ran during the relevant window, at least in some failing runs.**
   In one fully-traced deterministic failure, `CRATONVM_DBG_WATCHREF` recorded
   exactly ONE young-GC cycle for the entire ~38s / 14-test run, and it
   occurred early — before the suite thread's own watched-referent address
   ever appeared in a published watch-list (i.e., before `RandomizedContext.
   create()`'s `put()` plausibly ran). No GC of any kind (young, concurrent
   old-gen mark-sweep via `maybe_concurrent_gc`, or otherwise) is recorded
   between `put()` and the failing `get()` in this run. A bug that requires a
   GC to fire cannot explain a failure with no GC in the relevant window.

### Confirmed: JIT-only reproduction

Re-running the identical repro with `--nojit` **does not reproduce this NPE
at all** — it hits a different, unrelated `ClassCastException` inside
`testConcurrentSerialization` instead
(`java.lang.Object cannot be cast to org.elasticsearch.common.io.stream.Writeable`,
a distinct pre-existing bug, out of scope here). This is strong, direct
evidence that **JIT compilation is necessary to reproduce the
`RandomizedContext` NPE** — the bug is in JIT-compiled code, not in GC, not
in the interpreter.

`CRATONVM_DBG_DUMP_JIT=LIST` (env var, prints every JIT-compiled method sig)
on a failing run shows exactly four `java/util/WeakHashMap` methods get
JIT-compiled during the run:

```text
java/util/WeakHashMap.maskNull(Ljava/lang/Object;)Ljava/lang/Object;
java/util/WeakHashMap.indexFor(II)I
java/util/WeakHashMap.hash(Ljava/lang/Object;)I
java/util/WeakHashMap.matchesKey(Ljava/util/WeakHashMap$Entry;Ljava/lang/Object;)Z
```

These are precisely the bucket-lookup/key-matching primitives behind
`WeakHashMap.get()`/`getEntry()`. `matchesKey` in particular is the
strongest suspect: it dereferences an `Entry`'s `WeakReference.get()` and
compares against the query key — a JIT codegen bug there (wrong register
compared, a spilled value read after being clobbered, etc.) would produce
exactly this symptom: an entry present in the correct bucket that the
lookup fails to recognize as a match, so `getEntry()`/`get()` returns null
even though the entry was never actually cleared. `matchesKey`'s compiled
body is unusually large for its Java source (979 bytes; dumped via
`CRATONVM_DBG_DUMP_JIT=matchesKey`) with 3-4 near-duplicate code blocks,
consistent with the method being inlined at multiple call sites — not yet
manually verified against expected semantics (needs a proper disassembler
pass, not just hex inspection).

**Ruled out**: the bug is NOT one specific, already-toggleable JIT
optimization pass. Re-running with `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1`
alone, and then with all of `CRATONVM_DISABLE_SCALAR_REPLACEMENT`,
`CRATONVM_DISABLE_AALOAD_LICM`, `CRATONVM_DISABLE_ARITH_LICM`,
`CRATONVM_DISABLE_UNROLL`, and `CRATONVM_NO_IR_BRANCHY` set together, the
NPE still reproduces deterministically. Whatever is wrong is either in base
JIT codegen/register allocation (not a specific optimization pass) or in a
JIT-adjacent subsystem (calling convention into `Reference.get()`, GC-root
interaction with JIT-compiled `WeakHashMap` methods, or similar).

**Isolated repro attempts did NOT reproduce it** (see
`docs/internal/repros/randomizedcontext-perthread-null/whm-repro/` for the
four `.java` files tried): (1) single-threaded, 2M `get()` calls on a stable
key against a 2000-entry map; (2) interleaved growth/resize with 50 `get()`
checks between each of 40000 `put()`s; (3) 8 concurrent threads under a
shared `synchronized` lock, each `put`ting and `get`ting its own key 20000
times plus periodic churn `put`s, then a final check from `main` after
`join`ing; (4) 500 rounds of `new Thread().start().join()` with a `get()`
check on the stable "self" key after each round. None reproduced a single
miss. The real failure needs either the full ES/RandomizedContext workload's
scale (165 total JIT-compiled methods interacting, not just the four
`WeakHashMap` ones in isolation) or a timing window not yet captured by
these simplified repros — worth another isolated-repro attempt that more
closely matches ES's exact allocation/thread-churn pattern, or a proper
disassembly-level review of the four suspect compiled methods.

### Leads for further investigation

- **Primary lead**: disassemble and manually verify
  `WeakHashMap.matchesKey`/`hash`/`indexFor`/`maskNull`'s JIT-compiled output
  against expected semantics (a real x86-64 disassembler, not hex-by-eye).
  Reproduce via `CRATONVM_DBG_DUMP_JIT=matchesKey` (or `hash`/`indexFor`/
  `maskNull`) against the Repro command below.
- The blocked-thread root-snapshot mechanism (`reference_thread_mirror_
  snapshot_root` in memory — the Tomcat `TestDigestAuthenticator` fix,
  commit 34f9f68b / merge b109248e) was a leading theory earlier in this
  investigation but is now directly ruled out by the "Thread mirror identity
  is stable" evidence above (zero identity changes observed across every
  thread in a full failing run) — do not re-open it without new evidence.
- The non-deterministic `testConcurrentSerialization` worker-thread failure
  (mid-test `ExecutionException` wrapping the identical NPE, seen in some but
  not all runs) is presumably the same JIT bug hitting a different thread's
  entry at a different point — not independently re-investigated after the
  JIT-vs-nojit finding above.
- A tighter isolated repro: try scaling up the isolated `WhmRepro*.java`
  tests' total JIT-compiled-method count (e.g. by adding unrelated hot loops
  elsewhere in the same process) to see if register/codegen pressure from
  *other* JIT-compiled code is a necessary ingredient, not just
  `WeakHashMap`'s own methods in isolation.

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
