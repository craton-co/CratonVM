# FIXED: `Thread.join` monitor-ownership desync under concurrent GC

**Status:** ✅ FIXED on `dev` (commit `74b195b4`, merged). This was the residual
~1–3% tail of the multi-thread-STW work after the four barrier/expansion fixes
(`68c6993e`) took `scratch_churn/Churn.java` from **0% → ~97%**. With this fix,
Churn shows **0 hangs in ~480 runs** (250/250 IMSE-trace clean + 149/150 timing +
40/40 stripped-build sanity). Kept as an internal writeup per the known-issues
triage rule (fixed bugs live in `docs/internal/`).

## Symptom

`main` livelocks inside `java.lang.Thread.join(long)` constructing
`IllegalMonitorStateException` in a tight loop (2823–4728 IMSE allocations in
~7 s before the watchdog aborts). It never reaches a GC safepoint, so the next
`System.gc` initiator's `wait_for_all` waits for it forever and the whole VM
wedges (every worker parks in `arrive_and_wait`, the initiator in `wait_for_all`).

JDK `Thread.join(0)` is `synchronized` via explicit `monitorenter`/`monitorexit`
bytecode, with the javac synchronized-exit handler whose exception table contains
`140 144 140 any` — i.e. the handler guards its **own** `monitorexit` at pc 143.
When `wait(0)` (pc 129) throws IMSE because the thread does not own the monitor,
control reaches pc 140 → `monitorexit` (pc 143) → IMSE again → routes back to
pc 140. This infinite loop is correct JDK bytecode + correct JVM exception
semantics; HotSpot does not hang here only because ownership is intact. **So the
loop is a symptom; the root cause is the lost ownership.**

## Root cause (confirmed)

`NativeContextImpl::monitor_wait` and the worker terminate-tail both do:

```rust
let blk = enter_blocked();
if blk.pre_stw { let _ = arrive_and_wait(tid); }  // let the in-flight STW finish
... monitors.wait(obj) ...                         // obj captured BEFORE blocking
```

`arrive_and_wait` lets the already-active collection **complete**, and that
collection can **relocate** the very Thread object we are about to wait on /
notify, zeroing its old slot. `obj` (and the terminate-tail's `wake_obj`) is a
raw `ObjectRef` captured before we blocked; it is **not** remapped by the
blocked-thread fixup (that runs on *wake*, against our *frames*). So
`monitors.wait(stale obj)` → `ensure_inflated` reads an all-zero (NEUTRAL) mark
word → `inflate_locked` synthesises a **fresh `owner=None` monitor** → the owner
check fails → IMSE → the synchronized-exit loop above → livelock → STW wedge.

### How it was pinned

- Non-invasive `cdb` native stacks across many reproductions showed `main`
  spinning `Churn.main → Thread.join(J) → IllegalMonitorStateException.<init>`.
- Gated mark-word/owner tracing (`CRATONVM_DBG_MONIMSE`) caught
  `inflate-from-NEUTRAL obj=… cur_mark=0x0` on **main's own `Object.wait`** path,
  resolving the exact `owner=None` monitor it then failed on.
- A deposit-time probe proved the object was correctly captured in main's root
  snapshot (`in_snapshot=true, in_local_tagged=true`) yet read `0x0` at
  `ensure_inflated` in the **same** `monitor_wait` call — i.e. relocated during
  the `arrive_and_wait` between the deposit and the wait.

## Fix

`arrive_and_wait` already **returns** the completed collection's pointer map.
Remap `obj` / `wake_obj` through it before use — the same discipline the contended
monitor-enter path already follows (`native_pin_roots` push + read-back):

```rust
let pm = arrive_and_wait(tid);
if let Some(&new) = pm.get(&(obj.as_ptr() as usize)) {
    obj = unsafe { ObjectRef::from_raw(new as *mut u8) };
}
```

Audit of the other `pre_stw { arrive_and_wait }` sites: `thread_join` (native)
uses a `ThreadId`, not an object; contended monitor-enter already pins+reads-back;
`LockSupport.park` uses no monitor object — so `monitor_wait` and the terminate
tail were the only two gaps.

## Not affected

The `libcratonvm` foreign-attach concurrent-GC soak — foreign workers detach via
the host join, never Java `Thread.join` — so it never hit this and was already
re-enabled at the concurrent (all-initiators) form (commit `048e70c5`).
