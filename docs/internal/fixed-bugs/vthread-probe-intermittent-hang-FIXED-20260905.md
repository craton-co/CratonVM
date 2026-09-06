# `VthreadProbe` hangs about one run in five — a virtual thread that yields never arrives at the stop-the-world barrier it was counted for

**Status: FIXED 2026-09-05** (`vm/src/vm/vm_exec.rs`, two sites).
Reproduced 5 times in 25 on the tip of `dev`; 0 in 25 after the fix, and a
purpose-built stress probe went 7-hangs-in-7 to 8-clean-in-8.

## The symptom

No cargo, no test harness — the VM alone:

```bash
CRATONVM_JAVA_HOME=<jdk25> ./target/release/cratonvm \
  -c vm/tests/resources/vthread_probe/classes VthreadProbe
```

25 consecutive runs on Azure `20.80.105.49`, tip of `dev` (`a044e1fe1`):

| runs | elapsed | output |
|---|---|---|
| 20 | 2–12 s | `counted=10000 ok=true` + `OK` |
| 5 | killed at 70 s | nothing on stdout |

Bimodal with nothing in between, and independent of machine load — which is
what separates a hang from a workload that is merely slow under contention.

## What it actually was

A hung run is **not** silent. It writes one line to stderr:

```
WARN cratonvm_vm::runtime::interpreter::gc_and_alloc:
  STW cross-thread JIT takeover is still waiting for cooperative mutators
  rounds=64 pending=2 taken=0
```

That is `stw_take_over_and_wait`'s 64-round tripwire. The VM is not livelocked
in the virtual-thread scheduler at all: it is inside a **stop-the-world pause
that can never be satisfied**, because `arrived` cannot reach `expected`.

`CRATONVM_DBG_STW_CENSUS=1` names the shape exactly. From a hung run:

```
[stw-request] initiator=0 alive=237 blocked=0 live_blocked=233 effective_blocked=233 expected=3
[stw-arrive]  tid=4952 gen=0 arrived=1 expected=3
[stw-arrive]  tid=4948 gen=0 arrived=2 expected=3
   ... nothing further, forever ...
[stw-census] rounds=64 pending=1 taken=0
```

Three mutators were counted; two arrived; the third never did. In the
per-thread census taken 64 rounds later, every `ready=true` thread except the
two parked in the barrier reads `blocked=true` with

```
top=VthreadProbe.lambda$main$0@3 <- java/lang/Thread.runWith@5
    <- java/lang/ThreadBuilders$BoundVirtualThread.run@29
```

— i.e. the missing thread is a virtual thread that *became* blocked after the
census counted it as running.

## The mechanism

`GcBarrier::request_stw_counted_with_live_blocked` computes

```
expected = alive(stw_ready) - 1(initiator) - live_blocked
```

where `live_blocked` is the set of registry entries whose
`gc_block_state.in_blocked_region` is up. A parked continuation is excluded by
that flag, which its carrier raises in `deposit_root_snapshot()` on the way to
`VirtualThreadManager::suspend_runtime`.

Every other transition into a blocked region in this VM takes the flag change
**under the barrier lock** and arrives for a pause that is already in flight —
`NativeContextImpl::park`, `monitor_enter_blocking`, and the virtual thread's
own TERMINATION path in `resume_virtual_continuation` all do

```rust
let blocked = gc_barrier.enter_blocked();
if blocked.pre_stw { let _ = gc_barrier.arrive_and_wait_auto(tid); }
```

The **yield** path did not. It ran `deposit_root_snapshot()` and went straight
into `suspend_runtime`:

```rust
if let Err(...ContinuationYield { wake_after_nanos }) = &result {
    NativeContextImpl { .. }.deposit_root_snapshot();
    shared.threads.virtual_thread_manager.suspend_runtime(vt_id, thread, ..);
    return;
}
```

So a pause requested while the continuation was still a **counted** mutator has
its `tid` in `expected`; the deposit then raises the flag (too late for that
census); `suspend_runtime` hands the `JvmThread` to the manager; and the carrier
OS thread returns to `ForkJoinScheduler::wait_for_task_until`. Nothing on that
OS thread will ever arrive for that `tid` again. `wait_for_all` blocks forever
and the whole VM freezes with no output.

**Why intermittent.** The interpreter polls for a safepoint on *every*
bytecode, so the unprotected window is only the stretch from the last poll to
the deposit: the `Thread.sleep` native plus the unwind that carries
`ContinuationYield` out of `execute_frame`. A pause has to be requested inside
that window on one of the eight carriers. With 10 000 sleeps and a handful of
collections per run, that lands about one run in five.

**Why the carrier pool was the wrong suspect.** The displaced poll loop's
comment named "the v-thread scheduler regresses to a 1-carrier livelock", and
the fact that a hung run prints nothing was read as "the stall is before
dispatch". Both readings were wrong: `VthreadProbe` prints nothing until
`done.await()` returns, so "no output" carries no positional information at all,
and the scheduler is idle — its carriers are parked in `wait_for_task_until`
with an empty queue, exactly as they should be.

## The fix

Both `ContinuationYield` consumers — `resume_virtual_continuation` (a
continuation yielding on a carrier) and `thread_start`'s virtual arm (the first
yield, still on the spawning OS thread) — now perform the same handshake as
every other blocked-region entry, between the deposit and the hand-off:

```rust
{
    let blocked = shared.mem.gc_barrier.enter_blocked();
    if blocked.pre_stw {
        let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
    }
}
```

`arrive_and_wait_auto` is the required flavour rather than `arrive_and_wait` or
`arrive_and_wait_excluded`: whether *this* pause counted us depends on whether
its census ran before or after the deposit immediately above, which is not
decidable locally — that is precisely the ambiguity
`GcBarrierInner::excluded_blocked` exists to resolve, and `_auto` is the
accessor that consults it under the same lock the census populated it under.

The returned `PointerMap` is deliberately dropped. A thread whose
`in_blocked_region` is up is remapped by `fold_pointer_map_into_blocked` into
`gc_block_state.fixup`, which the continuation's next mount drains in
`check_post_block_gc`; applying the map here as well would apply the same move
twice.

Every producer of `ContinuationYield` (`Thread.sleep`, `Thread.sleep0`,
`sleepNanos`, `Unsafe.park`, `LockSupport.park*`, `CountDownLatch.await`) funnels
through those two consumers, so the two sites cover the whole family.

## Evidence

| binary | probe | result |
|---|---|---|
| `dev` tip `a044e1fe1` | `VthreadProbe` x 25 | **5 hangs** (70 s cap), 20 clean |
| `dev` tip `a044e1fe1` | `VthreadGcStress` x 8 | **8 hangs in 8** (60 s cap) |
| fixed | `VthreadProbe` x 25 | 0 hangs |
| fixed | `VthreadGcStress` x 8 | 0 hangs |

## The regression gate

`VthreadProbe` is a one-in-five detector, which is not a gate. The new fixture
`vm/tests/resources/vthread_probe/VthreadGcStress.java` makes the same defect
deterministic: 3 000 virtual threads each sleeping 5 ms while one platform
thread calls `System.gc()` 400 times, so a pause is nearly always in flight when
a continuation unmounts. On the unfixed binary it hung 8 times out of 8; it is
now gated by `vthread_gc_stress_completes` in
`vm/tests/vthread_probe_regression.rs`.
