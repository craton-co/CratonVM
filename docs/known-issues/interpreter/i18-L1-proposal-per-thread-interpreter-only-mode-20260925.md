# Proposal: interpreter-only mode per thread, for events enabled on one thread

**Status: open — filed 2026-09-25 by interpreter round i1 wave 18, lane L1.**
A proposal (kept for triage), not a defect.

## Where it stands

`jvmti::interp_only_events_active_for_vm` answers per VM: once any listener
of the VM enables an interpreter-only event (`SingleStep`, `MethodEntry`,
`MethodExit`, `FramePop`, the field watches, and since wave 18 `Exception`,
`EXCEPTION_EVENTS_NEED_THE_INTERPRETER`), every thread of the VM runs
interpreted, even when the enable names one thread
(`SetEventNotificationMode(ENABLE, kind, thread)`; the manager keeps the
per-thread sets in `thread_events`, and `any_*_listener` is their union).
HotSpot's `JvmtiThreadState::interp_only_mode` is per thread: only the
threads the events are enabled for leave compiled code.

Per-thread enables are what a debugger's "step this thread" and an agent
tracing one request thread use; with `Exception` in the union since wave 18,
an agent that watches one worker thread's exceptions now slows every thread
10–50x on hot loops.

## Design

1. **The answer.** `interp_only_events_active_for_vm(vm)` keeps its
   one-load fast path. Its slow path becomes
   `interp_only_for_thread(vm, tid)`: a global enable of any interpreter-only
   kind answers yes; otherwise yes iff `tid` is in the per-thread set of such
   a kind. Cache it on the thread: a `JvmThread::interp_only_epoch` compared
   with a per-VM epoch the manager bumps in `refresh_listener_flags`, so the
   doors pay one load plus one compare while the union is up.
2. **The askers.** The interpreter→compiled doors and the OSR door have the
   `JvmThread` in hand. The compiled-code helpers find it through
   `JIT_THREAD` (`current_jit_thread_ptr`). The poll verdict
   (`jvmti_events::compiled_frame_exit_verdict`) already receives the
   thread; its every-frame bit becomes per thread.
3. **The edge.** `publish_union_listener_flags` flushes the whole VM's inline
   caches on the false→true edge; keep that (a flushed cache costs a miss on
   other threads and refills while they are not interpreter-only), and ask
   loop exits for the named threads only.

## Staged plan

* Stage 0: count per-thread enables in the wild (JDWP's thread-filtered step
  requests go through `DebuggerGates`, not this manager; the C table's
  per-thread `SetEventNotificationMode` is the JVMTI source).
* Stage 1: the per-thread answer and the thread cache, doors only; the poll
  verdict and helpers keep the VM-wide answer (correct, slower).
* Stage 2: the helpers and the poll verdict.

## How to verify

A test beside `runtime::jvmti::tests::an_exception_listener_holds_its_vm_off_compiled_code`:
`Exception` enabled for thread 3 only; the door question for thread 3 is yes
and for thread 4 no; a global enable makes both yes. End to end: a two-thread
hot loop with an agent enabling `Exception` for one thread; the other
thread's throughput matches a run without the agent.

## Risk

Moderate: every door gains a thread argument, and a thread whose enable
lands while it runs compiled must still be told at its next poll (the
epoch covers it). The VM-wide answer stays the fallback.

## Progress (wave 21)

Interpreter round i1 wave 21, lane L5 (third priority of the lane). No code
landed: every file stage 1 touches — `vm/src/runtime/jvmti.rs` (the
manager's flags), `vm/src/threading/jvm_thread.rs` (the thread cache) and the
door sites in `interpreter.rs` / `jit_bridge.rs` — was being edited by other
lanes of this wave (the `ExceptionCatch` listener flag landed in the same
struct and recompute), and the stage changes the signature of 35 door call
sites. Re-derived from the current code, the stage is:

1. **Manager side** (`JvmtiEventManager`, `jvmti.rs`): in
   `recompute_listener_flags`, beside the per-kind unions, compute
   `interp_only_global: AtomicBool` (an interpreter-only kind enabled in the
   GLOBAL set of this manager or an attached env) and publish
   `interp_only_threads` (the thread ids whose per-thread set holds such a
   kind, own or attached) as a sorted `Arc<[ThreadId]>` behind an
   `ArcSwap`-style slot or an `RwLock`, and bump a per-manager
   `interp_only_epoch: AtomicU64`. `set_event_enablement` already funnels
   every change through `refresh_listener_flags`, so nothing else moves.
   `has_interp_only_listener` stays the union (the fast VM-wide gate).
2. **The answer**: `interp_only_events_active_for_thread(vm, tid)` =
   `UNION_INTERP_ONLY` load, then (cold) `manager_for_vm(vm)` →
   `interp_only_global || interp_only_threads.binary_search(&tid).is_ok()`.
   `EXCEPTION_EVENTS_NEED_THE_INTERPRETER` gates `Exception` as today.
3. **Thread cache**: `JvmThread::interp_only_stamp: Cell<(u64, bool)>`
   (epoch, answer), so a door pays the union load and, while the union is
   up, one epoch compare.
4. **Doors only**: `jit_bridge::jvmti_requires_interpreter(_for)` and
   `interpreter::jvmti_requires_interpreter_for_method` gain the thread id
   (each of the 35 call sites must be checked for a `JvmThread` in hand; one
   without keeps the VM-wide answer); the compiled-code
   helpers and the poll verdict (`compiled_frame_exit_verdict`) keep the
   VM-wide answer, which is correct and only slower.

Test to add with it: beside
`runtime::jvmti::tests::an_exception_listener_holds_its_vm_off_compiled_code`,
`Exception` enabled for thread 3 only answers yes for 3 and no for 4, a
global enable answers yes for both, and disabling returns both to no.

## Wave 24 note (lane L1): the JDWP twin — one suspended thread slows every thread

The same shape exists on the JDWP side, and it bites a more common session:
a debugger that suspends ONE thread — `ThreadReference.Suspend`, or a
breakpoint whose request suspends only the event thread
(`SUSPEND_EVENT_THREAD`, IntelliJ's "Suspend: Thread") — makes every thread
of the VM run interpreted through the per-bytecode suspend point for as long
as that thread stays suspended.

**Evidence.** `debug::publish_debugger_gates` sets `all_methods` when
`DebugState::any_suspension()` is true — any thread's count above zero —
and `all_methods` makes `DebuggerGates::concerns_method` answer yes for
every method: the dispatch loop runs `interpreter::deliver_breakpoint_if_set`
before every bytecode of every thread (two debug-state lock round trips per
bytecode, plus the per-request scans), and the JIT's doors stand down VM-wide
(`requires_interpreter_everywhere`, `request_compiled_loop_exits`). The
suspension needs only the SUSPENDED thread to reach a suspend point; HotSpot
suspends it alone (a JVMTI `SuspendThread` handshake) and the others run
compiled. Timing probe: `tools/probes/interp/L1/L1W24SuspendedThreadBench.java`
(a worker's throughput while `main` is held at a `SUSPEND_EVENT_THREAD`
breakpoint for 3 s, against before and after): HotSpot 25.0.3 prints
`during/before` 1.02, 1.00, 1.01 over three runs; CratonVM's row is the
orchestrator's to take (read from the code: the worker drops to interpreted
speed with a mutex round trip per bytecode).

**Design.** Split `all_methods` in two: the VM-wide cause (a step request, a
method event request, a pending `ThreadReference.Stop`, a VM-wide
`suspend_all_count`) keeps arming every thread; a per-thread suspension
arms only that thread. The per-thread half needs a flag the suspended
thread's dispatch loop reads at its gate refresh (frame switch, back edge)
without the debug-state lock: an `AtomicBool` per registered thread
(`ThreadRegistry` entry, beside the interrupt flag, or the thread-local
`JvmThread` cache this proposal's stage 1 adds), set by
`DebugState::suspend_thread` / cleared by `resume_thread` through a
callback, and OR-ed into `refresh_debugger_gate!` (`breakpoints_armed_now`
stays the VM-wide pre-filter). The JIT's doors then ask the per-thread
answer for the JDWP half exactly as stage 1 asks it for JVMTI, and a
compiled loop on the suspended thread leaves at its next back-edge poll
(`request_compiled_loop_exits` already names the VM; it would name the
thread).

**Expected win.** The worker's `during/before` in the bench above from
interpreted-with-locks speed to about 1.0; every IDE session with
thread-only suspension stops slowing the rest of the program.

**Cost / risk.** Moderate: the gate refresh gains one load of a per-thread
flag; a thread suspended while it runs compiled code must still reach a
poll (the same verdict the VM-wide suspension uses today, narrowed to the
thread). `VirtualMachine.Suspend` and `SUSPEND_ALL` keep the VM-wide gate.

**Staged plan.** 1: the per-thread flag, the gate refresh and the doors
(interpreter only; compiled loops keep the VM-wide exit request). 2: the
compiled-loop exit narrowed to the suspended thread. Verify with the bench
(rows: HotSpot, CratonVM before, CratonVM after; direction: `during/before`
up to about 1) and the existing suspend tests
(`debug::tests::a_suspension_makes_a_compiled_loop_leave_at_its_back_edge_poll`,
`thread_events_apply_their_suspend_policy`).

## Wave 28 note — lane L1

Two wave-28 changes lean on the same VM-wide gates this proposal would make
per thread, and so inherit its cost model:

* The native-call funnel's debugger byte (`DebuggerGates::method_events_armed`)
  gained a third bit for breakpoints and step requests
  (`METHOD_EVENTS_FRAMES`): while any breakpoint is set, every native call of
  every thread takes the full funnel (leaf natives included) and its cold
  hook (`jvmti_events::run_stood_in_java_method`: one decode of the caller's
  invoke under a class-manager read and one registry probe) to learn whether
  the native stands in for a Java method a breakpoint sits in. A step request
  is per thread in JDWP (`StepRequest` names its thread); with a per-thread
  flag the step half of that bit would stop charging the other threads.
  Measure with `L1W24SuspendedThreadBench`'s shape plus a native-heavy worker
  loop (`System.nanoTime`, `String.length` through an intrinsic), rows:
  no debugger, one breakpoint elsewhere, one step on another thread;
  expected: the third row back to the second after stage 1.
* A stood-in Java method called from COMPILED code is still answered without
  a frame (`i27-L1-methods-served-without-a-frame-report-no-method-events`,
  "What remains" item 1); stage 2's per-thread compiled-code exit is the
  mechanism that would bring such a caller back to the interpreter, where
  the funnel's hook applies.

## Wave 29 note — lane L1

What wave 29 built, and what it did not.

* **Built (VM-wide, as the proposal's cost model already is):** a JVMTI
  `SingleStep` listener of the C table now raises the native-call funnel's
  `METHOD_EVENTS_JVMTI_FRAMES` bit (`DebuggerGates::jvmti_frames_armed`,
  `jvmti::native_env::refresh_method_events_gate`), so a stood-in Java method
  called from interpreted code runs its bytecode while an agent steps
  (`i27-L1-methods-served-without-a-frame-report-no-method-events`, item 3).
  A per-thread enable (`SetEventNotificationMode(ENABLE, SINGLE_STEP,
  thread)`) raises it for the whole VM; stage 1's per-thread answer would
  narrow it the same way it narrows `interp_only_events_active_for_vm`.
* **Not built: sending compiled code back to the interpreter when a request
  arms.** The items that need it are `i27-L1` item 1 (a compiled caller of a
  stood-in method), `i24-L1` item 2 (method events of Java methods already
  running compiled, and of their compiled callees), `i9-L5`'s baked direct
  calls and `i9-L1` item 1. The doors already refuse NEW interpreter→compiled
  entries for concerned methods, and loops leave at their back-edge polls;
  what keeps running compiled is a compiled caller's baked direct `CALL` and
  every inlined callee. The machinery that already reaches exactly those
  shapes is the redefinition path's, and the route is to reuse it, not to
  build a second one:
  1. on the false→true edge of a request that concerns every method (a
     JDWP method event or step request, a JVMTI `MethodEntry` / `MethodExit`
     / `SingleStep` listener), withdraw the published bodies as a
     whole-cache redefinition does (`JitRealm::redefine_and_flush`'s
     class-less arm: `JitCache::clear_all_collecting`, `demote_withdrawn`) and
     make them not entrant (`JitCache::make_not_entrant`, which patches only
     bodies no longer published), then force their exit polls
     (`JitCache::force_withdrawn_exit_polls`) so a frame already inside one
     leaves at its next back edge or post-call exit;
  2. the not-entrant stub's helper (`jit::helpers::jit_not_entrant_entry`)
     re-dispatches by name through the interpreter's invoke path, which the
     standing-down doors keep interpreted; but its FORWARD word is filled
     from the cache's published body for the same key, so a body compiled
     and published while the request is in force would be jumped to
     directly. The helper must not fill the forward word while
     `jit_bridge::jvmti_requires_interpreter_for` answers yes for the method,
     and publication (or the doors) must keep refusing such bodies until the
     request ends;
  3. the cost is HotSpot's: every body is recompiled after the debugger's
     request ends. A breakpoint-only session must not take this path (a
     breakpoint concerns one method; the per-method door answer already
     covers it).
  Every function in steps 1-2 is lane L3's this wave (the JIT's withdrawal /
  not-entrant code, `vm/src/jit/**`, `jit_realm.rs`), which is changing the
  same redefinition-driven retirement for `i24-L6-*` / `i19-L3-*`; building
  step 1 on a moving base without a build to validate it was judged the
  larger risk. The positive control it will need: a compiled caller in a
  hot loop calling a compiled callee, a `MethodEntryRequest` created while
  the loop runs, and the callee's entry reported on its next call
  (`L1W26JdiNativeMethodEvents`' shape with the call moved into a compiled
  loop; `CRATONVM_DBG_DEOPT=1`'s verdict lines show the forced exits fire).

## Progress (wave 37) — lane L1

The wave-29 note's steps 1-2 are built, VM-wide (the proposal's per-thread
narrowing is not; this is the "defect fix that needs part of a proposal's
mechanism" case):

* **Step 1.** `JitRealm::withdraw_every_body_for_the_interpreter` (a
  cross-lane addition to lane L3's file, beside `redefine_and_flush`): the
  flush barrier raised before the scan, every published body and every body
  a baked caller or forward reaches listed
  (`JitCache::redefinition_stale_bodies` with `whole_cache`), the optimizing
  OSR bodies listed, the flush, the tier demotions and the code-state epoch,
  then every listed body made not entrant and marked withdrawn, and their
  exit polls and post-call exits forced. No redefine epoch moves and nothing
  is spared as an obsolete activation (the pass is named
  `<interpreter-only>` on its debug lines). It runs on the edge where the
  first source arms (`jvmti_events::note_every_method_needs_the_interpreter`):
  a JDWP step or method event request (`debug::publish_debugger_gates`) or a
  JVMTI `MethodEntry` / `MethodExit` / `SingleStep` / `FramePop` listener of
  the VM (`jvmti::publish_union_listener_flags`). Breakpoints, suspensions,
  field watches and exception requests do not withdraw.
* **Step 2.** The not-entrant stub already refused to forward while
  `compiled_frames_may_be_asked_to_leave` holds (wave 23's
  `jit_not_entrant_entry`), which covers every source above. Publication is
  not refused; instead no tier-up is offered while a source holds the code
  withdrawn (`DebuggerGates::every_body_withdrawn`, read by
  `jit_bridge::offer_invocation_to_tiered_manager`, a cross-lane edit), and
  the doors refuse any body that is still published. The other tier-up
  paths (the general `execute` path's `on_method_invocation_observed`, the
  OSR requests) are not gated: a compile one of them starts is wasted work
  while the request lasts, never entered, and withdrawn again by the next
  request.
* **Cost.** Every compiled body is recompiled after the request ends, as the
  note said. For a debugger that steps with `SUSPEND_EVENT_THREAD` while
  other threads run and compile, each new step request withdraws what they
  compiled since the last one: that is the per-thread mode this proposal
  describes, still unbuilt (HotSpot deoptimizes only the stepping thread's
  frames).
* **Positive control.** `CRATONVM_DBG_JITC=1` prints `[cratonvm-jitc]
  interpreter-only withdrawal: source=<jdwp|jvmti> evicted=<n>
  not-entrant=<n> exits-forced=<n>` once per withdrawal; the probe is
  `tools/probes/interp/L1/L1W37JdiCompiledLoopMethodEvents.java`.
