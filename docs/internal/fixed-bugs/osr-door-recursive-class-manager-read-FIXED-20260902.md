# The OSR door took a recursive `ClassManager` read — three intrinsic regions, one guard

**FIXED 2026-09-02.** `vm-cli/tests/jit_compile_gate_doors.rs` was red on `dev`
with

```
thread 'cratonvm-jit-co' panicked at types/src/lock_order.rs:391:13:
lock order violation: attempted to acquire ClassManager (level 10) while holding
ClassManager (level 10); descending order requires attempted < held
```

That is not a lint. It is `OrderedPlRwLock` reporting a **self-deadlock** one
step before it can happen.

## The defect

`compile_osr_artifact`'s invoke-planning loop opens with

```rust
let cm_lock = shared.classes.class_manager.read();
let class = cm_lock.get_class(class_id)?;
for &(pc, cp_idx, opcode) in &scan.invoke_ops { … }
```

and the guard is live for the whole loop, because `class` is borrowed out of it.
Three intrinsic regions inside that loop turned a class NAME into an id with a
**second** `class_manager.read()`:

| region | landed | latent for |
|---|---|---|
| `AtomicInteger` (`try_resolve_atomic_intrinsic`) | 2026-08-13 `7e3cd7909` | 20 days |
| `AtomicLong` (`try_resolve_atomic_long_intrinsic`) | 2026-08-27 `1b49b529a` | 6 days |
| box/unbox (`try_resolve_box_unbox_intrinsic`) | 2026-09-02 `5903d40ca` | 0 days |

`parking_lot`'s `RwLock` does not let a reader barge past a queued writer, so the
inner acquisition parks behind a writer that is itself waiting for the outer
guard this thread holds. A background compile racing any class load is the whole
window — and the workload the `AtomicLong` commit was written for, netty's
`HashedWheelTimer` worker, is exactly a single-invocation method whose whole life
is one hot loop over an `AtomicLong`.

## Why it took three weeks to surface

Only the third one was ever reached by a test, and only by accident:
`CompileGateDoorsProbe` boxes an `Integer` for its reflection arm, so the
box/unbox region fired the day it landed. Nothing in the suite put an
`AtomicInteger` or an `AtomicLong` inside an OSR-compiled loop, so the first two
were invisible — a real deadlock hazard sitting in the tree with a green gate
over it.

The comment above the first arm is why they were written that way:

> The class-manager guard is read and dropped inside the `let` so no lock is held
> across the matcher call.

True, and the wrong half of the question. The matcher never held one; the LOOP
did.

## The fix

All three resolve through `cm_lock`, the guard the loop already has. Strictly
cheaper, and it is the house pattern — `dispatch_virtual.rs`'s proxy walk states
the same rule in full, including the `parking_lot` fairness argument, and these
three arms were written without it.

## The guard

`vm-cli/tests/jit_compile_gate_doors.rs::the_osr_door_takes_no_recursive_class_manager_lock`
runs `OsrIntrinsicDoorProbe`: one hot loop with one call from every family whose
OSR-door planning region resolves a bootstrap class id per site. Add a call there
when a new family gets one.

Two things it had to get right:

* **It asserts on stderr, not on the exit status.** The panic is on the
  `cratonvm-jit-co` background thread; the VM keeps running. Measured on the
  unfixed tree: `rc=0`, correct checksum, `OK` printed — and two `lock order
  violation` lines. An exit-status assertion would have been mute.
* **It was proven able to fail.** Fix reverted, rebuilt, re-run: two violations.
  Restored: none.

A static sweep for the same shape across `vm/`, `gc/`, `classloading/`,
`native-builtins/` and `jit/` turned up 27 candidates and every one that was not
these three was a false positive — an explicit `drop(guard)` before re-acquiring,
or a guard whose block had already closed.
