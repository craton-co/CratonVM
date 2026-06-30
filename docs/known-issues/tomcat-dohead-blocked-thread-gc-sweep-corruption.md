# PARTIALLY FIXED: Tomcat DoHead start/stop GC corruption flood

**Status:** Dominant cause FIXED (branch `fix/dohead-aqs-gc-sweep`, commit `2ea402b7`).
A narrower residual remains (AQS blocked-thread sweep gap). Full investigation:
memory notes `reference_tomcat_dohead_gc_safepoint_deadlock` and
`reference_tomcat_dohead_reflectiondata_sidestore_gc`.

## Symptom
Running `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite0ValidWrite0` (~156
sub-tests, each a full Tomcat start/stop) floods stderr with:
- `Stale pointer detected in invokevirtual receiver (ptr=0x..., all-zero header)`
- `gen_heap::get/set_field: out-of-bounds field read dropped ... class_id=ClassId(0)`
- cascade NPEs / `implicit monitorexit ... does not own the monitor`.

## Root cause #1 — reflectionData side-store root-hole (FIXED)
The **first victim is always a `java/lang/ref/SoftReference`**, cascading to
Method/Field/List/etc. — a GC root-hole in the native-builtins synthetic-offset
side stores. `static_obj_store`, `synthetic_field_store` and
`class_atomic_side_store` hold live `ObjectRef`s that exist in **no heap slot**
(the synthetic-offset scheme services `Unsafe` load/CAS/store from a Rust-side map
when the field's real slot is unknown to our layout). They were never wired into
GC root scanning / remapping, so the collector cannot reach them through the heap
graph. Chief offender: `Class$Atomic.casReflectionData` / `Class.newReflectionData`
stow the `SoftReference<ReflectionData>` for a Class mirror here; a young GC then
reclaims that still-live SoftReference and the whole reflection subgraph decays.

Separately, `Class.reflectionData()` reads `this.reflectionData` (heap **slot 11**)
directly — an old(Class mirror)→young(SoftReference) edge whose card **never goes
dirty** (`CRATONVM_DBG_RSET_AUDIT`: `Class fld[11] -> young CLEAN` every cycle), so
even with the side store rooted the direct getfield reads a freed/relocated ref.

**Fix (commit 2ea402b7):**
1. `gc_scan_unsafe_side_store_roots` (→ `roots.rs` collect_roots) +
   `gc_update_unsafe_side_store_refs` (→ `gc.rs` update_all_roots): the side stores
   are now scanned as roots and remapped, mirroring the established
   `gc_scan_value_of_cache_roots` pattern.
2. Override `Class.reflectionData()` with a native that reads the now-GC-safe cas
   side store instead of the dangling heap slot 11.

**Validated:** the SoftReference / reflection-subgraph cascade is eliminated
(36 diverse stale receivers → 2); the run progresses ~2× further before the
residual below bites.

## Root cause #2 — AQS blocked-thread sweep gap (OPEN, residual)
After fix #1, the only remaining stale receivers are
`java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode` /
`$ConditionObject`. `CRATONVM_DBG_STALE_RECV` shows a **ScheduledThreadPoolExecutor
worker parked in `DelayedWorkQueue.take()` → `ConditionObject.awaitNanos()`**: its
`ConditionNode` (awaitNanos LOCAL[3]) **and** the `DelayedWorkQueue` receiver
(take LOCAL[0]) read all-zero — reclaimed while the worker is parked. This is the
documented blocked-thread / moving-young GC sweep gap (a parked thread's live frame
objects reclaimed): `reference_blocked_thread_gc_gap`,
`reference_stale_ref_decode_hardening` ("underlying GC sweep gap unfixed"). The
`plausible_heap_pointer` gates contain it (no SIGSEGV) but it still surfaces as a
late corruption. Suspected angle: a worker marked dead (or whose snapshot is not
collected) during executor shutdown while still parked in `awaitNanos`, so
`collect_all_root_snapshots` (alive-only) skips its frame roots.

## Repro / validate (PowerShell, from `apps/tomcat-suite-runner`)
```
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName gcbug -Start 28 -Count 1 -TimeoutSec 700 -Parallel 1
```
Env: `CRATONVM_REAL_NET_SOCKETS=1`, `CRATONVM_REAL_AQS=1`, `CRATONVM_ROOTSNAP_CACHE=1`.
Diagnostics: `CRATONVM_DBG_STALE_RECV=1` (frame dump), `CRATONVM_DBG_RSET_AUDIT=1`
(clean-card audit). Build a uniquely-named binary in a separate worktree.
**Goal:** `Tests run: 156, Failures: 0` with no all-zero-header warnings.
