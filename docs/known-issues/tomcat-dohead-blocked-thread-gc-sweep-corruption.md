# OPEN: blocked-thread GC sweep frees live AQS condition objects (Tomcat DoHead)

**Status:** OPEN. Gates the Tomcat `TestHttpServletDoHead*` family from going fully green.
Spawned-task id: `task_6fd2be1e`. Full investigation: memory note
`reference_tomcat_dohead_gc_safepoint_deadlock`.

## Symptom
Running `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite0ValidWrite0` (~156
sub-tests, each a full Tomcat start/stop) is flaky: usually finishes
`Tests run: 156, Failures: 1`, occasionally crashes mid-run with a linkage error
(e.g. `java/lang/Object.hasNext()Z`). During one `LifecycleBase.stop()` the stderr
floods with:
- `cratonvm::gc::guard: gen_heap::get/set_field out-of-bounds ... class_id=ClassId(0) class_name=java/lang/Object num_slots=0`
- `Stale pointer detected in invokevirtual receiver (ptr=0x..., all-zero header)` for
  `AbstractQueuedSynchronizer$ConditionNode` / `$ConditionObject`
- then `implicit monitorexit on synchronized-method-frame-pop failed ... does not own the monitor`
  and cascade NPEs (`mapperListener` null, `utilityExecutorLock` null).

## Root cause
A stop-the-world young GC fired while executor/worker threads are **parked on AQS
conditions** during Tomcat shutdown **frees those threads' live `ConditionNode`/
`ConditionObject`** (all-zero header = swept while still reachable). This is the
documented "blocked-thread / moving-young GC sweep gap": live young objects held by
a parked thread are reclaimed. The existing `plausible_heap_pointer` gates only
*contain* it (drop the bad reads → no SIGSEGV); the underlying free-of-live-object
is unfixed. Related: `reference_stale_ref_decode_hardening` ("Underlying GC sweep gap
unfixed"), `reference_blocked_thread_gc_gap`, `reference_hib_temporal_gc_lambda_native_corruption`.

The recently-merged GC-safepoint-deadlock fix (blocking accept/read made
GC-cooperative) is what lets the tests reach `stop()` and hit this; the trigger is
the park-based blocking in `stop()` (`LockSupport.park` → `vm_exec` park →
`enter_blocked` → `deposit_root_snapshot`).

## Where to look
- `vm/src/vm/vm_exec.rs` — `park` / `begin_blocking_region` / `deposit_root_snapshot`
  (is the parked thread's root snapshot complete?).
- `vm/src/threading/thread_registry.rs` — `collect_all_root_snapshots`.
- `vm/src/memory/gc.rs` — the young sweep; does marking follow heap edges from rooted
  AQS `ConditionObject.firstWaiter/lastWaiter → ConditionNode`?

## Repro / validate (PowerShell, from `apps/tomcat-suite-runner`)
```
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName gcbug -Start 28 -Count 1 -TimeoutSec 700 -Parallel 1
```
Env (runner sets these): `CRATONVM_REAL_NET_SOCKETS=1`, `CRATONVM_REAL_AQS=1`,
`CRATONVM_ROOTSNAP_CACHE=1`. Build a uniquely-named binary in a separate worktree
(`reference_worktree_build_recipe`). **Goal:** `Tests run: 156, Failures: 0`
reliably, with no all-zero-header warnings. Tools: `--stack-dump-on-timeout`, cdb
attach, `CRATONVM_GC_STATS=1`.
