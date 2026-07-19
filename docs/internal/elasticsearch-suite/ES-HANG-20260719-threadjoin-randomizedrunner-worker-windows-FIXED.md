# ES HANG — `Thread.join()` never returns for a dead RandomizedRunner worker (Windows host)

Status: FIXED (2026-07-19, `vm/src/threading/monitor.rs` + `vm/src/vm/vm_exec.rs`, worktree `serene-lamarr-01d83a`; merged to `dev` at `bd83c42fa`)

Discovered 2026-07-19 while doing full end-to-end verification of the
IVFKnn stale-precise-root-mirror fix (see
[`ES-HANG-20260709-...-3ff8aa1c4b-FIXED.md`](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b-FIXED.md)).
**Not caused by, or related to, that fix** — confirmed to reproduce
identically on a clean, unmodified `origin/dev` build. This is a
separate, pre-existing CratonVM defect, first observed on this Windows
host; the doc it was found alongside has historically only been
verified on Linux (Azure), which may explain why this has not been
caught before.

## Symptom

Any ES `JUnitCore` direct-invocation repro that goes through
`com.carrotsearch.randomizedtesting.RandomizedRunner` (i.e. essentially
every real ES test class) hangs indefinitely. A `--stack-dump-on-timeout`
dump shows:

```
tid=0 name="main" ... blocked=?  (in Thread.join())
  depth=15 class=java/lang/Thread method=join desc=()V
  depth=16 class=java/lang/Thread method=join desc=(J)V
  ... com/carrotsearch/randomizedtesting/RandomizedRunner.runSuite
```

and the worker thread it is joining shows:

```
name="...-seed#[B17AC9D3E1F2A0C4]-worker" alive=false daemon=false blocked=true roots=1 top=<no-frame-trace>
```

i.e. the worker thread has already finished (`alive=false`) but the
main thread's `Thread.join()` never wakes up — a lost-wakeup /
notification-ordering bug in CratonVM's `Thread.join()` implementation
or in the thread-death → registry-update → notify sequence.

## Reproduction

Any of the following, run directly (bypassing the suite runner), hang
identically:

```powershell
$JDK = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$ES  = "C:\craton\CratonVM\apps\elasticsearch"
$CP  = Get-Content "$ES\..\..\server\build\craton-testcp.txt" -Raw
& $EXE --java-home $JDK --stack-dump-on-timeout 90 --Xmx 2g `
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home=$ES `
  -Dtests.testfeatures.enabled=true -Dtests.security.manager=false -Dtests.asserts=false `
  -Dtests.timeoutSuite=580000! -Dtests.method=testSlicesSparseWithFilter `
  <standard ES --add-opens set, see run-elasticsearch-suite.ps1 Get-EsJavaArgs> `
  -cp $CP org.junit.runner.JUnitCore `
  org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests
```

- `testSlicesSparseWithFilter` on `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests`: hangs, 2/2 runs.
- `testRandomWithFilter` on the **sibling class** `IVFKnnFloatVectorQueryTests`: hangs, 1/1 run.
- Same hang with `--nojit` — **not JIT-related**.
- Same hang on a from-scratch `origin/dev` build (`C:\craton\CratonVM\target\release\cratonvm.exe`,
  no IVFKnn-session changes at all) — confirms this is not something this
  session's GC/JIT fixes introduced.
- The identical repro under real HotSpot (`java.exe`, same JDK, same
  classpath, same args minus the CratonVM-specific flags) completes
  cleanly: `OK (1 test)`, ~2.2s. This rules out the ES test fixture /
  checkout state — the hang is CratonVM-specific.

## Root cause

Confirmed via a minimal, fast, non-ES repro (a trivial `@RunWith(RandomizedRunner.class)`
test class doing nothing but `Thread.sleep(10)`, compiled against the ES
server test classpath) — this is a plain CratonVM logic bug, not anything
IVFKnn/Lucene/ES-specific, GC-timing-specific, or JIT-specific:

In `thread_start`'s spawn closure (`vm/src/vm/vm_exec.rs`), the terminating
thread's death sequence is, in order:

1. Acquire its own Java `Thread` mirror's monitor (`enter_inflated_or_contend`)
   as `term_monitor` — held deliberately across the next step so the final
   `notify_all()` below is safe (mirrors `synchronized(this) { alive=false; notifyAll(); }`).
2. `term_blk.finish_after(|| { clear_tlab_addr; mark_dead; release_monitors_held_by(tid); })`.
3. `term_monitor.notify_all(tid)` + `term_monitor.exit(tid)`.

Step 2's `release_monitors_held_by(tid)` sweeps **every** inflated monitor
currently owned by `tid` and force-releases it (`Monitor::force_release_if_owned_by`,
setting `state.owner = None`) — a sweep intended to reclaim monitors a thread
abandoned mid-native-call inside an ordinary `synchronized` block without
executing its `monitorexit`. It does not distinguish those abandoned locks
from `term_monitor`, which `tid` **still legitimately owns** at that exact
point specifically for step 3. So step 2 force-releases `term_monitor` out
from under step 3.

Step 3's `monitor.notify_all(tid)` then checks `state.owner == Some(tid)`,
finds `None` (just cleared), and returns `Err(MonitorError::NotOwner)` —
**silently discarded** by the `let _ = monitor.notify_all(tid);` call site.
`wait_condvar.notify_all()` is therefore never invoked, and any thread
parked in `Object.wait()` inside `Thread.join()`'s `synchronized(this) {
while (isAlive()) wait(millis); }` loop is never woken — a classic lost
wakeup. `state.owner` is already `None` by the time `monitor.exit(tid)`
runs next, so that call fails the same way and is equally silently ignored.

This exactly matches the observed signature: worker `alive=false` (mark_dead
ran fine), joiner permanently parked in `Thread.join()` (the wakeup that
should have fired never did). No GC, JIT, or ES-suite dependency — the
sweep runs unconditionally on every platform-thread death via this path, so
the bug is present regardless of OS; this doc's original "Windows-specific"
framing was a red herring (Windows just happened to be where an ES
`JUnitCore` run was first exercised end-to-end this session).

## Fix

`vm/src/threading/monitor.rs`: added `MonitorTable::release_monitors_held_by_except(thread_id, except: Option<&Arc<Monitor>>)`,
identical to `release_monitors_held_by` except it skips `except` even if
owned by `thread_id`. The original `release_monitors_held_by` is now a thin
wrapper (`except = None`) — all other call sites (`vm/src/native/jni.rs`'s
foreign-thread teardown, `vm_exec.rs`'s `unregister_native_thread`) are
unaffected.

`vm/src/vm/vm_exec.rs`'s `thread_start` spawn closure: step 2 now calls
`release_monitors_held_by_except(tid, term_monitor.as_ref())`, excluding the
monitor step 3 is about to `notify_all`/`exit` on.

## Verification

- Minimal repro (trivial `Thread.sleep(10)` test under `RandomizedRunner`):
  hung reliably pre-fix (`alive=false` / stuck `Thread.join()`, matching the
  Symptom section); 5/5 clean `OK (1 test)` runs (~0.07s each) post-fix.
- Real-world repro, `testSlicesSparseWithFilter` on
  `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests` (same command as
  the Reproduction section): `OK (1 test)`, 85.957s, no watchdog abort —
  previously hung indefinitely (aborted only by the `--stack-dump-on-timeout`
  watchdog). Confirms the fix holds under the original real ES workload, not
  just the synthetic isolation case.
- `cargo test --release -p cratonvm-vm --lib threading::`: 287 passed, 0
  failed — no regressions in the monitor/thread-registry test suite.

Merged to `dev` at `bd83c42fa`. Not yet re-run under `--nojit` or on Linux
post-fix (pre-fix evidence already showed the hang was independent of both).

## Impact (pre-fix)

Blocked full end-to-end `JUnitCore`-based verification of ES test classes
on this Windows host — including the final confirmation step for the
IVFKnn stale-precise-root-mirror fix and any future ES suite work done
here. The underlying GC/JIT correctness fixes in the sibling doc were
still validated thoroughly via non-ES-suite means (full `cratonvm-gc`/
`cratonvm-jit`/`cratonvm-vm` `--lib` suites, JIT differential suites, and
the `bench/BenchSuite.java` micro-benchmarks bt10–bt18/arith/matrix, all
correct-checksum) — this hang specifically blocks only the ES-suite-shaped
verification path.
