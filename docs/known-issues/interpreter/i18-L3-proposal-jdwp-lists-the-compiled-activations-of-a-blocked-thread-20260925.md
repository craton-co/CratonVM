# Proposal: JDWP lists the compiled activations of a thread blocked under compiled code

**Status: open — filed 2026-09-25 by interpreter round i1 wave 18, lane L3.
Stages 1-3 landed in wave 21 (lane L3). Wave 22 (lane L1): the -1 location
is gone, a native called from compiled code is listed, and stage 4 is
specified against the tree ("Wave 22 note").**

## Where things stand

A thread the debugger suspends while it is blocked in a native method is
listed from its interpreter frames (`interpreter::publish_blocked_frames`,
`read_blocked_frames`, `publish_frame_snapshot`; waves 10-15). When the
native was called from COMPILED code, that list is wrong in two ways
(`interpreter-L1-jdwp-suspension-does-not-reach-compiled-or-native-code-FIXED-20261005.md`,
item 2):

1. **Missing frames.** The compiled activations between the top interpreter
   frame and the native have no `Frame`, so they are not listed; the top
   interpreter frame is reported at its `last_instr_pc` (the invoke that
   entered compiled code), and `blocked_native_method` names no native frame
   (the invoke there is the compiled method's).
2. **Stale locals.** A single-pass OSR body keeps its method's locals in its
   own slots, so the OSR'd interpreter frame's `Frame` still holds the values
   of OSR entry, and it is reported at the back edge that entered OSR.
   `StackFrame.GetValues` on it answers those stale values.

A thread RUNNING compiled code is not affected: it publishes nothing until it
parks, and since waves 12-18 a suspension makes its loop bodies leave at
their next back-edge poll (then it parks in the interpreter with exact
frames).

## Design

The thread itself knows its compiled activations:
`jit::conservative_roots::active_compiled_frames()` (the stack walker's
interleave source) gives, innermost first, each activation's artifact label,
owner class id, current bci (from its published safepoint id; `-1` when none)
and the interpreter depth it was entered at.

1. **Capture on the blocking transition** (own thread, where the TLS chain is
   readable), only while a debugger is attached and only when
   `conservative_roots::current_thread_jit_depth() > 0`: store the list in the
   thread's `GcBlockState` beside `debugger_inspect` (a `Mutex<Vec<..>>`,
   emptied on leave) — `open_blocked_inspection` already runs there.
2. **Publish interleaved.** `publish_frame_snapshot` takes the list and
   splices each activation above the interpreter frame it was entered from:
   `(owner class id, jdwp_method_id(name, descriptor), bci)`, its locals
   recorded as unavailable; an OSR activation REPLACES its interpreter frame's
   location (the body's bci, not the entry back edge) and withholds that
   frame's locals.
3. **Commands.** `StackFrame.GetValues` / `SetValues` on a compiled frame
   answer `OPAQUE_FRAME` (32) — what HotSpot answers for a frame it cannot
   describe — instead of stale values. `ThisObject` likewise.
4. **Exact locals, later.** Values need a deoptimisation state at the call
   return the activation is standing at (the i9-L5 page's stage 3: a
   `DeoptimizationPoint` per call site); with it, the compiled-frame oracle can
   rebuild each activation's locals exactly as a deopt would, and step 3's
   `OPAQUE_FRAME` goes.

## Expected benefit

`jdb where` on a thread sleeping or waiting under a compiled method lists the
compiled methods and the right line of the OSR'd one, and no command answers
a stale value.

## Staged plan

1. Stages 1-3 (VM only; `vm/src/runtime/interpreter.rs`, `debug/`), behind the
   attached-session gate; cost is paid only by blocking calls made under
   compiled code while a debugger is attached.
2. Stage 4 with the jit round's call-return deopt state.

## How to verify

A unit test like
`interpreter::i1w10_l1_debugger_event_tests::a_blocked_thread_publishes_its_frames_while_a_debugger_is_attached`
with a fabricated compiled-activation list: the snapshot interleaves it, and
`GetValues` on a compiled frame answers `OPAQUE_FRAME`. By hand: `jdb` against
a program whose OSR'd loop calls `Thread.sleep`; `suspend`, `where`, `locals`.

## Progress (wave 21)

Interpreter round i1 wave 21, lane L3, 2026-09-25. **Stages 1-3 landed**,
for a thread blocked in native code AND for a thread parked at an interpreter
suspend point (the second case had the same two defects and the same fix);
stage 4 (exact locals) is next and needs the jit round's call-return deopt
state.

* **Capture, on the thread itself.** `vm/src/runtime/interpreter.rs`
  `capture_blocked_compiled_view(frames)`: one thread-local read
  (`conservative_roots::current_thread_jit_depth`) when no compiled code is
  active; otherwise `active_compiled_frames()` spliced exactly as a stack
  trace splices it — the same dedupe (`stackwalker::drop_osr_continuations`,
  now `pub(crate)`) decides which compiled entry is the same activation as an
  interpreter frame. The result is plain data
  (`threading::jvm_thread::BlockedCompiledView` / `BlockedCompiledRow`: class
  id, `jdwp_method_id`, bci, and the interpreter frame each row is listed
  below; plus the frames whose body runs compiled, with the compiled half's
  bci when the dedupe's authoritative arm has it). Inlined callees are listed
  as frames too, outermost first, as a stack trace lists them.
* **Where it is taken.** Eagerly in `publish_blocked_frames_if_suspended`
  (already suspended as it blocks) and in `publish_debugger_frames` (every
  park at a suspend point); lazily in `open_blocked_inspection`, which stores
  it in the new `GcBlockState::debugger_compiled` before the window opens
  (only under compiled code), where `read_blocked_frames` reads it under the
  window's READING flag; `close_blocked_inspection_slow` clears it once no
  reader holds the window.
* **Publish interleaved.** `publish_frame_snapshot(.., compiled, ..)` lists
  each row at its place (location -1 when the activation's bci is unknown),
  lists a frame whose body runs compiled at the compiled half's bci, and
  records both in the new `DebugState::opaque_frames` (`true` = a compiled
  activation, `false` = a stale interpreter frame). No native frame is listed
  above a compiled one (the top interpreter frame's invoke is the compiled
  method's, not the native's). `withdraw_debugger_frames` and the detach
  forget the ids.
* **Commands.** `StackFrame.GetValues` (`commands::frame_has_no_readable_locals`),
  `SetValues` (`inspect::set_frame_values`) and `ThisObject` of a non-static
  method answer `OPAQUE_FRAME` (32) for both kinds instead of stale values.
  `set_frame_values` now finds its `Frame` among the interpreter frames by
  skipping the frames that have none (a native frame, a compiled row) rather
  than by the frame id. A step's starting depth and line count interpreter
  frames only (`commands::is_compiled_frame`), so a step completes exactly
  where it did before.

Cost: nothing without a debugger (the gates already in front of these paths);
with one, a JIT-chain walk per blocking call made under compiled code, and per
park.

Test: `interpreter::i1w10_l1_debugger_event_tests::a_blocked_threads_compiled_activations_are_listed_without_locals`
(a fabricated view: the listing interleaves it, the OSR'd frame is at its
compiled bci, `GetValues` / `ThisObject` answer `OPAQUE_FRAME` for compiled
and stale frames and the values for the others, no native frame above a
compiled one, the capture is empty without compiled code, the withdrawal
forgets the ids). The capture itself walks a real JIT chain and is left to
the by-hand check below, since a unit fixture runs no compiled code. By hand:
`jdb` against a program whose OSR'd loop calls `Thread.sleep` (or hits a
breakpoint in an interpreted callee); `suspend`, `where`, `locals` in the
compiled frames.

**Next: stage 4.** Exact locals for the compiled activations and the OSR'd
frame need a deoptimisation state at the call return each activation stands
at (`interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md`,
stage 3); with it the compiled-frame oracle rebuilds the locals as a deopt
would, and `OPAQUE_FRAME` goes. Also open: a compiled activation whose
safepoint id names no bci is listed at -1, which `jdb` shows as a native
method. This proposal stays open for the user to triage.

## Wave 22 note

Interpreter round i1 wave 22, lane L1, 2026-09-26.

**Landed.**

* The -1 location. JDI does not show such a frame as native: for a
  concrete method `LocationImpl.lineNumber()` reaches
  `ConcreteMethodImpl.codeIndexToLineInfo`, which throws
  `InternalError("Location with invalid code index")` for an index outside
  the method's code (JDK 25 sources), so `jdb where` failed for the whole
  thread. A row of unknown bci is now listed at code index 0
  (`interpreter::push_compiled_row`), still opaque.
* A native called from compiled code is listed above it, named from the
  innermost compiled activation's invoke and confirmed by the running-native
  record (`interpreter::blocked_native_top`); see the i9-L1 page's
  "Progress (wave 22)".

**Stage 4 (exact locals), specified — not landed.** It does not need a
resumable call-return deopt state (that is for *writing* a frame, or resuming
it interpreted); *reading* a stopped activation needs only a description of
where each local lives at the call it stands in, which the single-pass
backend can record. What the tree has and lacks, read in wave 22:

1. **Where values are at a call.** A frame-homed local lives in its
   canonical slot `[rbp - (idx+1)*8]` (`Compiler::local_offset`) and every
   store writes it. A GPR-homed local (callee-saved RBX / R12-R15, plus
   RSI / RDI on Windows; `callee_saved_gpr_local_homes_enabled`, default on)
   is copied to that slot before a safepoint call only when it may hold a
   reference (`x64/safepoint.rs` `emit_pre_safepoint_spill_impl`, the
   `register_homed_reference_locals` filter of the publication plan), and the
   self-call elision (`can_elide_self_call_register_spill`) skips even that.
   The blind spill of used callee-saved GPRs (SB-CRASH-04) is a second copy
   but has its own elisions (args-published, the TLAB sink). XMM homes exist
   only on Windows (`xmm_local_homes_enabled`) and are never spilled at a
   call. So: under `BackendRequest::debugger_observes_locals` (debugger
   compiles only; the same gate wave 21 uses), publish EVERY register home
   at every call that can leave compiled code — the loop is already there,
   drop its reference filter and add an XMM store — and do not take the
   self-call elision.
2. **The description.** At each such call, record a locals vector built
   like `build_frame_state_at` (kinds, oop mask, the kept-assigned rule of
   wave 21) but with slot-only provenance: every local described by its
   canonical slot (`StackSlot` / `StackSlotLong` / `StackSlotFloat` /
   `StackSlotDouble` / `StackSlotRef`), `Undefined` for an unassigned one,
   and the whole description dropped (the frame stays opaque) for a
   scalar-replaced local (`sr_here`), an `Ambiguous` kind the per-bci pass
   cannot settle, or an inline splice's callee locals (their homes are the
   callee scope's, `inlining.rs`). Key it by the call's return-address
   offset (the key `InlineFrameMap` rows use) and by the call's safepoint id
   (the innermost activation has no return address on the walk; its id slot
   is current, `conservative_roots::activation_bci`'s doc).
3. **The read.** `ActiveCompiledFrame` gains the activation's RBP and return
   address (the walk in `conservative_roots::active_compiled_frames`
   computes both and drops them). `capture_blocked_compiled_view`, which
   already runs on the stopping thread itself while it is still a mutator,
   reads each described slot into a `Value` (a reference is current: nothing
   has moved since the call, and `publish_frame_snapshot` pins the exported
   id) and carries it in `BlockedCompiledRow`; `publish_frame_snapshot`
   publishes those locals and leaves the row out of `opaque_frames`. An OSR'd
   interpreter frame (a `stale` entry) is read the same way from its compiled
   half's activation. `SetValues` stays `OPAQUE_FRAME` (a write needs the
   deopt HotSpot does).
4. **Hazards to validate with a build, each with a positive control.** A
   loop transform (LICM, the unroller's replicated pcs, the matrix-dot
   pre-header's R12-R15) holding a local in a scratch register across a
   call; the IR tier (separate: it has its own frame states and no
   publication); a callee inlined at the call (its locals); the Windows XMM
   homes. A JDI probe on the probe host that stops a callee of a compiled
   loop at a breakpoint and reads the loop's locals `up` one frame, compared
   with HotSpot, is the acceptance test.

Cost: one store per register-homed local per call in a debugger's compiles,
nothing otherwise; a slot-description vector per call site in those
artifacts.
