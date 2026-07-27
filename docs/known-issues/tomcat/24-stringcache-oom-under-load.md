# `StringCache.toString()` OOMs under sustained load — `TestMethodPerformance`

**Status:** ✅ **FIXED** on `dev` (fix commit: `fix(gc): restore the
free-list-aware GC-overhead productivity metric`). The OOM was never a
`StringCache` defect at all — it was a spurious `OutOfMemoryError` produced by
the GC-overhead limit mis-scoring every productive young sweep as "freed 0".

A **residual** remains: with the OOM gone the class no longer fails on a
heap error, but it still cannot finish inside a suite timeout — see
[30](30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md), which
root-causes that part and stays OPEN.

## Symptom (as originally filed)

`org.apache.tomcat.util.http.TestMethodPerformance.testGetMethodPerformance`:

```
java.lang.OutOfMemoryError: Java heap space (new_object class_id 6 fields 4)
	at org.apache.tomcat.util.buf.StringCache.toString(StringCache.java:285)
	at org.apache.tomcat.util.http.TestMethodPerformance.testGetMethodPerformance(TestMethodPerformance.java:42)
```

Reproduced at 597s and 227s wall time on two different `dev` tips, and again
at 154s in the 2026-07-22 `g16-reaudit` run — the iteration count varies but
the OOM inside `StringCache.toString()` is consistent. HotSpot completes the
whole class in 41.2s at the same `-Xmx2g`.

The original analysis guessed at "a memory leak in `StringCache`'s caching
structure" or "excessive per-call allocation". Both were wrong: the test's
default configuration leaves `tomcat.util.buf.StringCache.byte.enabled`
false, so `bcCache` stays null forever and every call goes straight to
`bc.toStringInternal(...)`. The loop allocates nothing but immediately-dead
garbage — which is exactly what made it a good detector for the real bug.

## Root cause

`vm/src/runtime/interpreter.rs::note_gc_productivity` scored a forced
(allocation-failure) GC's productivity as `before - after`, with both sides
read from `VmHeap::allocated_bytes()` — `young_from.used() + old_gen.used()`.

The default young collector is **non-moving**: it sweeps dead objects into
the from-space free list *without retreating the bump cursor*. (`moving_young`
defaults off, and `gen_heap` fail-closes to non-moving whenever any thread
holds a live JIT frame — the steady state inside a hot loop.) So once young
has filled once, `young_from.used()` is pinned at its high-water mark forever
and `before == after` on **every** collection, no matter how much it
reclaimed.

Eight consecutive "freed 0" cycles latch `gc_overhead_limit_exceeded`, and
from that point every `jit_new_object` returns OOM immediately — *including
when the young-gen probe succeeded*, because `jit/helpers.rs` consults the
limit unconditionally after the probe. The heap was overwhelmingly garbage
when the `OutOfMemoryError` was thrown.

Measured on the pre-fix binary (`CRATONVM_DBG_GC_OVERHEAD=1`, `-Xmx2g`):

```
[GC_OVERHEAD] before=537102200 after=537102200 freed=0 cap=1610612736 unproductive=true streak=1
[GC_OVERHEAD] before=537267048 after=537267048 freed=0 cap=1610612736 unproductive=true streak=2
...
[GC_OVERHEAD] before=539507000 after=539588880 freed=0 cap=1610612736 unproductive=true streak=8
  -> java.lang.OutOfMemoryError: Java heap space (new_object class_id 6 fields 4)
```

### This was a silently-reverted fix, not a new defect

`a9c580aff` (2026-07-14, "part 3 — GC-overhead metric") had already fixed
exactly this, with a free-list-aware live estimate plus a promoted-bytes
credit. `d8092acba` ("fix-tests-real-jdk-contracts"), the **same day**,
reverted that hunk in `interpreter.rs` — while leaving
`VmHeap::live_bytes_estimate()` and `VmHeap::bytes_promoted_total()` in place
in the gc crate. Both accessors then sat with **zero callers in the whole
workspace** (they are `pub`, so no dead-code warning fired) and the metric
silently went back to reading zero. Nothing failed a test; the only symptom
was suite classes OOMing.

`git log --oneline -S live_bytes_estimate -- vm/src/runtime/interpreter.rs`
shows both commits, one day apart, with the reverting commit's subject having
nothing to do with GC.

## Fix

Restore the two call sites and the function signature in
`note_gc_productivity` / `maybe_gc_forced`:

* `before`/`after` now come from `VmHeap::live_bytes_estimate()`
  (`young.used() - young.free_list_bytes() + old.used()`);
* promoted bytes are credited (a promotion-only cycle conserves live bytes but
  still drains young, which is the point of the collection);
* the 2%-of-capacity threshold is unchanged, so the genuine
  everything-survives-into-a-full-old-gen death spiral still trips the limit.

Regression guard:
`gc/src/gen_heap.rs::live_bytes_estimate_sees_non_moving_sweep_that_allocated_bytes_misses`
asserts that after a non-moving sweep which demonstrably reclaimed bytes,
`allocated_bytes()` reports zero freed while `live_bytes_estimate()` reports
the reclaim — so a third revert fails a unit test instead of a suite class.

## Verification

Same fixture, same `-Xmx2g`, `MessageBytes.setBytes`/`toStringType` loop
(a standalone probe with the identical body to the test's first loop):

| binary | result |
|---|---|
| pre-fix | `OutOfMemoryError` at ~7M iterations (233s), streak reached 8 |
| post-fix | 10M iterations, no OOM, flat heap, streak never leaves 0 |

Post-fix trace:

```
[GC_OVERHEAD] before=537058760 after=288400 promoted=0 freed=536770360 cap=1610612736 unproductive=false streak=0
```

`cargo test -p cratonvm-gc --lib`: 872 passed, 0 failed.

## Blast radius

Not Tomcat-specific. Any workload that (a) allocates enough garbage to fill
young at least eight times and (b) has a live JIT frame (so the sweep is
non-moving) could take a spurious `OutOfMemoryError` on a mostly-empty heap.
Long-running allocation-heavy suite classes across every app in the gauntlet
were exposed to this between 2026-07-14 and the fix.

## Reproduction (historical)

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> -TimeoutSec 1500 -Parallel 1 -RunName stringcache-repro -Exe <cratonvm.exe>
```
Single class `org.apache.tomcat.util.http.TestMethodPerformance`. For the
isolated form, any loop that allocates a few short-lived objects per iteration
under `-Xmx2g` with `CRATONVM_DBG_GC_OVERHEAD=1` shows the `freed=0` /
`streak=N` ladder within a few minutes on the pre-fix binary.
