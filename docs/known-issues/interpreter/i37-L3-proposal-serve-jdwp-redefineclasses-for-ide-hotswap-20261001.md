# Proposal: serve JDWP `VirtualMachine.RedefineClasses` so an IDE can HotSwap

**Status: open (proposal, not implemented) — filed 2026-10-01 by interpreter
round i1 wave 37, lane L3. Wave 39 (lane L3): the retired i19-L3 page's last
item (the granted mode exit of a compiled body of a redefined class) is
folded in here; see "Progress (wave 39)".**

## Progress (wave 39) — lane L3: the i19-L3 remainder, folded in

`docs/internal/fixed-bugs/interpreter-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool-RETIRED-20261003.md`
is retired; its one open item becomes part of this proposal's work, since
this proposal is what makes it reachable.

**The item.** `jvmti_events::polling_body_must_leave` withholds a
JVMTI / JDWP mode exit (a step, a breakpoint, method events,
interpreter-only mode) from a compiled body compiled before its OWN class's
last redefinition (`class_was_redefined(..) && !compiled_since_last_redefinition`):
no sink can yet rebuild the interpreter frame from the body's own bytecode.
After an IDE HotSwap of a class whose method is running compiled (a hot
loop), a step in that activation therefore lands at its caller, where
HotSpot deoptimises the frame at the redefinition and steps it.

**What landing it needs, in order** (pieces of the retired page's "Progress
(wave 21)", with what changed since):

1. The artifact keeps its bytecode: done, `CompiledMethod::compiled_source`
   (round 13), with `compile_cp_stamp`.
2. The grant carries it: `jit_safepoint_loop_exit_verdict` hands the body's
   `compiled_source` to `PollingBody`, and `note_mode_exit_grant` keeps it in
   `ModeExitGrant` for a stale body; `polling_body_must_leave`'s
   redefinition clause then withholds only a stale body WITHOUT a source.
3. The sinks build from it (`helpers::try_resume_trapped_callee`,
   `deopt_resume::real_frame_deopt_resume_and_despeculate`): a grant naming
   the stash's point builds the frame from the grant's template and
   restamps it with `compile_cp_stamp` through
   `obsolete_frames::stamp_frame_rebuilt_from_compiled_code` (the wave-28
   piece 4, already used by the trap sinks), which moves it before it runs.

**Verification once this proposal lands:** a JDI scenario (`tools/jdi/`)
that HotSwaps a class with a renumbered pool while a compiled loop of it
runs, then steps in that activation: the step must stop in the loop (the
obsolete method, `Method.IsObsolete` true), and the loop must read its old
constants (`tools/probes/interp/L7/RedefineCompiledOldConstantsProbe.java`'s
shape).

## Problem, with evidence

Wave 29 scoped the redefinition pages by what real tools do and named the one
tool that renumbers a class's constant pool: an IDE's HotSwap, a debugger's
`RedefineClasses` with fresh `javac` output of an edited class. On CratonVM
that tool cannot run at all:

* `vm/src/debug/commands.rs`, `dispatch`: command set 1 has no arm for
  command 18 (`VirtualMachine.RedefineClasses`); it falls to the
  `NOT_IMPLEMENTED` default.
* `handle_vm_capabilities_new` answers `CapabilitiesNew` index 7
  (`canRedefineClasses`) and 8 (`canAddMethod`), 9
  (`canUnrestrictedlyRedefineClasses`) false, so JDI's
  `VirtualMachine.redefineClasses` throws `UnsupportedOperationException`
  before it sends anything, and IntelliJ / Eclipse / VS Code report "HotSwap
  is not supported" (the i1 JDI-command review recorded `canRedefineClasses`
  as refused by design:
  `docs/internal/fixed-bugs/interpreter-L1-jdi-reachable-jdwp-commands-that-still-answer-not-implemented-FIXED-20260927.md`).

So today the renumbering shape reaches CratonVM only through a Java agent
that calls `Instrumentation.redefineClasses` with `javac` output (HotswapAgent,
JRebel-style reloaders, the wave-37 probes). Everything the VM needs for the
debugger's case already exists behind that path: the all-or-nothing check of
a batch (`install_redefinitions`, `check_class_redefinition`), obsolete frames
with their own constants and lines (`obsolete_frames`), the redefinition
fence that keeps other threads from straddling a renumbered swap (wave 37),
withdrawn compiled bodies and their forced exits, `Method.IsObsolete`
(`Frame::runs_obsolete_method`).

## Design

1. **The command.** `VirtualMachine.RedefineClasses` (1/18): `classes` ×
   (`refType`, `classfile` bytes). The JDWP thread holds no `JvmThread`, so
   the install must run on a Java thread: the same shape the server uses for
   `ClassType.InvokeMethod` (a suspended thread runs the request), or a VM
   operation queued to a dedicated agent thread with a `NativeContextImpl`.
   Each definition goes through `check_class_redefinition` for every class
   first, then `redefine_class_as_instrument(.., retransform = false, ..)`
   (which keeps the retransformation base as JVMTI does), then the batch's
   loop-exit handshake (`begin/end_redefinition_batch`). Errors map to JDWP
   codes the way `RedefinitionRefusal::instrument_throwable` maps them to
   libinstrument's throwables (`INVALID_CLASS_FORMAT`, `FAILS_VERIFICATION`,
   `NAMES_DONT_MATCH`, `UNSUPPORTED_VERSION`, `ADD_METHOD_NOT_IMPLEMENTED`,
   `SCHEMA_CHANGE_NOT_IMPLEMENTED`, `HIERARCHY_CHANGE_NOT_IMPLEMENTED`,
   `DELETE_METHOD_NOT_IMPLEMENTED`, `CLASS_MODIFIERS_CHANGE_NOT_IMPLEMENTED`,
   `METHOD_MODIFIERS_CHANGE_NOT_IMPLEMENTED`).
2. **Capabilities.** `canRedefineClasses` true; `canAddMethod` and
   `canUnrestrictedlyRedefineClasses` stay false (the structural check
   refuses them, as HotSpot does without `-XX:+AllowEnhancedClassRedefinition`).
3. **Events and ids.** What a redefinition does to the debugger's method ids
   and to breakpoints set in a redefined method must be taken from HotSpot's
   JDWP agent and JVMTI sources (`C:\craton\jdk25src`) before this lands, not
   assumed: an obsolete frame's method must answer `Method.IsObsolete` true
   (the frame's obsolete mark exists), and JDI re-reads
   `ReferenceType.Methods` after a redefinition.
4. **Frames.** `StackFrame` of an obsolete activation: its location's method
   is the obsolete method; `Frame::runs_obsolete_method` already tells. JDWP
   `StackFrame.PopFrames` is not needed (IDE "drop frame" is separate,
   `canPopFrames`).

## What it unlocks

* HotSwap from every IDE, the main real-world source of pool-renumbering
  redefinitions; the i19-L3 pages' remaining items become reachable
  from a debugger (the granted mode exit of a compiled body of a redefined
  class, `polling_body_must_leave`'s redefinition clause:
  `docs/internal/fixed-bugs/interpreter-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool-RETIRED-20261003.md`).
* A JDI conformance scenario (`tools/jdi/`) can then drive the wave-37
  HotSwap probes' shapes with a real debugger.

## Cost and risk

Medium: the command runs Java-visible work (class checks, verification that
may load classes) from the JDWP server, so the thread it runs on matters; the
rest is existing machinery. No cost on any path without a debugger.
Owner: lane L1 (debugger).
