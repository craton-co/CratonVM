# i124-L9: a compiled-home write can be applied to the wrong frame

Interpreter round i6, wave 124, lane L9. This is a cross-lane review of wave 123, lane L2: stage 3b
of `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`, which makes
`StackFrame.SetValues` work on a compiled activation.

## Status

FIXED in code, unbuilt (wave 124, lane L2). All three fixes below are in, under the same switch
and cfg. Move this page to `docs/internal/` once the orchestrator's build and the checks in
"Wave 124 (lane L2): the fix" pass.

Original status: OPEN. Routed to the lane that owns `vm/src/debug` and `jit/src/deopt.rs`; both files were being
edited during wave 124. The code is behind `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL` (default off)
and only builds with `--features experimental-debug`, so the default path is unchanged. Nothing
was built or run.

## What is wrong

`inspect::apply_compiled_home_writes` runs inside `deopt_resume::build_deopt_frame_or_refusal`.
It does not get the activation from the frame being built. It reads
`cratonvm_jit::deopt::last_taken_frame()`, which is whatever this thread last popped from a deopt
stash (`pop_stash_entry`). It then accepts a pending write when `compiled_home_write_matches`
holds:

- the frame bases are equal;
- the write's exit bci equals the frame's `pc`;
- **and either** the bodies are equal (`cm_ptr`), **or**, only when the stash named no body, the
  frame's class and method are equal.

Two gaps follow.

1. **A stale take.** `build_deopt_frame_or_refusal` has five callers:
   `deopt_resume.rs` `build_deopt_frame_inner` (which also serves the GC-stress rebuild) and two
   more in that file, `interpreter.rs` around line 5895, and `jit/helpers.rs` around line 13101.
   Nothing ties the frame being built to the most recent pop, so one caller that builds from a
   frame it got some other way (a peek, a constructed frame) leaves `last_taken_frame` describing
   an earlier pop. Wave 124 did not audit every caller, so this is unproven but nothing prevents
   it. Suppose that earlier pop was an
   activation of the same body, at the same frame base and the same exit bci, as the activation
   now holding a pending write. This happens when a caller calls the method repeatedly at one
   stack depth. Then a rebuild of an unrelated frame whose `pc` equals that bci gets the write.
   With a body named, class and method are never compared, so the write can even land in another
   method's locals.
2. **A reused body address.** `last_taken_frame` upgrades a `Weak`, and `CompiledHomeWrite::body`
   keeps a bare `cm_ptr`. A write is not applied if its activation leaves some other way, such as
   unwinding by an exception or a `PopFrames`. It is then only dropped when the thread exits
   (`discard_deferred_frame_writes_if_any`), and it keeps its object pinned until then. After the
   old body is freed, a recompiled body can sit at the same address. A later activation of that
   body, at the same frame base, leaving at the same exit bci, then matches and receives the old
   write.

## Fix

- In `compiled_home_write_matches`, always compare `class_id` / `method_id`, not only when
  `body` is `None`. The frame being built is the authority.
- Make the match consume what it read. `apply_compiled_home_writes` (or a `deopt.rs` helper)
  should clear `TakenEntry::last_frame` after reading it, so a later build that popped nothing
  sees `rbp == 0` and returns early.
- Drop a pending write when its activation can no longer reach the exit. The cheapest test is at
  the next publish of the thread's snapshot: no `compiled_home_rows` entry with the write's
  `(cm_ptr, frame_rbp)` and the thread not parked in that call means the write goes. That also
  releases its pinned object.

## Also noted

`apply_compiled_home_writes` takes the debug-state lock inside the deopt resume's no-collection
window. That is a new nesting (deopt resume, then `debug_state`), reached only while a write is
pending. It is worth a line in the lock-order notes, or the read can move to after the frame is
pushed.

## Wave 124 (lane L2): the fix

FIXED in code, unbuilt. Still behind `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL` (default off); the
VM half only builds with `experimental-debug`.

1. **Class and method always.** `inspect::compiled_home_write_matches` now requires
   `w.class_id == class_id && w.method_id == method_id` in every case; a body the stash names
   must also equal the write's `cm_ptr` (`body.is_none_or(..)`). Test
   `w123_l2_compiled_home_write_tests::a_write_matches_its_own_activation_only` now refuses a
   named body on another method's frame.
2. **The take is consumed.** New `cratonvm_jit::deopt::take_last_taken_frame()` reads
   `TakenEntry::last_frame` and resets it to `(0, None, 0)`; `apply_compiled_home_writes` reads
   through it. New `clear_last_taken_frame()`, called by `set_compiled_home_values` on the owner
   thread right after it records a write, so a pop made before the record cannot match it either.
   Test: `x64_deopt_entry_tests::the_frame_base_rides_beside_the_stash_through_a_take_and_restash`
   (extended: the consuming read answers once, then nothing; the clear empties it).
3. **Unlisted writes are dropped.** New `inspect::drop_unlisted_compiled_home_writes`, called at the
   end of `interpreter::publish_debugger_frames_under` while the gate is up: a pending write of the
   thread whose `(cm_ptr, frame_rbp)` is not among the bodies the capture just read homes for
   (`BlockedCompiledView::homes`) is dropped and its object released
   (`[COMPILED_SETVALUES] dropped tid=<n> writes=<k>` under `CRATONVM_FRAME_TRACE=1`). Only that
   publish drops: it is the only capture that reads homes, and every activation that took a write
   was read that way, so a live one is listed again. A native-exit park's publish
   (`jvmti_events::park_if_suspended_at_native_exit`) reads no homes and drops nothing; a write
   whose activation unwound meanwhile waits for the thread's next debugger park (or its exit).
   This also bounds gap 2: a write cannot outlive its activation past one park, and the force
   made the body not entrant, so no new activation of that body starts.

**The five `build_deopt_frame_or_refusal` callers** (audit of gap 1):
* `jit/helpers.rs` (`try_resume_trapped_callee`, about line 13101): `rframe` comes from
  `take_last_deopt_with_source()`, a pop of this thread's ordinary stash just before.
* `interpreter.rs` (the first-call tier-up sink, about line 5895): `rframe_for_despec` comes from
  the door's take (`jit_bridge`'s `take_last_deopt_with_point` /
  `take_last_deopt_with_point_pushed_after`, both through `pop_stash_entry`).
* `deopt_resume.rs` `resume_real_ir_deopt_or_throw` (line 6472): reached from
  `real_frame_deopt_resume_or_throw_and_despeculate`, whose `rframe` the `jit_bridge` doors took
  the same way.
* `deopt_resume.rs` `build_deopt_frame_inner` (line 3884): unit tests only, with frames they
  construct (no pop). With the consumed take they now read 0 and apply nothing.
* `deopt_resume.rs` line 16323: a unit test.

So every production build follows its own pop on this thread. The one take that does not go
through `pop_stash_entry`, `take_exceptional_frame_with_point_and_discards`, leaves
`last_frame` as it was; a handler frame built from it now meets a consumed (0) or a cleared
record, and in any case a different bci, class or method.

**The lock nesting.** `apply_compiled_home_writes` takes `debug_state` inside the build's
no-collection window (the frame is not pushed yet). The nesting deopt resume -> `debug_state` is
new. It is safe under the rule the debug state already follows:
* The resume holds no Rust lock when it calls the apply. The build's pins and relocked Java
  monitors are not locks a `debug_state` holder waits on.
* No `debug_state` holder waits for a safepoint or a collection. The park loop releases it
  (`await_state_change`), and `set_compiled_home_values` drops it before the force.
* A mutator blocked on the mutex is not at a safepoint. That is true of every mutator that
  already takes `debug_state` between bytecodes (`early_return::take`, `enter_point`), so it
  adds no new hazard.
* The window only means no collection can run before the push, and the apply allocates nothing.
* The apply runs only while a write is pending (one load otherwise).

If a later change makes a `debug_state` holder wait on a safepoint, both this site and those
others deadlock, so the rule belongs in the lock-order notes.

**Check:**
* `cargo test -j 5 -p cratonvm-jit --lib x64_deopt_entry_tests`
* `cargo test -j 5 -p cratonvm-vm --features experimental-debug --lib w123_l2_compiled_home_write_tests`
* `cargo check -j 5 -p cratonvm-vm` with and without `experimental-debug`
* `tools/probes/interp/L2/L2W124JdiCompiledCallerSetValues.java` with the switch on: the
  `[COMPILED_SETVALUES] applied` lines still appear, and no `dropped` line appears.
