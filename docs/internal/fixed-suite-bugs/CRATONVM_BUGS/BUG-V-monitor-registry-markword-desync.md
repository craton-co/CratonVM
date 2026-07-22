# Bug V — monitor inflation invariant: registry/mark-word desync (Rust panic)

**Severity:** High (hard VM abort via Rust `panic!`). Class:
`org.apache.catalina.tribes.group.TestGroupChannelSenderConnections`.
**Status on CratonVM:** **FIXED** (2026-06-13, branch `fix/tomcat-hardcrashes`,
worktree `C:/craton/CratonVM-tcbugs`). Was CRASH (panic, process aborts).
**HotSpot:** PASS. **Run date:** 2026-06-13 (tag `loop2`).

## RESOLVED (2026-06-13) — non-moving sweep skipped the monitor-registry remap

Root cause **confirmed and fixed**. The leading hypothesis below ("a moving GC
relocating a locked object without updating the monitor registry") was correct
in spirit but pointed at the wrong collector: it is the **non-moving young
sweep**, not the Cheney moving collector.

`GenHeap::collect_garbage_inner` (`gc/src/gen_heap.rs`) has two collectors. The
moving path remaps the monitor registry at the end via
`monitors.remap_after_gc(&pointer_map)`. But when JIT frames are live it takes a
**non-moving** young sweep instead (`sweep_young_non_moving`) and `return`s its
result **early — before that remap call**. The non-moving sweep is *not*
relocation-free: with **selective promotion default-on** (`selective_on`, the
Fix-A path that makes bt18 = 68332206) it tenures live young objects into old
gen, producing an `evac_map` of `old→new` relocations. The early return
propagated that map to the interpreter's frame locals/operand stack (the caller
in `vm/src/memory/gc.rs` does `update_local_refs(&result.pointer_map, …)`) but
**never re-keyed the monitor registry**.

So a `synchronized`-inflated object that got selectively promoted kept its
monitor-registry entry keyed to its **old young address**, while its copied mark
word (now at the new old-gen address) still read `INFLATED`. The next
`monitorenter` / `lookup_inflated` at the new address missed the registry →
`inflate_locked` returned the deliberate "mark inflated but registry entry
missing" `Err` → the caller's `.expect(…)` panicked at `monitor.rs:996`. The
tribes test surfaced it because its multi-threaded sender churn both inflates
locks (via `wait`/contention) and runs hot enough to JIT, so promotions happen
while monitors are inflated.

**Fix:** call `monitors.remap_after_gc(&result.0.pointer_map)` on the non-moving
path immediately before the early `return`, identically to the moving path
(`gc/src/gen_heap.rs`, the `gc_quiescence::is_active()` branch).

### Deterministic reproduction + verification

`apps/tomcat/.tooling/drv/MonV.java` forces the exact path: `wait()`-inflate 32
long-lived lock objects, then 4 threads re-lock them while a hot
`new byte[256]` loop drives young GCs under a live JIT frame (selective
promotion). Run with `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` (+ optional
`CRATONVM_DBG_PRECISE=1`).

| binary | result |
|--------|--------|
| pre-fix | all 4 threads + main `panic` at `monitor.rs:996` "registry entry missing"; `[PRECISE]` shows `sweep_young_non_moving returning evac_map.len()=210` then `remap: … reg_size=0` (registry not remapped) |
| post-fix | runs to `MONV DONE`, **0 panics**, with `evac_map` relocations of 210 + 2 still occurring (fix path exercised) |

Regression: `TestGroupChannelSenderConnections` no longer panics (now fails only
on the pre-existing real-socket gap, like HotSpot-incompatible net, not a
crash); **bt18 = 68332206** (HotSpot-correct, selective-promotion path
unaffected); gc crate unit tests unchanged.

## Symptom

```
thread 'main-vm' panicked at vm\src\threading\monitor.rs:996:26:
monitor inflation invariant: registry/mark-word desync:
  InternalError(Runtime(IllegalStateException {
    message: "monitor mark inflated but registry entry missing
              (data race or memory corruption) at obj=0x1c9e5f50" }))
```

## Root cause (not yet fixed)

`monitor.rs` (object-lock / `synchronized` support) reached a state where an
object's mark word says "inflated" (a heavyweight monitor exists) but the
monitor registry has no entry for it — then `inflate_locked(...).expect(...)`
panics. This is a concurrency/GC invariant violation in the monitor subsystem,
surfaced by the tribes test's multi-threaded channel-sender connection churn
(many threads contending on locks while objects are allocated/collected).

Likely contributing factors to investigate:
- A moving GC relocating a locked object without updating the monitor
  registry's key (the registry is keyed by a packed `(hash, gen)` or raw
  address that the GC doesn't remap), so post-GC the mark word still says
  inflated but the registry lookup (by new address) misses.
- A genuine data race between the inflating thread's mark-word CAS and its
  registry insert (the code comments claim this window is closed by the
  registry mutex, but the panic indicates otherwise under this workload).

A hard `panic!`/`expect` on a recoverable "registry miss" is itself a
robustness bug — re-inflation (the fallback the surrounding code already
contemplates for the reserved-state arm) would be safer than aborting the VM.

## Reproduction

```
cratonvm.exe -cp <tomcat-test-cp> org.junit.runner.JUnitCore \
  org.apache.catalina.tribes.group.TestGroupChannelSenderConnections
# Rust panic at monitor.rs:996 — registry/mark-word desync. HotSpot: PASS.
```

(Note: some other `tribes` tests are environmental — they also fail on HotSpot
due to multicast — but `TestGroupChannelSenderConnections` passes on HotSpot, so
this panic is a genuine CratonVM defect.)
