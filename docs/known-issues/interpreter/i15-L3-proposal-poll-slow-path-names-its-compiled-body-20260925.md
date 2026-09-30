# Proposal: the safepoint slow path learns which compiled body polled

**Status: open — filed 2026-09-25 by interpreter round i1 wave 15, lane L3.**

## Where things stand

Wave 15 made single-pass METHOD-ENTRY bodies leave for the interpreter at
their back-edge and self-tail polls
(`interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md`, stage 3).
The slow path (`vm/src/jit/helpers.rs` `jit_safepoint_slow_path` →
`interpreter::compiled_frame_exit_verdict`) is called with one argument, the
stop-the-world flag's address, so it cannot tell which body polled. It
answers two bits (`cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME`,
`SAFEPOINT_VERDICT_EVERY_FRAME`):

* an OSR body tests any bit, because it runs inside the innermost
  interpreter frame's activation and that frame's method is the one the
  innermost bit was asked about;
* a method-entry body tests the every-frame bit alone
  (`jit/src/x64/safepoint.rs::emit_safepoint_poll_leaving_to`), which is set
  only for a JVMTI interpreter-only event or a JDWP request concerning every
  method, and only while no class of the process has been redefined
  (`jvmti_events::every_compiled_frame_may_leave`).

Two consequences follow from the missing identity:

1. **A JDWP breakpoint in a method currently running in a method-entry body
   does not make that body leave.** `DebuggerGates::concerns_method` would say
   yes for the method, but the verdict is asked about the innermost
   interpreter frame, which is some caller. The body finishes compiled and the
   breakpoint is hit only on the next call (which the door runs interpreted).
2. **Any class redefinition in the process withholds every method-entry
   exit for the rest of the run.** The dispatch helpers' sink refuses to
   resume a frame of a redefined class (`helpers::try_resume_trapped_callee`,
   `named_class_was_redefined`) and re-runs the callee from entry, so an exit
   of an obsolete body would replay what it had committed. Without the body's
   class the gate can only be the process-wide
   `classloading::any_class_redefined()`. A HotSwap in a debugging session
   therefore turns stage 3 off for that session.

## Design

Pass the body's identity to the slow path, on the slow path only.

1. `jit-api` `helpers_abi.rs`: `HelperFnSafepointSlowPath` becomes
   `(i64, i64) -> i64`; bump `JIT_HELPERS_ABI_VERSION` and add the
   `ABI_REVISIONS` row. Argument 1 is a per-artifact word, 0 = unknown (every
   existing poll that does not read the verdict may keep passing 0 or keep
   the old `MOV`).
2. x64 (`emit_safepoint_poll_leaving_to`, only when `exit_header` is `Some`):
   `MOV ARG_REGS[1], imm64` of the artifact's `compile_id` (already published
   in the frame record's identity half; see `emit_mov_tls_disp32_rbp`'s
   comment in `op_invoke.rs`) before the `CALL`. The fast path is unchanged;
   the slow path grows by one 10-byte `MOV`.
3. VM: map the compile id to its `CompiledMethod`
   (`cratonvm_jit::lookup_compile_id`, lock-free, the frame-record walk's
   resolver; the artifact is live while one of its frames polls), and answer the
   innermost bit about THAT method: `jvmti_requires_interpreter_for_method`
   with its declaring class id, name and descriptor, and
   `class_was_redefined` for its class instead of the process flag.
4. The method-entry poll then tests `TEST RAX, RAX` like the OSR tier, and
   `SAFEPOINT_VERDICT_EVERY_FRAME` can be retired (keep the bit value
   reserved).

Aarch64 and the IR tier ignore the verdict today and need nothing.

## Expected benefit

* A breakpoint set in a method running in a compiled loop is hit at the
  next slow path instead of on the next call.
* HotSwap no longer disables the exits of unrelated classes.

## Staged plan

1. ABI change and the `MOV` (jit crate + `jit-api`), VM ignores argument 1.
2. VM answers per body; method-entry polls switch to `TEST RAX, RAX`.
3. The redefinition clause moves from the process flag to the body's class.

## How to verify

* jit: extend
  `x64::tests::a_method_entry_body_leaves_at_its_nth_back_edge_poll_on_the_every_frame_verdict`
  with a helper that asserts it received the artifact's compile id and
  answers per id.
* VM: a unit test of the per-body verdict with a JDWP breakpoint set in one
  method (`DebuggerGates::store` through `publish_debugger_gates`), asking
  for that method's body and for another's.
* By hand: `jdb`, a program whose `main` calls a method spinning in a
  counting loop long enough to be compiled at method entry; set a breakpoint
  inside the loop while it runs; it must hit within one loop-exit pause.

## Risk

Low for the fast path (unchanged). The ABI bump touches every
`JitRuntimeHelpers` producer only through the helper's type, not a new
field. The id→artifact lookup runs on a slow path that has just parked, so
its cost is not on any hot path.

## Progress (wave 17)

Interpreter round i1 wave 17, lane L1, 2026-09-26. Stages 1, 2 and 3 landed,
with two departures from the design above, each for a reason found in the
code:

* **No ABI version bump.** `ABI_REVISIONS` records the table's growth only
  (its const assertions refuse a row that does not add a slot), and a slot's
  C signature is not part of it: wave 12 gave this same slot its return value
  without a row. `HelperFnSafepointSlowPath` is `(i64, i64) -> i64`
  (`jit-api/src/helpers_abi.rs`; `assert_helper_call_shape!` at both x64
  call sites says `int_args = 2`, and the arity test says 2).
* **The id is confirmed, not trusted.** The IR tier's polls now pass 0
  (`ir_lower::Lowerer::emit_poll_no_body_id_arg`, two bytes on the slow
  path), but the aarch64 poll loads neither argument, so a non-zero value
  there is whatever the register held. The VM uses the id
  only when it equals this thread's compile-id mirror
  (`vm/src/jit/helpers.rs` `polling_body_id`, then
  `cratonvm_jit::lookup_compile_id`): the innermost compiled frame's
  prologue published exactly that id, and that frame is the one calling, so
  its artifact is live. Anything else is an unnamed body.
* **The bit is kept, not retired.** Rather than switch the method-entry poll
  to `TEST RAX, RAX` and give the innermost-frame bit a second meaning, the
  every-frame bit became `SAFEPOINT_VERDICT_POLLING_BODY` (same value, 2),
  answered about the named body, and the OSR polls of both tiers now test
  `SAFEPOINT_VERDICT_INNERMOST_FRAME` alone (`TEST AL, 1`, three bytes like
  `TEST RAX, RAX`): each tier leaves on exactly the question its sink asks
  (`osr_exit_left_for_the_interpreter` asks it of the innermost frame,
  `exit_left_for_the_interpreter` of the body's method).

What landed:

1. **jit.** `x64/safepoint.rs::emit_safepoint_body_id_arg` —
   `MOV ARG_REGS[1], compile_id` (`XOR` when the compile has none) after the
   flag argument, in `emit_safepoint_poll_leaving_to` (every poll of this
   backend) and in `op_invoke.rs::emit_self_tail_safepoint_poll`'s GC-inert
   form. **Fast path byte-identical**, checked two ways: by reading (the
   instruction sits between the pre-safepoint spill and the `CALL`, inside the
   `JZ`-skipped block; only the `JZ`'s rel32 grows by the instruction's
   length), and by the test
   `x64::safepoint::tests::the_poll_names_its_body_in_arg1_on_the_slow_path_only`,
   which compares a poll with and without an id byte for byte up to the
   argument loads, skipping only the `JZ` displacement.
2. **VM, the verdict.** `jvmti_events::compiled_frame_exit_verdict(shared,
   thread, body: Option<PollingBody>)`; `PollingBody::from_artifact` reads
   the artifact's `owner_class_id` and `method_label`
   (`Class.method:descriptor`, both tiers). `polling_body_must_leave`: the
   doors' question (`jvmti_requires_interpreter_for_method`), withheld when
   the body's class was redefined (by id, and by name as
   `try_resume_trapped_callee` asks), and — for an answer about this method
   only — unless the class name resolves to the body's own class id (a sink
   holding only the stashed frame judges by name,
   `stashed_exit_left_for_the_interpreter`, and would otherwise charge the
   exit). The slow path asks the negative fast path
   `compiled_frames_may_be_asked_to_leave` first, so with no agent it still
   reads a few per-VM flags and looks nothing up.
3. **VM, the pause.** `request_compiled_loop_exits` pauses when a compiled
   frame can be asked to leave at all (a JVMTI interpreter-only event or an
   armed JDWP gate), not only when every frame must; its callers already ask
   only when what the gates concern grew, which includes a changed
   breakpoint set.
4. **The redefinition clause (stage 3)** is per class for a named body; the
   process-wide `any_class_redefined` clause remains for unnamed bodies only.

Tests: jit `x64::safepoint::tests::the_poll_names_its_body_in_arg1_on_the_slow_path_only`;
`x64::tests::a_method_entry_body_leaves_at_its_nth_back_edge_poll_on_the_every_frame_verdict`
(its stub now takes `(flag, body_id)` and the test asserts every slow-path
call passed the artifact's `compile_id`); jit-api's arity test. VM:
`jvmti_events::i17_l1_polling_body_tests` (label parsing; no agent answers
0; under a VM-wide JVMTI mode a named body leaves whatever other class of the
process was redefined; with `--features experimental-debug`, a breakpoint in
`T.hot` pulls back `T.hot`'s body and not `T.cold`'s, not an unnamed body, and
not a same-named class's), `helpers::i17_l1_polling_body_id_tests`, and the
wave-15 tests updated to the new signature. Bench:
`tools/probes/interp/L5/L1W17PollSlowPathBench.java` (allocation-heavy
compiled loops, so the poll slow path is taken at every collection; it
should time the same as the previous build).

**Not done / next.** (a) The by-hand `jdb` check in "How to verify" is the
end-to-end test; there is no in-process JDWP harness for it. (b) A body of a
redefined class still never leaves: the resume sinks would re-run it from
entry. Making them resume a frame of an obsolete body (HotSpot keeps running
the obsolete method in the interpreter) would retire that clause. (c) The IR
tier's method-entry bodies have no mode exits at all; the IR poll could pass
its compile id the same way once it has an uncharged sink. (d) The aarch64
poll should load both arguments. This page stays open as a proposal for the
user to triage.

## Progress (wave 18)

Interpreter round i1 wave 18, lane L3, 2026-09-26. Item (b) of "Not done /
next", narrowed: the slow path now also passes the artifact's
`deopt_points` range and `install_epoch` (`PollingBody::with_artifact_facts`,
`vm/src/jit/helpers.rs` `jit_safepoint_loop_exit_verdict`), and records the
exit it grants on the thread (`jvmti_events::note_mode_exit_grant`). A body
compiled AFTER its class was redefined (`JitCache::compiled_since_last_flush`)
now leaves, and the two sinks that refused a redefined class's stash resume
it (`helpers::try_resume_trapped_callee`, `deopt_resume::real_frame_deopt_resume_and_despeculate`,
through `granted_exit_of_a_current_body`). A body compiled BEFORE the
redefinition still does not: resuming it needs the constant pool the
redefinition dropped
(`docs/internal/fixed-bugs/interpreter-L3-obsolete-methods-keep-no-constant-pool-FIXED-20260925.md`).
The by-name sinks judge a granted exit by the recorded class id
(`stashed_exit_left_for_the_interpreter(.., point_addr)`), so a body whose
class name resolves to another loader's class leaves for a per-method request
too. Tests: `jvmti_events::i18_l3_mode_exit_grant_tests`.

## Progress (wave 18, lane L2) — the optimizing tier names its body

Interpreter round i1 wave 18, lane L2, 2026-09-26. Item (c) of "Not done /
next": the IR poll now passes its compile id (`MOV r64, imm64` on the slow
path; `ir_lower::Lowerer::emit_poll_body_id_arg`, formerly
`emit_poll_no_body_id_arg`) in every method-entry compile whose back-edge
polls may leave (`IrPollModeExits::MethodEntry`), and those polls leave on
`SAFEPOINT_VERDICT_POLLING_BODY`; every other IR poll still passes 0. The IR
prologue and post-call republish already keep the compile-id mirror, so
`helpers::polling_body_id` confirms the id. The slow path gives an IR body no
artifact facts (lane L3's grant is matched inside `deopt_points`, where an
IR poll exit never is), so an IR body keeps the wave-17 verdict:
`docs/internal/fixed-bugs/interpreter-L2-ir-bodies-get-no-mode-exit-grant-FIXED-20260925.md` (fixed in wave 19). Record: the i9
page's "Progress (wave 18) — optimizing method-entry bodies".

## Progress (wave 19, lane L2) — an optimizing body's facts

Interpreter round i1 wave 19, lane L2, 2026-09-25. The slow path now gives a
named optimizing body the same artifact facts as a single-pass one
(`helpers::jit_safepoint_loop_exit_verdict`: the `used_ir_backend` early
return is gone), plus the addresses of its boxed `OsrExit` points, which the
grant keeps (`PollingBody::with_boxed_exit_points`,
`ModeExitGrant::boxed_exits`, `ModeExitGrant::names_point`). An optimizing
body's poll therefore answers exactly as a single-pass body's: about its
own method, by class id, and it leaves when compiled after its class's last
redefinition. Tests: `jvmti_events::i19_l2_ir_grant_tests`. Record:
`docs/internal/fixed-bugs/interpreter-L2-ir-bodies-get-no-mode-exit-grant-FIXED-20260925.md`.

## Wave 22 note — lane L2 (what has landed, what is left)

Interpreter round i1 wave 22, lane L2, 2026-09-26. Of wave 17's "Not done /
next": (c) has landed — the optimizing tier's method-entry polls name their
body (wave 18), get the artifact facts and the grant (wave 19), and since
wave 22 that covers `ACC_SYNCHRONIZED` methods too
(`docs/internal/fixed-bugs/interpreter-L2-ir-entry-exits-refused-for-monitor-methods-FIXED-20260926.md`).
(b) is narrowed to a body compiled BEFORE its class's redefinition (a body
compiled after it leaves since wave 18); its remaining pieces are lane L3's
`docs/internal/fixed-bugs/interpreter-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool-RETIRED-20261003.md`
and the make-not-entrant proposal
`i21-L1-proposal-make-withdrawn-bodies-not-entrant-20260925.md`. (a) is
unchanged: no in-process JDWP harness (`interpreter-L1-proposal-jdi-conformance-harness-FIXED-20261003.md`
proposes one). (d) is unchanged: the aarch64 poll still loads neither
argument, so an aarch64 body is an unnamed one (`jit_safepoint_slow_path`'s
doc). Nothing else of this proposal is open; it can be closed at triage with
(a) and (d) moved to those pages.

## Wave 24 note — lane L6

One more body the slow path could not name, found on the host: an
OPTIMIZING OSR body. Wave 23 made its polls pass their compile id and its OSR
entry stub publish it (so `polling_body_id` confirms it), but the id is
resolved with `cratonvm_jit::lookup_compile_id`, and only `JitCache::put` /
`put_osr` bind an id; the optimizing OSR body is served by the VM's memo and
never published, so the lookup answered `None` and the body was unnamed
(`RedefineRunningSpliceProbe` kept running its withdrawn splice). The slow
path now falls back to the OSR door's continuation record for the innermost
interpreter frame, taken only when that artifact's compile id is the one the
poll named (`helpers.rs::running_osr_body_named`; test
`helpers::i24_l6_unpublished_osr_body_tests`). Binding the id at the memo
instead was not done: it would also change what the GC root walk
(`conservative_roots::published_innermost_method`) and the uncommon-trap
charge (`jit_uncommon_trap_body`) resolve for such a frame.

Of wave 17's "Not done / next": (a) is still the JDWP harness
(`interpreter-L1-proposal-jdi-conformance-harness-FIXED-20261003.md`); (b) is
`docs/internal/fixed-bugs/interpreter-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool-RETIRED-20261003.md`'s
half 2; (c) landed; (d) the aarch64 poll still loads neither argument (the
backend is off unless `CRATONVM_JIT_ARM64` is set). Nothing else is open
here.
