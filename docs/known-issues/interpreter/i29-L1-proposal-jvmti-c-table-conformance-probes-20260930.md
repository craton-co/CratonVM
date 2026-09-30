# Proposal: HotSpot-comparison probes for the C JVMTI table

**Status: open — filed 2026-09-30 by interpreter round i1 wave 29, lane L1.**
A proposal (kept for triage), not a defect.

## Where it stands

The JDWP server is measured against HotSpot end to end: JDI scenarios and,
since wave 28, raw JDWP scenarios (`tools/probes/interp/L1/L1W28RawJdwp*`,
`L1W29RawJdwp*`) run with the conformance runner
(`tools/jdi/run-jdi-conformance.sh`). Wave 29's two raw probes found five
answers that differed from HotSpot 25.0.3 (`EventRequest.Set` modifier
checks, `EventRequest.Clear`'s event kind, `StackFrame.GetValues`' tags),
all invisible to JDI because JDI validates first.

The C JVMTI table (`vm/src/jvmti/native_env.rs`: `SetEventNotificationMode`,
`SetBreakpoint`, `GetFrameCount`, `GetStackTrace`, `GetLineNumberTable`,
the capability functions, the event deliveries) has no such comparison. Its
error codes and phase checks are checked against the JVMTI specification by
reading, and by unit tests that call the table from Rust
(`jvmti::native_env::tests`), which pin what CratonVM answers, not what
HotSpot answers. HotSpot's native back ends are not in the JDK source tree
this repository keeps (`C:\craton\jdk25src` holds the Java sources only), so
reading cannot settle a disagreement either.

## Proposal

1. **One small C agent, built once per host**, under `tools/probes/jvmti/`:
   `Agent_OnLoad` reads an option naming a scenario, runs its rows at
   `VMInit` (and at the events the scenario enables), and prints one line
   per row (`<call>: <jvmtiError name>` or the values it read), exactly like
   the raw JDWP probes. A Java driver class per scenario provides the program
   under test.
2. **The runner learns `-agentpath`**: run the scenario under HotSpot
   (`java -agentpath:<lib>=<scenario> -cp out <Driver>`) for the reference
   transcript and under CratonVM in each mode, and diff, with the same
   allow-list.
3. **First scenarios**, each a row list:
   * `SetEventNotificationMode`: an invalid mode, an invalid event number, a
     thread-level enable of `VMInit` / `VMDeath` / `ThreadStart`, an enable
     without the capability, the start-phase refusal;
   * `SetBreakpoint` / `ClearBreakpoint`: a location past the end, a native
     method, a duplicate, a clear of one never set;
   * the stack functions on the current thread, on a suspended thread, on a
     running one (`THREAD_NOT_SUSPENDED`), and on a dead one;
   * `SingleStep` into a method an intrinsic answers (the positive control of
     wave 29's `METHOD_EVENTS_JVMTI_FRAMES` bit, which today only a Rust unit
     test observes).

## Cost

A C toolchain on the host (the Linux host has one; the Windows development
box may not), and a build step in the runner. Nothing in the VM.

## How to verify the harness itself

A scenario whose rows are known to agree (capability negotiation), and one
row known to differ, deliberately, must make the runner fail until the
allow-list names it.
