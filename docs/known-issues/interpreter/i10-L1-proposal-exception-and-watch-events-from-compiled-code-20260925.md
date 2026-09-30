# Proposal: post JDWP / JVMTI exception and field-watch events from compiled code

**Status: open — filed 2026-09-25 by interpreter round i1 wave 10, lane L1.**
A proposal (kept for triage), not a defect.

## Where it stands

Wave 10 delivers JDWP `Exception`, `FieldAccess` and `FieldModification`
events from the interpreter only (`vm/src/runtime/interpreter.rs`
`deliver_exception_event_if_armed`, `deliver_field_watch_if_armed`). So while
any such request is in force `debug::DebuggerGates::requires_interpreter`
answers yes for every method and the JIT's doors stand down VM-wide
(`interpret_all`, `debug::publish_debugger_gates`). jdb creates an
`Exception` request ("uncaught java.lang.Throwable") at start-up, so every jdb
session now runs interpreted from the first command on — correct, and on the
order of 10–50x slower than compiled code for hot loops.

HotSpot keeps compiled code running: with `can_post_on_exceptions` a
compiled throw goes through the runtime (`OptoRuntime::handle_exception_C`,
`SharedRuntime`), which posts the event; field watches deoptimize only the
methods that access a watched field (`JvmtiExport::post_field_access_by_jni`
and the compilers' `JvmtiExport::can_post_field_access` checks).

## Design

1. **Exceptions.** Compiled code already leaves a body through a small set of
   runtime doors when it throws or when an exception unwinds through it
   (`interpreter/exception_dispatch.rs` `route_jit_signal_exception`,
   `route_jit_exception_through_method`, the JIT's `jit_dispatch_threw`
   helper). Give those doors the same hook the unwinder has: report at the
   compiled throw site (the precise bci the reason-9 frame or
   `jit_local_athrow_pc` recovers), with the catch location computed over
   interpreter AND compiled frames (the compiled frames' exception tables are
   the method's own; `find_jit_exception_handler` answers "can this compiled
   frame resume at a handler"). Parking there needs the frame rebuilt as an
   interpreter frame first (the resumable deopt state of
   `interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md` stage 3);
   until that exists, a `SUSPEND_NONE` request (IDE exception breakpoints
   with "suspend: none", logging) can already be served from compiled code,
   and only suspending requests need `interpret_all`.
2. **Field watches.** Instead of `interpret_all`, stand the JIT down per
   method: the methods whose constant pools name a watched field (by
   `(declaring class, name, descriptor)` after resolution). Build the set when
   a watch is armed (a walk over the loaded classes' field refs; new classes
   checked at link), fold it into `DebuggerGates`' per-method set, and flush
   the inline caches as today.
3. Keep `interpret_all` as the fallback for requests the compiled path cannot
   serve (a suspending exception request before stage 3 exists).

## Staged plan

* Stage 0 (measure): time a jdb-driven run of the regression core under
  `--compatible` with the default exception request in force, against the
  same run without a debugger, to size the cost.
* Stage 1: field-watch per-method stand-down (2).
* Stage 2: non-suspending exception requests served from the compiled doors
  (1, first half).
* Stage 3: suspending requests from compiled code, on top of the L5 deopt
  work.

## How to verify

Unit tests beside `i1w10_l1_debugger_event_tests` driving a compiled body
(the jit_bridge test harness) that throws with an `Exception` request armed;
the event carries the compiled throw site. End to end: jdb against
`tools/probes/interp/L1/L1Wave10UnwindAndFields.java` with `catch
java.lang.IllegalStateException`: 254 stops (one per throw, the rethrow
included, as on HotSpot), and the run's
wall time with the default request close to the no-debugger time.

## Risk

Moderate: the compiled throw doors are GC-sensitive (reconstructed frames,
raw references); the hook must pin the exception as the unwinder's does.

## Progress (wave 17)

Interpreter round i1 wave 17, lane L1, 2026-09-26. **No code stage landed**;
re-deriving the stages from the current code found that stage 1 has an unmet
prerequisite and that stage 2 is five doors, not one. What was learned, so
the next lane starts from it:

* **Stage 1 (per-method field-watch stand-down) is not safe yet.** Standing
  the JIT down for only the methods that access a watched field keeps every
  other compiled body running, and those bodies reach compiled callees
  through baked direct `CALL`s (`CompiledMethod::_direct_callee_entries`) and
  cycle-edge cells that no door re-asks; an evicted callee's code stays
  mapped (`RetainedCode`) and keeps being called. A compiled caller running
  when the watch is armed would then run the watched field's accessor
  compiled and post nothing. `interpret_all` is correct today only because
  every compiled frame leaves at its next poll (wave 15; per body since
  wave 17) and the doors stand down VM-wide, so no compiled caller is left
  to make such a call. Prerequisite: stage 2 of
  `interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md` (a
  suspended state for cells, and retirement of baked direct calls into a
  method), plus the method set: a method is concerned when its own bytecode
  or any spliced body (`CompiledMethod::inlined_methods`) has a `get*`/`put*`
  whose field ref matches the watched field's name and descriptor — matching
  by name and descriptor alone over-approximates safely and needs no
  resolution (which could load).
* **Stage 2 (exceptions from compiled code) has five doors.** The source
  witness `helpers::every_compiled_catch_door_drains_the_leftover_native_return`
  lists every place where an exception thrown in compiled code is caught
  without reaching the interpreter's throw path (`settle_thrown_exception`),
  which is where the unwinder posts `Exception` today: the local-handler
  stubs' `jit_local_handler_lookup_body` and `local_handler_enter_implicit`
  (compiled code catches its own throw; the `JitLocalHandlerSite` carries the
  declaring class and throw bci, and the method comes from the compile-id
  mirror as the wave-17 poll does), `route_jit_exception_through_method`
  (an interpreter handler frame for the compiled method — posting after its
  `push_resumed_frame` would use the existing `deliver_exception_event_if_armed`
  with that frame and the precise `throw_pc`, park included, since the frame
  is an interpreter frame by then), `run_jit_callee_handler` (the frame is
  pushed inside `execute_resumed_frame`, which runs the handler to
  completion, so the post needs a hook there) and
  `jit_bridge::route_osr_exception_out_of_artifact` (the live OSR frame).
  Exceptions that escape every compiled frame are already posted, but at the
  interpreter caller's invoke pc instead of the compiled throw site. A
  cheaper first half: the two local-handler lookups decline (answer `-1`,
  which by their contract changes nothing but speed) while an exception
  client is armed for the VM, so every compiled catch goes through the three
  VM doors, which can post with an interpreter frame in hand.
* **JVMTI `Exception` is the case that loses events today.** JDWP's
  `Exception` request sets `interpret_all`, but the JVMTI `Exception` event is
  deliberately kept out of the interpreter-only union (`jvmti.rs`
  `UNION_EXCEPTION`, `has_interp_only_listener`), so an agent enabling it
  keeps compiled code running and receives nothing for a throw caught inside
  compiled code. Filed in wave 17, fixed in wave 18 (option 1):
  `docs/internal/fixed-bugs/interpreter-L1-jvmti-exception-events-lost-for-throws-caught-in-compiled-code-FIXED-20260925.md`.

Next stage, recommended: the local-handler decline plus the
`route_jit_exception_through_method` post (the door every declined compiled
catch of the single-pass tier reaches), with a source witness pinning that
each of the five doors either posts or declines, and a jit_bridge-harness
test that compiles a method throwing and catching its own exception with a
JVMTI `Exception` listener installed. This page stays open as a proposal for
the user to triage.

## Progress (wave 18)

Interpreter round i1 wave 18, lane L1, 2026-09-26. The JVMTI half's stop-gap
landed; **no compiled-door post landed**.

* A JVMTI `Exception` listener now puts its VM in interpreter-only mode
  (`vm/src/runtime/jvmti.rs` `EXCEPTION_EVENTS_NEED_THE_INTERPRETER = true`,
  folded into `has_interp_only_listener` and `UNION_INTERP_ONLY`), the JVMTI
  twin of JDWP's `interpret_all`. Page:
  `docs/internal/fixed-bugs/interpreter-L1-jvmti-exception-events-lost-for-throws-caught-in-compiled-code-FIXED-20260925.md`.
* That constant is the kill switch stage 2 flips: once every one of the five
  compiled catch doors posts (or declines to the interpreter) while an
  exception client is armed, set it to `false` and the JIT keeps running
  under an exception-listening agent. Flipping it any earlier loses the
  events of the doors that do not post yet.
* Why no door landed this wave: `run_jit_callee_handler` pushes its handler
  frame inside `execute_resumed_frame` and runs it to completion there, so
  its post needs a hook in `deopt_resume` (another lane's file); and a stage
  that posts from `route_jit_exception_through_method` alone is dead code
  while the constant is `true` and wrong while it is `false`. The next stage
  should land all three VM-door posts and the local-handler decline
  together, with the source witness and the jit_bridge-harness test
  described above, and flip the constant in the same change. JDWP's
  `SUSPEND_NONE` exception requests can reuse the same posts (stage 2 of this
  page).
* A sixth route to check when stage 2 lands: `interpreter.rs`
  `invoke_method_shared`'s `jit_early_exception` arm (a compiled first call
  that threw; the interpreter pushes the method's frame and searches its
  table with `jit_local_athrow_pc_in_frame` /
  `find_exception_handler_pc_unknown`). It posts `ExceptionCatch` but not
  `Exception`, and is not in the drain witness's door list; a throw raised
  inside the compiled body itself (not by an interpreted callee, whose own
  unwinder already reported it) reaches its handler there unreported.

## Progress (wave 21)

Interpreter round i1 wave 21, lane L2, 2026-09-26. **Stage 2's doors
landed; the kill switch stays `true`.**

What landed:

* **One reporting helper.** `interpreter.rs`
  `report_exception_caught_by_compiled_door(shared, thread, frame_idx,
  throw_pc, handler_pc)`: once a door's handler frame is on `thread.frames`
  with the throwable on its operand stack, it posts `Exception` at the
  compiled throw site (JDWP and JVMTI through the unwinder's own
  `deliver_exception_events`, once per throw, a JDWP park included) with the
  door's known handler as the catch location (no prediction; new
  `known_handler` parameter), then JVMTI `ExceptionCatch`, then forgets the
  reported exception so a rethrow is reported again. `throw_pc ==
  usize::MAX` (the door does not know it) is reported as the `start_pc` of
  the row whose handler runs (`compiled_catch_report_pc`). An exception an
  interpreted callee threw was reported by its own unwinder and is not
  reported twice (`JvmThread::reported_exception`). One difference from the
  unwinder's post: the frame already sits at its handler, so a JVMTI agent
  that asks the frame's location during the callback reads the handler pc
  and the handler's operand stack, while the event carries the throw bci (a
  JDWP park publishes the throw bci as the top frame's location, as the
  unwinder's does).
* **Every door calls it.** `exception_dispatch.rs`
  `route_jit_exception_through_method` (after `push_resumed_frame`);
  `run_jit_callee_handler`, whose frame is now pushed by
  `deopt_resume::execute_resumed_handler_frame` (push, report, run);
  `jit_bridge::route_osr_exception_out_of_artifact` (replacing its own
  `ExceptionCatch` + forget, so `Exception` now precedes `ExceptionCatch`);
  and the first-call door's `jit_early_exception` arm of
  `interpreter::execute` (the "sixth arm"; this page called it
  `invoke_method_shared`). `route_jit_exception_through_method` and
  `run_jit_callee_handler` posted no `ExceptionCatch` before either: an
  agent enabling `ExceptionCatch` alone now hears those catches (a
  behaviour change only with such an agent, the lost events being the bug).
* **Escapes with a precise bci.** The OSR door's no-handler arm and the
  first-call arm's no-handler arm (when the body stamped a throw site) post
  `Exception` at the compiled throw bci before the frame is left, so the
  unwinder, which would report it at the frame's stale pc
  (`OSR_FRAME_DECLINED_TO_CATCH`) or the caller's invoke pc, finds it
  reported.
* **The local-handler decline.** `helpers::jit_local_handler_lookup_body`
  answers `-1` (propagate, every signal left as found) while
  `interpreter::compiled_catch_events_armed(vm)` holds — this VM's JDWP
  gate, a JVMTI `Exception` listener OF THIS VM
  (`exception_events_may_be_armed`, `jvmti::exception_listener_active_for_vm`,
  one union load when no VM listens, so another VM's agent does not slow
  this one) or a JVMTI `ExceptionCatch` listener of this VM (new flag and
  union, `jvmti::exception_catch_listener_active_for_vm`;
  `docs/internal/fixed-bugs/interpreter-L2-jvmti-exception-catch-lost-for-compiled-local-catches-FIXED-20260925.md`)
  — before either of its arms can catch; the throw then reaches one of the
  doors above.
* Tests: `interpreter::i1w21_l2_compiled_catch_event_tests`
  (`a_compiled_catch_posts_exception_then_exception_catch`,
  `an_exception_reported_by_an_interpreted_callee_is_not_reported_again`,
  `another_vms_listener_does_not_arm_this_vm`,
  `an_exception_catch_listener_alone_arms_the_decline`, and the source witness
  `every_compiled_catch_door_reports_the_catch`, which pins each door's
  call, the escape posts and the decline's position before both
  local-handler arms). No bench: no fast path changed (the doors are
  exception paths; unarmed, each pays the loads it already paid for
  `ExceptionCatch` plus one).

Why `jvmti::EXCEPTION_EVENTS_NEED_THE_INTERPRETER` stays `true`: with it
`false` no event would be LOST any more, but some would be reported at the
wrong place, which the interpreter-only mode reports exactly:

1. **A throw in a compiled callee reached through a baked direct call** (or
   a cycle cell) is first seen by the CALLER's door or post-call check, so
   it would be reported at the caller's call-site bci, in the caller's
   method. HotSpot reports the callee's throw bci. Needs a post at the throw
   site in compiled code: the throwing helpers (`jit_throw_exception`, the
   implicit-signal materialisers, `jit_set_throw_bci`'s callers) know the
   bci but not the method; the compile-id mirror (`emit_frame_record_identity`,
   as the wave-17 poll uses it) gives the body. Posting there needs a frame
   the event can name — JVMTI takes a method id and a location, which the
   body's `CachedBytecodeMethod` gives without an interpreter frame, so
   `post_jvmti_exception` would need a frame-less variant; JDWP's park needs
   a frame, so only non-suspending requests could be served (the stage-2 /
   stage-3 split above).
2. **The predicted catch location skips compiled frames.** For a throw the
   unwinder reports in an interpreted callee of a compiled frame,
   `predicted_catch_frame` walks interpreter frames only, so a catch in the
   compiled caller is reported as an outer frame's handler or as uncaught.
   Needs the compiled frames, with their method and current bci, in the walk
   (whatever compiled-frame walk stack traces use would have to answer both).

Next stage: (1) and (2) above, then flip the constant, with a jit_bridge-
harness test that compiles a caller/callee pair with a JVMTI `Exception`
listener and checks the reported location and catch location against the
interpreter's. This page stays a proposal for the user to triage.
