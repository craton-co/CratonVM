# RandomizedContext per-thread-null: fix evidence (2026-07-02)

Supporting evidence for
`docs/known-issues/elasticsearch-randomizedcontext-per-thread-null.md`. Raw
logs are not committed (repo-wide `*.log` gitignore, and they are mostly
noise) — the excerpts below are the parts that mattered.

## Repro command

See the "Repro" section of the known-issues doc. Direct `JUnitCore`
invocation of `org.elasticsearch.action.admin.cluster.stats.MappingStatsTests`
with `-Dtests.seed=B17AC9D3E1F2A0C4`.

## Pre-fix: deterministic single suite-level failure

```text
Time: 64.088
There was 1 failure:
1) org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null

FAILURES!!!
Tests run: 14,  Failures: 1
```

All 14 individual `@Test` methods pass; the failure is reported against the
*class* (JUnit's convention for a `@BeforeClass`/`@AfterClass`/rule-teardown
failure), after the last method (`testEqualsAndHashcode`) completes.

## Post pointer_map-completeness fix: same failure persists, plus an
occasional second one

Re-running the identical command against the fixed build reproduces the same
class-level failure **every time**. On some (not all) runs with the same
seed, a **second, non-deterministic** failure also appears mid-suite:

```text
E.[testSourceModes] before test
...
Time: 89.295
There were 2 failures:
1) testConcurrentSerialization(org.elasticsearch.action.admin.cluster.stats.MappingStatsTests)
java.util.concurrent.ExecutionException: java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null
Caused by: java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null
2) org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
java.lang.NullPointerException: Cannot read field "randomnesses" because the return value of "com.carrotsearch.randomizedtesting.RandomizedContext.getPerThread()" is null

FAILURES!!!
Tests run: 14,  Failures: 2
```

The `ExecutionException` wraps a `Future.get()` inside
`testConcurrentSerialization` (the test explicitly exercises concurrency —
multiple worker threads), so this NPE fires on a worker thread mid-task, not
during teardown. This is the identical exception text as the deterministic
class-level failure, but a structurally different manifestation (mid-test,
non-deterministic, worker thread vs. teardown, deterministic, unidentified
thread).

## `CRATONVM_DBG_WATCHREF` trace analysis

Ran with `CRATONVM_DBG_WATCHREF=1`, which traces (a) every
`sweep_young_non_moving` invocation, (b) every watched-referent
identity-map insertion / dead-detection in the non-moving sweep, and (c)
every `process_weak_refs` KEEP/CLEAR decision.

Representative output from a run with the 2-failure (non-deterministic)
outcome:

```text
[watchref] publishing 605 watched referent(s): [1e9b2eb0, 1e97a6a0, ...]
[watchref] sweep_young_non_moving ENTRY (non-moving path taken)
[watchref] non-moving sweep: watched survivor kept in place @0x1e97a6a0 — identity-mapped
...
[watchref] non-moving sweep: watched address @0x1ecae4d0 was DEAD (unmarked) — zeroing
...
[watchref] weak CLEAR ref_obj=0x1ecaec08 referent=0x1ecae4d0
```

605 active Weak/Phantom references is the VM-wide count (JDK-internal
caches, reflection metadata, etc.) — not specific to RandomizedContext.

**Cross-referencing every `weak CLEAR` in the trace against every `"was
DEAD"` report**: every single cleared referent address was independently
reported dead by the sweep earlier in the same run (`comm -23` over the
sorted address sets produced zero unmatched entries). In other words, in
this run, `process_weak_refs` never cleared anything that wasn't already
confirmed genuinely dead — the pointer_map-completeness fix is working
correctly for what it targets.

This means: **the residual failure is not an "incorrectly cleared while
still alive" bug of the class this fix addresses.** Something else — not yet
identified — is producing the same NPE.

## Session 2 (2026-07-02, later): GC exonerated, narrowed to a JIT bug

Follow-up investigation after the pointer_map-completeness fix above did not
resolve the deterministic `MappingStatsTests` failure. Summary of the
evidence chain (full detail in the known-issues doc's "Residual: root cause
NARROWED to a JIT bug" section):

1. Added `[watchref] THREAD MIRROR IDENTITY CHANGED` tracing to
   `current_thread_object` — zero identity changes across 20 threads in a
   full failing run. Rules out mirror-address or identity-hash instability.
2. In the same trace, only ONE young GC ran in the entire ~38s/14-test run,
   and it happened before the suite thread's watched-referent address ever
   appeared in a published watch-list — i.e. before `RandomizedContext.
   create()` plausibly ran. No GC occurred in the relevant window at all in
   this run. GC cannot explain a failure with no GC present.
3. **`--nojit` does not reproduce the NPE** — it hits an unrelated
   `ClassCastException` in `testConcurrentSerialization` instead. This is the
   key finding: JIT compilation is *necessary* to reproduce the bug.
4. `CRATONVM_DBG_DUMP_JIT=LIST` on a failing run shows exactly four
   `java/util/WeakHashMap` methods get JIT-compiled: `hash`, `indexFor`,
   `matchesKey`, `maskNull` — precisely the bucket-lookup/key-match
   primitives behind `get()`/`getEntry()`. `matchesKey`'s compiled body is
   979 bytes (`CRATONVM_DBG_DUMP_JIT=matchesKey`), unusually large, with 3-4
   near-duplicate code blocks (not yet disassembled/verified).
5. Ruled out as the SPECIFIC cause: `CRATONVM_DISABLE_SCALAR_REPLACEMENT`,
   `CRATONVM_DISABLE_AALOAD_LICM`, `CRATONVM_DISABLE_ARITH_LICM`,
   `CRATONVM_DISABLE_UNROLL`, `CRATONVM_NO_IR_BRANCHY` — individually and all
   together, the NPE still reproduces. Not one specific, already-toggleable
   optimization pass.
6. Four isolated `.java` repros in `whm-repro/` (single-threaded stress,
   interleaved resize, multi-threaded synchronized contention, thread
   start/join cycles) — none reproduced a single miss after millions of
   `get()` calls each. The bug needs either the ES workload's full scale (165
   total JIT-compiled methods, not just the four `WeakHashMap` ones) or a
   timing window these simplified repros don't hit.

### Repro commands for this session's findings

`--nojit` comparison (from `C:\craton\CratonVM\apps\elasticsearch`, same args
as the main Repro section, plus `--nojit`):

```powershell
& <cratonvm.exe> --java-home "C:\Program Files\Java\jdk-25" ... --nojit -cp $cp `
  org.junit.runner.JUnitCore org.elasticsearch.action.admin.cluster.stats.MappingStatsTests
```

List every JIT-compiled method in a failing run:

```powershell
$env:CRATONVM_DBG_DUMP_JIT = "LIST"
& <cratonvm.exe> ... # same command, JIT on
# stderr contains one "[JIT_COMPILED] Class.methodName(desc)" line per compile
```

Dump one suspect method's machine code:

```powershell
$env:CRATONVM_DBG_DUMP_JIT = "matchesKey"
& <cratonvm.exe> ...
# stderr contains "[JIT_DUMP] ..." with the raw hex bytes
```

Isolated repros (`whm-repro/*.java`) — compile with `javac`, run under this
VM with `-cp .`, no special flags needed; they exercise `WeakHashMap` get/put
under JIT-compilation-forcing iteration counts, both single- and
multi-threaded. All four passed cleanly (0 misses) against the build that
still fails the full ES repro.

## Old-gen `is_addr_live` masking note

`GenerationalHeap::is_old_gen_addr(addr)` is a coarse region-bounds check
(`self.old_gen.lock().contains(addr)`), not a per-object liveness check —
*any* address within old gen's currently-allocated span reads as "live" for
`is_marked` purposes, regardless of whether that specific object is actually
reachable. This means the `OldGen::compact()` "stationary survivor" gap
(fixed alongside the young-sweep one, for general pointer_map-consumer
correctness) could not by itself explain an *incorrect clear* for an
old-gen-resident referent — `is_addr_live` already gives old-gen addresses
an unconditional pass for that specific check. Ruling this out is what
redirected the investigation away from "the suite thread's mirror got
promoted to old gen and hit the compaction gap" as an explanation for the
residual failure.
