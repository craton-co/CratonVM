# `TestDefaultInstanceManager.testClassUnloading` — off-by-one recurrence (FIXED)

**Status:** FIXED and retired on 2026-07-27 — and **defect 1's guard was
silently disarmed the very next day**. `67de5400a` (07-28) flipped
`DEFAULT_MOVING_YOUNG` to `true`, which made
`young_marker_follows_side_tables()` return `false` unconditionally on the
shipped default, so the young-mirror deferral below stopped engaging at all. The
test recurred deterministically on 07-31 and was re-closed on 08-01: see
[defaultinstancemanager-third-recurrence-FIXED.md](defaultinstancemanager-third-recurrence-FIXED.md).
Everything below remains an accurate account of the two defects and both fixes
are still load-bearing; only the claim that defect 1's guard *engages* had an
expiry date on it. Supersedes
[defaultinstancemanager-classunloading-count-mismatch-FIXED.md](defaultinstancemanager-classunloading-count-mismatch-FIXED.md),
whose 2026-07-14 fix was real but **incomplete** — see "Why the first fix
looked sufficient" below. Retires
`docs/known-issues/tomcat/26-defaultinstancemanager-classunload-offbyone.md`.

## Symptom

```
java.lang.AssertionError: expected:<8> but was:<9>
	at org.apache.catalina.core.TestDefaultInstanceManager.testClassUnloading(TestDefaultInstanceManager.java:66)
```

Deterministic on the local Windows full-suite runs (`fullsuite-local-20260728`,
`rerun1500-craton-20260728`, `rerun1500-craton-v2-20260728`: all FAIL at
22–26s); HotSpot PASSes the same fixture in 7.7s.

The test loads three JSPs with `maxLoadedJsps=2`, forces one `System.gc()`, and
polls `DefaultInstanceManager.backgroundProcess()` 10× expecting the evicted
first JSP's weak key to clear so the annotation cache returns to its
pre-third-JSP size.

## Not a Tomcat-side difference

An instrumented clone of the test (`InstMgrProbe`, dumping the annotation-cache
keys plus the JSP LRU dequeue reflectively) showed **every Java-observable
Tomcat structure identical to HotSpot**:

```
after-jsp3  jspCount=2  unloadCount=1  uris=[/bug36923.jsp, /bug5nnnn/bug51544.jsp]
after-jsp3  entry#0 content=/bug5nnnn/bug51544.jsp  replaced=null
after-jsp3  entry#1 content=/bug36923.jsp           replaced=null
```

`annotations.jsp`'s `JspServletWrapper` is evicted, unlinked, and its
`replaced` back-reference cleared. The retained entry is
`org.apache.jsp.annotations_jsp` (loader `JasperLoader@…`), and it stays
non-null through all 10 polls — so the divergence is entirely in what CratonVM's
collector considers reachable.

## Root cause — TWO independent defects, both required

Established by making the collector report rather than by inference (see
"Diagnostic method"): the retaining chain is

```
parked worker's root_snapshot
  -> jit_hashmap_string_node_cache entry (map, node)      [defect 2]
     -> JDT LookupEnvironment/TypeSystem binding graph
        -> JDTCompiler -> JspCompilationContext -> JasperLoader
  -> loader_pin: live instance keeps its defining loader alive
  -> mirror_pin: live loader keeps its mirrors alive
     -> org/apache/jsp/annotations_jsp Class mirror        [defect 1]
```

### Defect 1 — a YOUNG class mirror was rooted unconditionally

`roots.rs` step 6 gated its "defer this mirror to `mirror_pin` instead of
rooting it" decision on `VmHeap::metadata_pin_deferrable`, which under the
Generational backend is **old-gen-only**. That restriction is correct for
`metadata_pin`, whose only Generational consumer is `old_gen_gc`'s BFS — but
`mirror_pin` has a *second* consumer the metadata case cannot rely on:
`gen_heap::mark_young_precise_object` follows it as an ordinary marking edge.

Reusing the stricter predicate meant **any young mirror was rooted
unconditionally**. Since a mirror's `classLoader` field is a real heap edge back
to its defining loader, that permanently defeats class unloading for any loader
whose classes' mirrors have not been promoted yet — and here the explicit
`System.gc()` is the run's *first and only* collection (exactly one
`reconcile_class_mirrors` pass in the whole log), so nothing had ever been
promoted.

Fix: new `VmHeap::mirror_pin_deferrable`, which additionally defers a young
mirror when the cycle is certain to take the non-moving young marker, via
`gc_quiescence::young_marker_follows_side_tables()`. That predicate mirrors
`collect_garbage_inner`'s `divert_non_moving` decision but only in its *certain*
direction (`CRATONVM_MOVING_YOUNG` and `CRATONVM_DBG_FORCE_MOVING` veto it), so
it errs strictly toward `false`: a false negative costs one extra conservative
root, a false positive would drop a live one. Opt out with
`CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER=1`.

### Defect 2 — a parked thread's JIT memo caches pinned its last working set

`JvmThread::jit_hashmap_string_node_cache` and `string_case_cache` are
per-thread JIT/interpreter fast-path memos. `deposit_root_snapshot_inner`
publishes both into the thread's `root_snapshot`, which is the **only** view a
cross-thread collector has of a parked thread — so every entry is a GC root for
the entire blocked window.

Entries are evicted only when a *lookup* finds a `modCount`/key mismatch, or by
FIFO eviction at 32 entries. An **idle** pooled thread performs no lookups, so
its last working set is pinned for as long as it stays parked. Measured: the
parked Tomcat worker `http-nio-127.0.0.1-auto-1-exec-1`, whose published stack
is 12 frames of pure pool-parking code (`TaskQueue.take` → `AQS$ConditionNode.block`)
and contains no JSP-compilation frames at all, still held an 83-entry snapshot
whose indices 62–63 were a `(java/util/HashMap, node)` pair from the JSP compile.

Fix: `deposit_root_snapshot()` clears both memos before building the blocking
snapshot. Safe unconditionally — both are pure memos that re-validate on every
lookup and recompute on a miss — and clearing *before* the snapshot is built
means the entries are neither published as roots nor left as stale addresses to
be read after the park.

This is a general retention bug, not something specific to this test: any idle
pool thread could pin an arbitrary object graph (a whole webapp's compiler
state, here) indefinitely.

## Why the first fix looked sufficient

The 2026-07-14 fix was verified on Azure Linux with a bare `JUnitCore`
invocation. Whether defect 1 bites depends on **whether the mirror had been
promoted before the `System.gc()`** — a heap-sizing/GC-count accident, not a
stable property. Under the suite's `-Xmx2g` on this Windows fixture the mirror
is still young; with `--nojit` it happens to be old-gen already. That is also
why the recurrence looked platform-specific when it is not.

## Both fixes are necessary — measured matrix

| mirror young-defer | memo-cache clear | JIT | result |
|---|---|---|---|
| ON | ON | on | PASS |
| ON | ON | off | PASS |
| **OFF** | ON | on | **FAIL (9)** |
| OFF | ON | off | PASS |
| ON | **OFF** | on | **FAIL (9)** |

Neither is sufficient alone. With `--nojit` the mirror is old-gen and therefore
already deferred, so defect 2's fix alone suffices there — which is exactly why
an early `--nojit` A/B wrongly appeared to exonerate defect 1.

## Verification

Local Windows fixture (`apps/tomcat`), release binary
`cratonvm-definstmgr-20260727.exe`, suite-runner env
(`CRATONVM_REAL_NET_SOCKETS/REAL_AQS/DISABLE_DEFAULT_WATCHDOG/ROOTSNAP_CACHE=1`,
`-Xmx2g`, real JDK):

```text
org.junit.runner.JUnitCore org.apache.catalina.core.TestDefaultInstanceManager
OK (1 test)
```

3/3 consecutive JIT-on runs PASS (21.3s / 17.3s / 18.4s), plus JIT-off PASS. The
probe confirms the intended liveness contract: the evicted
`org/apache/jsp/annotations_jsp` key clears on the FIRST poll (`size=8`, exactly
as HotSpot), while `bug36923_jsp` and `bug51544_jsp` stay live.

## Diagnostic method (generalize this)

Two hypotheses were discarded before the real cause, both from inferring
causality off a *cyclic* object graph instead of measuring it. What finally
worked, in order:

1. **Instrument the JAVA side first** to separate "the runtime under test
   behaves differently" from "the collector retains differently". Dumping the
   annotation-cache keys and the JSP LRU dequeue proved Tomcat identical to
   HotSpot and confined the bug to reachability.
2. **Terminate the referrer BFS at the ACTUAL root vector, not at zero-referrer
   nodes.** `VmHeap::retention_paths` (added here, alongside the existing
   `find_referrers`/`find_instances_of_class`) walks the full referrer closure in
   one heap walk and reports chains headed either by a real root-set member or by
   a zero-referrer node. Inside a strongly-connected component the zero-referrer
   variant alone always lands back on an arbitrary member — circular and
   useless.
3. **Capture the COMPLETE root vector.** `collect_roots`' own output is not it:
   the multithreaded `System.gc()` path appends peer threads' cached root
   snapshots and cross-thread conservative JIT roots afterwards. Answering
   "is this a root?" from the initiator's partial set reports a misleading `no`.
4. **Attribute each root to the source that pushed it** (the ~24 registered
   native root sources, `collect_roots`-core, peer snapshots, xt-JIT roots), then
   **attribute a snapshot hit to the publishing section** (frames / native pins /
   alloc pool / JNI locals / memo caches / conservative JIT scan). Those have
   entirely different fixes; guessing between them from an object's class name is
   what produced the wrong hypotheses.

Also note: `CRATONVM_ROOTSNAP_CACHE=0` does **not** disable peer root snapshots
— it only disables the frozen-frame *caching* inside `update_root_snapshot`.
`collect_all_root_snapshots()` still roots every thread's deposit, so a null
result from that flag exonerates nothing.
