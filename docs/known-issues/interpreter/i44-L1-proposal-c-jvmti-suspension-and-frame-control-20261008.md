# Proposal: thread suspension and frame control in the C JVMTI table

**Status: open. Filed 2026-10-08 by interpreter round i1 wave 44, lane L1
(proposal, kept for triage).**

## Why

The JDWP server can now suspend a thread, list its frames, read and write
locals, force a return and pop frames (`ForceEarlyReturn` since wave 43,
`PopFrames` since wave 44). The C JVMTI table
(`vm/src/jvmti/native_env.rs`) can do none of this for a thread other than
the caller. The following slots all answer `JVMTI_ERROR_NOT_AVAILABLE`:

| Slot | Function |
|---|---|
| 5 / 6 | `SuspendThread` / `ResumeThread` |
| 92 / 93 | `SuspendThreadList` / `ResumeThreadList` |
| 20 / 67 | `NotifyFramePop` / `ClearFramePop` (and no `FramePop` event) |
| 80 | `PopFrame` |
| 81-86 | `ForceEarlyReturn{Object,Int,Long,Float,Double,Void}` |

Wave 44 added the local-variable functions, but only for the calling
thread. Any other thread is `THREAD_NOT_SUSPENDED`, because nothing in the
table can suspend it
(`docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md`).
Profilers, debuggers written as native agents, and fault injectors (JVMTI
`ForceEarlyReturn`, `PopFrame`) cannot run on this VM as they run on
HotSpot.

## Design sketch

1. **`SuspendThread` / `ResumeThread` on the JDWP machinery.** A per-thread
   suspension count owned by the env (JVMTI counts are per VM, not per
   debugger). The thread parks at its next interpreter suspend point
   (`interpreter::deliver_breakpoint_if_set` →
   `park_for_debugger_under`), or is held in its blocking region
   (`publish_blocked_frames`), exactly as a JDWP `ThreadReference.Suspend`.
   The parks already publish the frames; the C stack functions for another
   thread read that publication (`DebugState::thread_frames`,
   `frame_locals`), or run on the parked thread
   (`debug::run_on_parked_thread`) for the local functions.
   * JVMTI's `SuspendThread` returns only once the thread is suspended.
     HotSpot waits for the handshake. Here the wait is for the park or the
     blocking region, bounded like the JDWP loop-exit pause
     (`request_compiled_loop_exits`), since a thread in a compiled loop parks
     only at an exit-capable poll.
2. **`PopFrame` and `ForceEarlyReturn*`** reuse `debug::pop_frames` and
   `debug::early_return` on the suspended thread. They have the same
   refusals, mapped to JVMTI codes (`OPAQUE_FRAME`, `NO_MORE_FRAMES`,
   `TYPE_MISMATCH`, `THREAD_NOT_SUSPENDED`). Their capabilities are
   `can_pop_frame` and `can_force_early_return`.
3. **`NotifyFramePop` / `FramePop`**: the interpreter's
   `fire_jvmti_frame_pop_if_requested` already posts to Rust-side listeners
   through `thread.frame_pop_requests`. The C table needs the request
   function and a delivery through the env, as for `MethodExit`
   (`JvmtiNativeEnv::sync_event`). The capability is
   `can_generate_frame_pop_events`.

Each step raises its capability only once served (the D14 rule of
`jvmti/capabilities.rs`). Each needs rows in a C-agent probe run against
HotSpot, like `tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.c`.

## Cost

* Without an agent: nothing. Every piece is behind a table slot or behind
  the existing debugger gates.
* With an agent that suspends: the JDWP suspension's costs (parks,
  loop-exit pauses).

## Progress (wave 45) — lane L1

**Built: design steps 1 and 3 (without `ClearFramePop`), and `GetThreadState`.**

* Slots 5 / 6 / 92 / 93, `SuspendThread` / `ResumeThread` /
  `SuspendThreadList` / `ResumeThreadList`, with `can_suspend`
  (`jvmti::native_env::suspend_thread` and siblings). A C agent's
  suspension is JVMTI's flag, not JDWP's count
  (`DebugState::jvmti_suspended`: a second `SuspendThread` is
  `THREAD_SUSPENDED`, one `ResumeThread` ends it), and it parks the thread
  exactly as a JDWP suspension does (`DebugState::is_thread_suspended`
  includes it; `any_suspension` arms the gates). A debugger's detach leaves
  it in force; a thread that ends drops it (`forget_jvmti_suspension`, from
  `send_thread_event_of`'s `ThreadDeath`).
  * `SuspendThread` of another thread returns once the thread stands still:
    parked, blocked in a native region, or gone (`thread_stopped`), waiting
    GC-safe (`wait_gc_safe`, a blocking region on the caller) for at most 2 s
    (`STOP_WAIT`); a thread in a compiled loop with no exit-capable poll is
    left suspended past that. `SuspendThread` of the calling thread returns
    only once it is resumed, as on HotSpot; the thread waits in a blocking
    region, so its frames stay readable through its window.
  * `can_suspend` is potential in the OnLoad phase only
    (`ONLOAD_POTENTIAL`, `JvmtiNativeEnv::potential`), as HotSpot 25.0.3
    grants it (measured).
* Slot 17, `GetThreadState`: `java.lang.Thread.State`'s bits from the
  registry's blocking kind, `SUSPENDED` from JDWP and JVMTI suspension alike
  (the agreement the proposal asked for), `INTERRUPTED`; `TERMINATED` for a
  thread the registry knows as ended, 0 for one never started.
* Design step 3, `NotifyFramePop` (slot 20) and the `FramePop` event (61),
  with `can_generate_frame_pop_events` (potential in the live phase too, as
  on HotSpot): the request goes into `JvmThread::frame_pop_requests` at the
  interpreter frame the listing's depth names (`request_frame_pop`, on the
  current thread, or on a suspended one while it is parked, through
  `debug::run_on_parked_thread`), and the interpreter's pop posts it
  (`fire_jvmti_frame_pop_if_requested`) to the env's delivery
  (`deliver_frame_pop`, a `MANAGER_EVENTS` row). `DUPLICATE` for a second
  request for one frame, `OPAQUE_FRAME` for a native or compiled one.
  While a thread is parked at a native's return, the native is at depth 0 of
  the table's listing as of JDWP's (`park_if_suspended_at_native_exit`
  enters a `NativeFrameRow`). Probe:
  `tools/probes/interp/L1/L1W45JvmtiNotifyFramePop.java` (C shim; HotSpot
  measured with a Rust port). Positive control: `CRATONVM_FRAME_TRACE=1`
  prints `[JVMTI_FRAME_POP] requested tid=<n> depth=1 frame=<index>` twice.
* The stack functions of another thread (see
  `interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010`, Progress
  (wave 45)).

Probe: `tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.java` with its
C shim (HotSpot measured with a Rust port on the Windows box; the
orchestrator should confirm the block with the C shim on the host).

**What remains**, in order:

1. `ClearFramePop` (slot 67), and `NotifyFramePop` of a thread suspended
   while blocked in a native region (it answers `NOT_AVAILABLE`: the
   request list is the blocked thread's own, written only by it; the window
   protocol, `interpreter::blocked_frame_rows`, could hold it still for the
   write, but the frame index must come from its listing, not the caller's
   thread-local JIT chain).
2. `PopFrame` (80) and `ForceEarlyReturn*` (81-86) on a suspended thread,
   through `debug::pop_frames` / `debug::early_return` run on the parked
   thread.
3. `GetThreadState`'s finer bits: `SLEEPING`, `IN_OBJECT_WAIT`, `PARKED`
   (the registry does not say which native entered a blocking region; JDWP's
   status guesses `SLEEPING` from the deposited frames,
   `debug::jdwp_thread_status_with`) and `IN_NATIVE`.
4. A thread parked at the return of `Object.wait0` is `RUNNABLE` to
   `GetThreadState`; HotSpot keeps it `WAITING` (JDWP's status keeps it
   `WAIT` since wave 45, `DebugState::native_exit_statuses`).
