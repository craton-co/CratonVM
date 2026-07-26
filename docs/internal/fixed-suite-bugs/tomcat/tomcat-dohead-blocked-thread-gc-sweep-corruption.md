# FIXED IN DEFAULT DEV: Tomcat DoHead start/stop GC corruption flood

**Status:** Fixed on the normal dev/Tomcat path in two layers.

1. `2ea402b7` fixed the dominant `Class.reflectionData` / Unsafe side-store GC
   root hole by scanning and remapping the synthetic-offset side stores and by
   serving `Class.reflectionData()` from the GC-safe side store.
2. The narrower AQS timed-wait residual was fixed on 2026-07-01 by adding the
   missing `AbstractQueuedSynchronizer$ConditionObject` timed wait variants to
   the targeted conservative JIT skip list.

Full investigation: memory notes `reference_tomcat_dohead_gc_safepoint_deadlock`,
`reference_tomcat_dohead_reflectiondata_sidestore_gc`, and
`reference_tomcat_dohead_aqs_blocked_jit_register_root`.

## Symptom

Running `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite0ValidWrite0`
(about 156 sub-tests, each a full Tomcat start/stop) flooded stderr with:

- `Stale pointer detected in invokevirtual receiver (ptr=0x..., all-zero header)`
- `gen_heap::get/set_field: out-of-bounds field read dropped ... class_id=ClassId(0)`
- cascade NPEs / `implicit monitorexit ... does not own the monitor`

## Root Cause 1: reflectionData side-store root hole

The first victim was consistently a `java/lang/ref/SoftReference`, cascading into
Method/Field/List reflection data. `static_obj_store`, `synthetic_field_store`,
and `class_atomic_side_store` held live `ObjectRef`s that existed in no heap slot,
so GC could not discover or remap them. The chief offender was
`Class$Atomic.casReflectionData` / `Class.newReflectionData`, which stored the
`SoftReference<ReflectionData>` for a class mirror in the Rust side store.

Separately, `Class.reflectionData()` read heap slot 11 directly, an old class
mirror to young SoftReference edge whose card was never dirtied. Even after the
side store was rooted, that direct field read could return a freed or relocated
reference.

**Fix (`2ea402b7`):**

- `gc_scan_unsafe_side_store_roots` is folded into `roots.rs::collect_roots`.
- `gc_update_unsafe_side_store_refs` is folded into `gc.rs::update_all_roots`.
- `Class.reflectionData()` is overridden with a native that reads the GC-safe
  CAS side store instead of heap slot 11.

## Root Cause 2: AQS blocked-thread timed-wait gap

After root cause 1, the remaining stale receivers were
`java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode` and
`ConditionObject`. Diagnostics showed a `ScheduledThreadPoolExecutor` worker
parked in `DelayedWorkQueue.take()` through `ConditionObject.awaitNanos()`: the
`ConditionObject` local was alive, but the `ConditionNode` local read as an
all-zero reclaimed object.

This is the JIT register-invisibility remainder: the compiled timed wait can keep
the `ConditionNode` oop only in a callee-saved register across `LockSupport.park`.
The blocked-thread snapshot and `scan_active_jit_frames` spill scan cannot see an
oop that was never spilled, and after signal transfer the heap path through the
AQS queue can be unreachable during executor shutdown.

**Fix (2026-07-01):**

`../../../../vm/src/jit/skip_list.rs` now keeps the missing AQS `ConditionObject` wait
variants interpreted under the normal conservative JIT policy:

- `awaitNanos`
- `awaitUntil`
- `awaitUninterruptibly`

`await` was already skipped. This means the Tomcat repro's parked worker publishes
ordinary interpreter frame locals in its blocked root snapshot instead of relying
on an unspillable compiled-register oop.

**Caveat:** this is a default-policy production fix, not an architectural removal
of the JIT register-root gap. Do not remove these AQS skip-list entries unless
parked compiled frames have precise register oop maps or equivalent shadow-stack
coverage. Aggressive JIT/package-allow runs can still expose the underlying gap.

## Validation

Targeted regression:

```powershell
cargo test -p cratonvm-vm --lib aqs_condition_wait_variants_skipped_under_conservative
```

The full historical repro was:

```powershell
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName gcbug -Start 28 -Count 1 -TimeoutSec 700 -Parallel 1
```

with `CRATONVM_REAL_NET_SOCKETS=1`, `CRATONVM_REAL_AQS=1`, and
`CRATONVM_ROOTSNAP_CACHE=1`.

As of 2026-07-01, `../../../../apps/tomcat-suite-runner` is not present in this checkout, so
the Tomcat class could not be rerun here. Goal for any external runner check:
`Tests run: 156, Failures: 0` with no all-zero-header warnings.

## Addendum 2026-07-02: register-only residual closed (large-write variant)

The harder large-write sibling,
`TestHttpServletDoHeadInvalidWrite1023ValidWrite1023`, still crashed (SIGSEGV /
non-moving-sweep desync) after the fixes above, because its heavier AQS churn
exposed a *register-only* remainder: the `ConditionObject`/`ConditionNode`
chain held purely in a callee-saved JIT register that is never spilled to any
scannable stack slot for the whole blocked window, so even the
`scan_active_jit_frames` stack scan (from Root Cause 2/3 above) missed it.

Fixed on `fix/aqs-park-jit-root-pin` (commit `23388aba`, merged to dev as
`71aef458`): `NativeContextImpl::park` (vm/src/vm/vm_exec.rs) now re-reads the
real `java.lang.Thread.parkBlocker` field (already written by real-JDK
`LockSupport.setCurrentBlocker` before every tracked park) and pins it as an
extra `native_pin_roots` entry for the duration of the block — the same
`monitor_wait_keepalive` (BUG-W) pattern, applied to `park`. Verified 6/6 clean
repro runs (`-Xmx500m`, 150s) vs. an unfixed-dev baseline reproducing the
corruption the same day. See memory
`reference_tomcat_dohead_aqs_blocked_jit_register_root` and
`reference_dohead_jit_heap_corruption_open` for full detail, including the
still-unmerged, complementary Layer-2 sweep-walker hardening
(`fix/dohead-sweep-freelist`).
