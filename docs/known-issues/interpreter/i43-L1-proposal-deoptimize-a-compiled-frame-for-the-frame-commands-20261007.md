# Proposal: deoptimize a compiled frame for the JDWP frame commands

**Status: open — filed 2026-10-07 by interpreter round i1 wave 43, lane L1
(proposal, kept for triage). Wave 44 (lane L1) built the stage that needs no
JIT change: a breakpoint's direct callers stay interpreted (see "Progress
(wave 44)"). Deoptimizing a compiled frame that is already running is not
built; it needs the JIT change described there.**

## The problem

Every JDWP command that changes a frame refuses a compiled one with
`OPAQUE_FRAME`:

* `StackFrame.SetValues` (`debug::inspect::set_frame_values`, and the wave-41
  deferred write of a blocked thread, `defer_blocked_frame_write`);
* `ThreadReference.ForceEarlyReturn` (wave 43, `debug::early_return`), which
  is served only at the interpreter's suspend point before a bytecode;
* `StackFrame.PopFrames` (built in wave 44, stage 3 of
  `docs/internal/fixed-bugs/interpreter-L1-proposal-pop-frames-force-early-return-and-source-debug-extension-FIXED-20261008.md`),
  for a compiled caller.

`StackFrame.GetValues` of a compiled frame answers `OPAQUE_FRAME` too (a
compiled row carries no locals, `interpreter::push_compiled_row`).

HotSpot serves all four on any Java frame, compiled or not (JVMTI's local
variable and frame functions make no difference between them; to measure
with a JDI scenario whose caller is C2-compiled before the stop). A compiled
frame reaches a debugger in this VM
wherever the thread was suspended while it ran compiled code below the
interpreter frame it parked in: every caller compiled before a breakpoint
was set in its callee (the callee is refused compilation since wave 39, the
caller is not), and every frame of a thread suspended while blocked under
compiled code (wave 21's listing). An IDE's variables view then shows those
frames with no variables, and "Set value", "Force Return" and "Drop Frame"
fail on them.

## Design sketch

The pieces exist, for other reasons:

1. **Materialize.** The JIT's deoptimization exits already rebuild an
   interpreter frame from a compiled activation's frame state and resume it
   at the trapping bci (`interpreter::deopt_resume::build_deopt_frame_inner`,
   with `runtime::deopt_materialize` for scalar-replaced objects). A
   debugger request would do the same for the compiled activations of a
   parked thread's listing — the work runs on that thread as a parked task
   (`debug::run_on_parked_thread`) — or mark them so the thread rebuilds
   them on its way back into them, as the post-call exits do for a
   redefinition.
2. **Keep them interpreted.** The method is then kept from re-entering
   compiled code while the debugger holds the frame
   (`interpreter::jvmti_events::breakpoint_bars_compiling`'s gate is the
   model).
3. **Serve the commands on the new frames.** `GetValues` / `SetValues` then
   find an interpreter frame; `ForceEarlyReturn` a point
   (`debug::early_return::enter_point` at the frame's resume); `PopFrames`
   an interpreted caller.

What must be checked first: that a compiled activation's frame state holds
every local at the bci it is listed at (`BlockedCompiledRow::bci`), and
which activations have no precise record (`active_compiled_frames` skips
them today), which would stay `OPAQUE_FRAME`.

## Why it matters

The four commands are the ones an IDE uses most after breakpoints and
steps. The wave-39 compile refusals already keep the method a breakpoint
sits in interpreted, so the frame a user stops in is served; its callers
are not, and a user who clicks one frame down sees no variables.

## Progress (wave 44) — lane L1

**Built: the frame one below a breakpoint stop is an interpreter frame.**
This is the "keep them interpreted" stage (design step 2), applied before
the stop instead of after a deoptimization. While a JDWP or JVMTI
breakpoint stands, the compile doors refuse any method whose own bytecode
has an `invokevirtual` / `invokespecial` / `invokestatic` /
`invokeinterface` naming (by name and descriptor) a method holding a
breakpoint, as they already refuse the method itself since wave 39. That
means no body, no splice, no direct bind, and no offer at a tier-up stride.
The code is `interpreter::breakpoint_bars_compiling` →
`jvmti_events::calls_a_breakpoint_method`, memoized per method and
breakpoint generation (`DebuggerGates::caller_verdict`,
`breakpoint_generation`). A method of a JDK class is never held this way:
JDK classes carry no `LocalVariableTable`, so a debugger shows none of
their variables anyway, and every `toString` caller of the JDK would
otherwise stay interpreted for one user `toString`. At every hit, then,
`StackFrame.GetValues` / `SetValues` on the caller find an interpreter
frame, and so would a `PopFrames` of the method the breakpoint sits in
(wave 44).

* Switch: `jvmti_events::BREAKPOINT_CALLERS_STAY_INTERPRETED_ENABLED`, ON
  in its own last commit of the lane (off, it is wave 43's behaviour).
* Probe: `tools/probes/interp/L1/L1W44JdiCallerFrameOfBreakpointCallee.java`,
  in the JDI runner's default list. HotSpot 25.0.3 reads and writes
  frame 1 at all 200 hits, and the 200 writes show in the sum
  (`19999900200`).
* Base: in the JIT modes `mid` compiles after the breakpoint, and frame 1
  answers `OPAQUE_FRAME` from then on (`caller refusals:
  OpaqueFrameException`).
* Positive control: `CRATONVM_DBG_JITC=1` prints
  `[cratonvm-jitc] debugger keeps the caller of a breakpoint method interpreted: L1W44JdiCallerFrameOfBreakpointCallee.mid(I)I`.
* Side effect on `L1W39JdiBreakpointBeforeCallersCompile`: its `mid` no
  longer compiles, so that header's positive control (the
  `inline-resolve REFUSED ... callee` line printed when `mid` compiles)
  does not appear. Its transcript is unchanged.

**What this stage does not reach** (each still `OPAQUE_FRAME`):

1. A direct caller compiled BEFORE the breakpoint was set, including one
   already running. The scoped withdrawal
   (`interpreter::note_breakpoint_classes_gained`) takes away the
   breakpoint class's own bodies, its dependents and their baked callers,
   but not a body that reaches the method through dispatch.
   * Withdrawing those is a VM-side step. On a breakpoint's rising edge,
     scan the loaded non-JDK classes for methods that invoke it
     (`code_invokes_any`) and withdraw each such class
     (`JitRealm::withdraw_class_for_the_interpreter`). It is not built:
     the scan is O(loaded bytecode) under the debug-state lock, which
     should be measured on a Spring Boot session first.
   * A frame already running such a body stays compiled until it returns.
2. A caller two or more frames below the stop, and a frame whose OSR body
   runs a loop around the call (the stale-locals row,
   `BlockedCompiledView::stale`). The OSR door asks
   `jvmti_requires_interpreter_for_method`, not
   `breakpoint_bars_compiling`, so the caller gate does not reach it.
3. A thread suspended while it runs compiled code (not at a breakpoint).
   Its top frames are compiled rows.

**Why a running compiled frame cannot be served from the VM side**
(checked this wave, for whoever builds the rest). HotSpot reads such a
frame's locals from the `ScopeDesc` of its call site. This VM's JIT
records no description of a compiled frame's locals at a call site:

* The single-pass tier maps locals to callee-saved registers
  (`x64::Compiler::reg_for_local`, `local_assignments`). At a call, their
  values live wherever the callee chain, Rust frames included, saved those
  registers, so no walk can find them.
* The GC oop maps name only references.
* The deopt maps (`OsrExit`, the post-call exit's successor map,
  `emit_post_call_exit_site`) exist at back edges and at a few admitted
  call successors, and describe the state AFTER the call returns. A body
  gets at most 64 of them.

Building the rest needs, in the JIT:

* either a per-call-site locals map (every local's home at the return
  address, the post-call exit's `FrameState` taken before the call);
* or, while a debugger is attached (`debugger_observes_locals`, today only
  a liveness flag), spilling every local to its home slot
  (`[rbp - (idx + 1) * 8]`, `x64::Compiler::local_offset`) before each call
  and recording the kinds.

The VM side would then read those slots for `GetValues`, and for
`SetValues` / `ForceEarlyReturn` / `PopFrames` force the body's post-call
exits, as a redefinition does (`JitCache::force_withdrawn_exit_polls`),
with the written values applied to the rebuilt interpreter frame. That is
JIT code generation (lane L2's files) and cannot be verified without a
build, so it was not attempted here.
