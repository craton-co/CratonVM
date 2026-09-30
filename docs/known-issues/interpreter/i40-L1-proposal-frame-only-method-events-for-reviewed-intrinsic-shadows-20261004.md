# Proposal: method events and breakpoints for Java methods a reviewed `Intrinsic` native shadows

**Status: proposal — filed 2026-10-04 by interpreter round i1 wave 40, lane
L1, when item 3 of
`docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md`
was closed as by design. Not implemented.**

## The gap

A Java method (bytecode, not `ACC_NATIVE`) that a registered
`NativeKind::Intrinsic` native shadows is answered by the native in every
mode (AGENTS.md: only a reviewed `Intrinsic` may win over real bytecode).
While a debugger or an agent needs that method's frame (a breakpoint in it,
a step into it, a JDWP method event request, a JVMTI `MethodEntry` /
`MethodExit` / `SingleStep` listener), the native-call funnel's hook
(`jvmti_events::run_stood_in_java_method`) runs the method's bytecode
instead, but only when `jvmti_events::stood_in_bytecode_may_run` allows it:
the interpreter's intrinsic table (`intrinsics::lookup`) yes, a `Bridge` /
`SyntheticStub` under `--jdk-only` yes, any other `Intrinsic` registration
no. So a breakpoint in `Class.getName`, in a `Math` method outside the
interpreter's table, in a `BigInteger` or Bouncy Castle kernel
(`biginteger_intrinsics.rs`, `phases_late/bouncycastle.rs`, about 140
`NativeKind::Intrinsic` registrations in `native-builtins/src`) is never hit
and the method posts no entry or exit. HotSpot's interpreter reports them
(its compiled intrinsics do not: measured in wave 40, a breakpoint in
`Math.max(JJ)J` was hit 6 times of 200 once its caller was compiled).

The shadows are kept because their bytecode is not known to run correctly
on this VM (`Class.getName` reads `Class.name`, which holds the internal
name here; wave 28's follow-up L1b), so "run the bytecode while a debugger
needs the frame" is not safe for them one by one without review.

## The proposal: a frame-only report

For a shadow whose bytecode may not run, give the debugger the frame without
the bytecode:

1. **Entry and exit.** Push a frame for the method (its real `Code`, pc 0),
   report the entry through the ordinary frame-push path
   (`fire_method_entry_after_push`, JVMTI `MethodEntry`), run the native,
   and report the exit at the method's last `return` bytecode with the
   native's value (`MethodExitWithReturnValue`, JVMTI `MethodExit`), then pop
   it. `ThreadReference.Frames` lists it while the native runs.
2. **A breakpoint at index 0** is delivered from that frame before the
   native runs (the suspend point run once at pc 0). A breakpoint at any
   other index of such a method cannot be honoured without running the
   bytecode. The server must still accept it (a debugger does not expect a
   valid location to be refused), so it stays silent, as now; a counter
   under an existing `CRATONVM_DBG*` gate would record each one.
3. **A step into** the method stops at index 0 and then steps out on the
   next step (the frame has no further locations to report).

Each shadow's bytecode that is reviewed as runnable moves to the
`stood_in_bytecode_may_run` "yes" list and gets full fidelity.

## Cost and place

Cold: all of it sits behind the funnel's existing one-load gate
(`DebuggerGates::method_events_armed`), which is 0 without a debugger or an
agent; nothing is added to any dispatch arm.

## Measure

A JDI probe with a breakpoint at index 0 of `Class.getName` and entry / exit
requests on `java.lang.Class` (HotSpot reports both); `L1W27JdiStandInMethodEvents`
unchanged.
