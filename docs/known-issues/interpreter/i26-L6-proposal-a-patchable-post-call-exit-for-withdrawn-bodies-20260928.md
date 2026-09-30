# Proposal: a patchable post-call exit for compiled bodies a redefinition withdrew

**Status: stages 1-2 landed for the single-pass tier's OSR compiles (wave 27,
lane L6), stage 3 for the optimizing tier's OSR-door compiles (wave 28, lane
L6); a positive control exists since wave 28 but neither tier's site has
fired on the host yet; method-entry bodies of both tiers open — filed
2026-09-28 by interpreter round i1 wave 26, lane L6 (the first "What remains"
item of
`docs/known-issues/interpreter/i24-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-20260927.md`).**

## Progress (wave 28) — lane L6

* **The wave-27 probe was not a control for the single-pass sites.** A
  single-pass OSR compile splices no callee (`compile_osr_body` hands the
  backend an empty `inline_sites`), so `RedefineSpliceAfterTheCallProbe` under
  `CRATONVM_JIT_OSR_OPTIMIZING=0` reads the new `Callee.value()` through a
  real call whatever happens; the one dependency such a body copies is an
  elided empty constructor. The control is
  `tools/probes/interp/L6/RedefineElidedCtorAfterTheCallProbe.java` (traced
  path and details in the i24 page's "Progress (wave 28)").
* **Seeing it fire.** `CRATONVM_DBG_DEOPT=1`: `post-call exit verdict #n:
  <label> id=.. named=.. withdrawn=.. verdict=..` per verdict a forced site
  asks for (`vm/src/jit/helpers.rs::note_post_call_exit_verdict`), counted per
  VM in the `JitCache` (`note_post_call_exit_verdict`) and summed on
  `CRATONVM_DBG_JITC=1`'s `exit polls forced:` line
  (`post-call-verdicts-so-far= post-call-exits-so-far=`).
* **Stage 3, the optimizing tier, landed for OSR-door compiles**
  (`jit/src/ir_lower.rs`, section "Post-call exit sites"), without the three
  steps listed below: no snapshot is kept that was not kept before, and the
  colouring is untouched. The exit's state is the INVOKE's snapshot (which
  survives pruning because a call can deopt) with the arguments popped and the
  result pushed, at the successor bci; `plan_slots` pins every value a kept
  snapshot names to a colour of its own, so the hazard of point 3 does not
  arise for them, and the result is described right after the call node
  stored it. Point 2's "current at the site" is checked per value (constant,
  parameter, or emitted in / a φ of a block dominating the call's); a frame
  holding a monitor, an unrolled copy without its own snapshot, or a value
  with no location refuses the site. The point is an `OsrExit` box kept out of
  the OSR admission facts (as the poll exits' are); the stub is `CALL tail; JZ
  back; MOV DEOPT_ARG0, point; JMP <shared deopt stub>`, the tail the
  single-pass one. Cost: a five-byte `NOP` (plus a pad at most one site in
  two) after each real call of an optimizing OSR body, ~30 bytes of cold stub
  per site, one ~170-byte tail, one boxed point per site; no instruction on
  any path until a redefinition forces a site. Kill switch:
  `ir_lower.rs::Lowerer::IR_POST_CALL_EXITS_ENABLED`. Side effect to know:
  `osr_exit::has_poll_mode_exits` answers `true` for a body that has only
  post-call boxes, so the OSR door counts it in `osr_bodies_running` (the
  conservative direction: at most a loop-exit pause that could have been
  skipped). Test:
  `ir_lower::i28_l6_post_call_exit_tests::an_osr_door_compile_puts_a_forceable_exit_after_a_call`.
* **Remaining:** method-entry bodies (both tiers; the single-pass reason is
  "Why OSR compiles only" below; for the optimizing tier the gate is
  `maybe_emit_post_call_exit_site`'s `IrPollModeExits::OsrDoor` test, and
  admitting `MethodEntry` needs the same look at what a new `OsrExit` box
  costs a method-entry body's callers first), an optimizing frame holding a
  lock at the call, a real call made INSIDE a splice (the optimizing tier
  splices small callees without an exception table -- `CountDownLatch.await()`
  is one -- and the frame the inner call returns into is the splice's; the
  natural site is the END of the splice, at the enclosing invoke's successor,
  which needs the IR builder to mark that point with a pinned node the
  lowerer can hang a site on: an `ir.rs` change, lane L2), and the host run of
  both tiers' controls.

## Progress (wave 27) — lane L6

**Stages 1-2, single-pass OSR compiles.** Traced path of the probe's loop when
it runs a single-pass OSR body: the redefinition (`vm_exec.rs::redefine_class_with`
→ `JitRealm::redefine_and_flush` → `force_withdrawn_exit_polls` →
`JitCache::force_withdrawn_exit_polls` → `CompiledMethod::exit_poll_writes` /
`commit_exit_poll_writes`, `jit/src/not_entrant.rs`) now rewrites the body's
post-call sites with its polls; the parked thread's `await()` (or the
redefining thread's `retransformClasses`) returns through the dispatch
helper and the post-invoke check into the site (`JMP` to its stub) → the
shared verdict tail → `jit_safepoint_slow_path(flag, id |
POST_CALL_EXIT_VERDICT_ONLY)` (`vm/src/jit/helpers.rs`, no park) →
`jit_safepoint_loop_exit_verdict` → `withdrawn_body_may_leave` → the verdict
bit → `JNZ` to the successor's reason-7 stub → the same OSR-exit transfer a
conditional back edge's exit takes, at the successor bci, where the
interpreter runs the read of `Callee.value()` with the new bytecode.

* **Emission** (`jit/src/x64/op_invoke.rs`, section "Post-call exit sites";
  hooked in `bytecode_walk.rs::compile_bytecode`'s invoke arm): after an
  invoke that emitted a post-invoke exception check (a real call; counted by
  `emit_post_invoke_exception_check`, `Compiler::post_invoke_checks_emitted`),
  when the successor is not a branch target, its opcode files no deopt point
  of its own (constants, local loads/stores, stack shuffles, `iinc`, and
  `goto` with an empty stack), and `safepoint.rs::branch_mode_exit_target`
  admits and records an `OsrExit` map at the successor with the whole stack
  (the result included) -- the conditional back edge's rules. The site is a
  five-byte `NOP`, padded (a one-to-four-byte `NOP` in front, at most) so it
  lies inside one 8-aligned word. At most 64 per compile.
* **Stubs**, after the walk and before `emit_deopt_stubs`: per site `CALL
  tail; JNZ <successor's reason-7 stub>; JMP <site + 5>` (16 bytes), and once
  per compile a tail that pushes RAX..R11, realigns the stack, saves
  XMM0-15, calls the slow path with `POST_CALL_EXIT_VERDICT_ONLY` in the body
  id, restores everything and returns with the flags of `TEST AL, <the bit
  the body's polls test>` (`POP` and `RET` keep them). No spill and no oop map:
  the helper does not park in that mode, so the thread never stops for a
  collection with the frame's references only in registers or the tail's
  save area; a stop-the-world request meanwhile waits for it (it parks at the
  body's next poll or leaves through the exit) or, once it is back in compiled
  code, freezes it and scans it conservatively without moving
  (`vm/src/jit/xt_root_scan.rs`), as for any thread between two polls.
* **Patch safety: an aligned 8-byte store, no safepoint needed.** The force
  replaces the word containing the site by a compare-exchange that keeps the
  other three bytes; a thread fetching the site -- including one whose call
  returns there at that moment -- decodes the whole `NOP` or the whole `JMP`.
  This is the entry patch's argument (wave 22). A site whose check fails
  (`not_entrant::post_call_exit_jump`) drops the body's post-call sites but
  not its polls, so no redefinition forces less than wave 26 did.
* **Publication**: `CompiledMethod::post_call_exit_sites` (`driver.rs`); a
  body with only post-call sites is registered for the redefinition's scan
  too (`JitCache::note_exit_poll_body`).
* **Tests**: `x64::tests::a_forced_post_call_exit_leaves_at_the_calls_successor`
  (a real single-pass OSR-tier compile of a loop calling through the dispatch
  helper: no slow path while unforced, the site a `JMP` to its stub once
  forced, one verdict-only call per return on "stay", and out at bci 12 --
  the successor -- with `i == 0` on "leave") and
  `the_post_call_exit_shape_check_wants_an_aligned_unwritten_nop`.
* **Cost**: the `NOP` (plus a pad at most one site in two) after each real
  call of an OSR-tier single-pass compile, 16 bytes of cold stub per site and
  one ~170-byte tail per compile; the successor map's metadata. A/B: flip
  `op_invoke.rs::POST_CALL_EXITS_ENABLED` (a `const`; no env switch, so the
  flag inventory does not change) and compare `jit/tests/x64_artifact_corpus.rs`
  sizes and `InvokeDoorCostBench` / `CratonBenchC2` OSR rows; expected no
  timing step.

**Why OSR compiles only.** A method-entry body that gains an `OsrExit` map is
refused by the caller-held direct-`CALL` route for synchronized callees
(`jit_bridge::wrapped_body_has_no_deopt_exit` refuses any body with
`osr_exit_points`), the bimodal `sync-method-1t` cost that function's doc
records. A loop-free method-entry body with a call has none today; giving it
one is a performance change to measure first. Method-entry bodies WITH a
loop already have header maps and could take sites at no such cost: the next
single-pass step, gated on `!self.osr_exit_points.is_empty()` at the site.

**What remains -- stage 3, the optimizing tier.** The probe's loops normally
run an optimizing OSR body (wave 25's host lines named one), so with the
default `CRATONVM_JIT_OSR_OPTIMIZING` the probe may still print `false`. The
verdict tail above is tier-neutral (it preserves every register, and an IR
poll exit's pad takes the machine state as it is: `MOV DEOPT_ARG0, point; JMP`
the shared stub, `ir_lower.rs::emit_poll_exit_pads`), so what the IR tier
needs is only the frame state at the successor:

1. keep the snapshot at the successor bci of every real call through
   `ea_ir_bridge::ir_prune_unconsumable_snapshots` (today it survives only
   when a trapping node or a merge sits at that bci);
2. resolve it at the call's return like `back_edge_mode_exit_state` resolves
   the header's -- every named value a constant, a parameter, or emitted and
   current at the site, the call's result from its home -- and
3. make the home-slot colouring count the site as a consumer of that
   snapshot (`dead_colours_at` / the colour plan read snapshots by bci): a
   named local whose last use precedes the call could otherwise share its home
   with a value defined between that use and the site.

Point 3 is why this lane did not write it blind: a wrong answer there resumes
a frame with another value's bits in a local. Owner: the `ir_lower.rs` call
lowering (`emit_direct_cross_call`, `emit_inline_cache_call`, the generic
dispatch arm) with lane L2.

## Problem, with evidence

A compiled frame whose thread is inside a call while a class the frame's body
spliced is redefined keeps running the body after the call returns. Since wave
25 it leaves for the interpreter at the body's next exit-capable back edge (the
redefinition forces those polls, `JitCache::force_withdrawn_exit_polls`), but
everything between the call's return and that back edge runs the OLD splice.
`tools/probes/interp/L6/RedefineSpliceAfterTheCallProbe.java` reads the spliced
callee right after the call in the same iteration: HotSpot 25 prints
`parked first-after-new=true` / `self first-after-new=true` (JIT and `-Xint`);
CratonVM with the JIT on is expected to print `false` on each row whose loop
was compiled at the call (the parked worker's `CountDownLatch.await()`, and the
redefining thread's own `retransformClasses`).

HotSpot patches the return address of every such frame to the deopt blob
(`frame::deoptimize`), so the call returns into the interpreter at the invoke's
successor. Neither CratonVM tier publishes a frame state at a call's return
point: every deopt point re-executes its bci (`deopt.rs::ResumeSemantics::for_reason`;
`ordinary_stash_frame` refuses a `RESUME` point into the stash), so there is
nowhere to leave to.

## Design

1. **The exit.** A `REEXECUTE` point keyed on the invoke's SUCCESSOR bci, whose
   operand stack is the post-invoke stack (the callee's result pushed), is the
   same interpreter state as `RESUME` at the invoke -- and it is an ordinary
   re-execute point, so the stash, the sinks and `ordinary_stash_frame` take it
   unchanged. The single-pass tier already builds points keyed on a successor
   (`deopt_stubs.rs::emit_post_invoke_exception_check`'s comment records the
   convention and why it moved the exception frame back to the invoke); the IR
   tier has the post-invoke `FrameState` of the call node.
2. **A patchable post-call site.** After each real (not spliced) call of a body
   that spliced another class's bytecode (`inlined_methods` or
   `copied_classes` non-empty), after the post-invoke exception check, emit a
   five-byte `NOP` at an address recorded beside `exit_poll_sites`
   (`post_call_exit_sites`, with the exit stub each one jumps to). Nothing
   else is paid on the fast path: HotSpot emits a post-call `NOP` after every
   call for its own frame lookup (JDK 21+ `post_call_nop`).
3. **Forcing.** `force_withdrawn_exit_polls` also rewrites each such `NOP`
   into `JMP rel32` to its exit stub (one aligned store where the site is
   8-aligned inside an 8-byte window, or a two-step `JMP rel8`-first patch as
   HotSpot's `NativeJump::patch_verified_entry` does), in the same protection
   window. The stub asks `jit_safepoint_slow_path` for the verdict exactly as
   a forced poll does (a withdrawn body that is its class's own obsolete
   activation stays), and leaves through the successor point on "leave".
4. **Where it is admissible.** The same rules as a back-edge mode exit
   (`x64/safepoint.rs::mode_exit_target`, the IR tier's
   `back_edge_mode_exit_state`): no held monitor the frame state cannot
   describe, the method's own scope (not inside a splice), and a stack the
   exit map can describe (the result, plus whatever the invoke left under
   it). A site that fails them gets no `NOP`: that call keeps today's
   behaviour (leave at the next back edge).

## Expected win and how to measure it

Correctness: `RedefineSpliceAfterTheCallProbe` prints HotSpot's lines with
the JIT on, in both modes. Cost: five bytes of code per real call in a
splicing body and no instruction on any path until a redefinition forces the
site. Measure code size on `jit/tests/x64_artifact_corpus.rs` before and after,
and `InvokeDoorCostBench` / `CratonBenchC2` for any timing step (a `NOP` after
a `CALL` should not show; if it does, emit it only in OSR bodies and loop
bodies).

## Cost and risk

Medium-high: both tiers' call lowering (L2's `ir_lower.rs`, the single-pass
`op_invoke.rs`), a new kind of deopt point per call site, and a patch of code
another thread may be about to execute (the same single-store argument as the
entry patch). Stage it: single-pass OSR bodies first (the probe's loops), then
the IR tier.

## Staged plan

1. Single-pass: successor-keyed points and `NOP` sites after real calls in
   bodies with splices; a jit-crate test that forces a site and gets the
   successor frame with the call's result on its stack.
2. Force them in `force_withdrawn_exit_polls`; the probe on the host.
3. The IR tier's call nodes.
