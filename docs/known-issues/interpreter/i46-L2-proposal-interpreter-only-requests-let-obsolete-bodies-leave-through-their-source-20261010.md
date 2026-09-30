# Proposal: an interpreter-only request lets an obsolete compiled body leave through the bytecode it was compiled from

**Status: proposal — filed 2026-10-10 by interpreter round i1 wave 46, lane
L2, after building
`docs/internal/fixed-bugs/interpreter-L2-proposal-method-entry-obsolete-bodies-leave-through-the-rebuild-sinks-FIXED-20261010.md`.
Not built. The most promising compile-door / compiled-dispatch direction
the round leaves: it closes the last "obsolete body" refusal of the
safepoint verdict with machinery that exists since wave 46.**

## The problem it removes

A JVMTI interpreter-only event (`MethodEntry`, `SingleStep`, `FramePop`,
...) or a JDWP step / method event / breakpoint needs every affected
compiled frame in the interpreter. The safepoint verdict refuses one kind of
frame outright: a body compiled BEFORE its class's last redefinition
(`jvmti_events::polling_body_must_leave`, the `class_was_redefined ... &&
!compiled_since_last_redefinition` clause, and the process-wide form
`every_compiled_frame_may_leave`'s `!any_class_redefined()`). The reason
written there is that "no sink resumes such a frame": the sinks refused a
frame of a redefined class and re-ran the method from entry. So after any
agent ever retransformed a class, an IDE debugger's step or breakpoint in a
method of that class leaves its running compiled activations compiled until
they return (`i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-20261005.md`,
shape 1 (b)), and `every_compiled_frame_may_leave` stops answering for the
whole process.

Since wave 46 the sinks CAN resume such a frame: a grant that carries the
body's compiled source and pool stamp
(`jvmti_events::granted_obsolete_source`) makes the interpreter doors'
sink and the call-site service rebuild the frame in its own bytecode and
restamp it (`jit_bridge::granted_obsolete_activation_source`). Only the
renumbering redefinition's own force pass uses it.

## The idea

* In `helpers::jit_safepoint_loop_exit_verdict`, attach the obsolete source
  (`PollingBody::with_obsolete_source`) to ANY polling method-entry body
  whose class was redefined since it was compiled and that
  `CompiledMethod::entry_resumable_as_obsolete` admits (make it `pub`), not
  only one the force pass marked; memoise the translation answer as wave 46
  does (`renumbered_entry_resumes`, renamed for its wider use).
* In `polling_body_must_leave`, let such a body leave when the source
  translates, instead of the blanket refusal; the grant then carries the
  source, and the existing sinks resume the exit in it.
* `every_compiled_frame_may_leave` keeps its process-wide refusal for
  bodies the poll does not name (no source to carry).

## What to check first

* The interpreter-only withdrawal forces the exits of such a body at all
  (`JitRealm::withdraw_every_body_for_the_interpreter` and the breakpoint
  withdrawals): an own-class body a redefinition spared is marked
  `exits_spared_as_obsolete` and may be skipped by a later force pass.
* The OSR bodies: their exits transfer in place and need no source, but the
  same clause refuses them in `polling_body_must_leave`; check whether the
  OSR door's transfer of an obsolete frame converts it first (wave 45's
  `convert_obsolete_frames_if_redefined` after the exit).
* A probe: a JDWP breakpoint (or a JVMTI `MethodEntry` agent) set in a
  method of a class retransformed earlier, while a compiled loop of that
  class runs; HotSpot deoptimizes the frame and the event fires. Positive
  control: `withdrawn body told to leave:` or the verdict's own line naming
  the body, then the sink's `resuming in the bytecode the body was compiled
  from` line.

## What it would cost

The verdict slow path of a body whose class was redefined asks one
memoised translation per body; nothing on a poll's fast path, a call or a
bytecode.
