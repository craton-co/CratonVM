# Proposal: a caller-held synchronized CALL that resumes a callee which left

**Status: open (proposal) — filed 2026-09-26 by interpreter round i1 wave 22,
lane L2. Wave 25: a smaller design that needs no caller-held cold side is
now the recommendation ("Wave 25 note").**

## Problem, with evidence

A compiled caller CALLs a `synchronized` callee's body directly while holding
the callee's monitor itself (round 11's caller-held sync-direct site:
`ir_lower::emit_direct_cross_call` with `sync_exit`, and the single-pass
`x64/op_invoke.rs` `sync_direct_site`). The VM binds such a site only to a
body that can stop in no way other than a return or an exception
(`jit_bridge::caller_held_body_is_closed`: no call, no `deopt_points`, no
`_deopt_point_boxes`), because the site's cold side treats every `i64::MIN`
as the callee's exception: it releases the monitor
(`emit_sync_exit_then_throw`) and takes the exception route. A callee that
stashed a frame instead (a guard trap, or a mode exit an agent asked for)
would leave that frame to travel to the caller's sink as a foreign stash and
be re-run from entry after the monitor was released.

Wave 22 (lane L2) gave `ACC_SYNCHRONIZED` optimizing method-entry bodies
back-edge mode exits
(`docs/internal/fixed-bugs/interpreter-L2-ir-entry-exits-refused-for-monitor-methods-FIXED-20260926.md`),
and to keep the binding it withholds the exit from the one body shape the
site binds: a graph that can neither trap nor call
(`ir_lower::ir_entry_poll_mode_exits`). So a debugger breakpoint inside the
loop of, say, `static synchronized long sum(int n)` (a pure arithmetic loop)
is still missed for the rest of a running activation when the optimizing
tier compiled it at entry; everything else leaves.

## Design

Teach the site that a callee can leave, instead of withholding the exit:

1. **The cold side asks before it releases.** After the sentinel compare
   (and the wide-result `dispatch_threw` peek), call the existing callee
   deopt service (`emit_inline_callee_deopt_service` → the VM's
   `jit_service_callee_deopt` → `handle_compiled_callee_deopt_sentinel`) with
   the site's `JitInvokeInfo`. The argument staging is already cold
   (`cold_arg_stage_enabled`).
2. **The service resumes a synchronized callee without taking its monitor.**
   `helpers::try_resume_trapped_callee` refuses a synchronized stash today
   ("stash method is ACC_SYNCHRONIZED") because no monitor can be handed to
   the frame. At a caller-held site none needs to be: the thread holds the
   monitor for the whole call, the rebuilt frame runs with
   `monitor_on_exit == None` (it releases nothing), and the caller's own
   normal edge (`Op::MonitorExit` after the call) or exceptional edge
   (`emit_sync_exit_then_throw`) releases exactly once, as for a callee that
   returned. The service must know the site is caller-held: a flag on the
   site's `JitInvokeInfo` (set when the lowerer plans the sync-direct row),
   never inferred.
3. **The binding admits a body whose only boxes are poll exits** (or any
   resumable point), and only for sites whose cold side has step 1: the
   `SyncDirectTarget` the lookup returns says "may leave", and each tier's
   consumer refuses such a target until its own cold side has the service
   (`compile_request::single_pass_sync_direct_site_admitted` for the
   single-pass tier).
4. Drop the exception in `ir_entry_poll_mode_exits`.

## Expected win and how to measure it

Debug-session precision for the one remaining synchronized shape; no change
on the fast path (the service sits behind the sentinel compare). Measure the
cost side with a timing probe of a hot compiled caller of a trap-free
`static synchronized` leaf loop (rows: `sync-direct` calls per second before
and after, `--compatible`, JIT on); expected: equal. Measure the win with
`tools/probes/interp/L2/L2W22SyncMethodLoopExit.java`'s by-hand `jdb` check
on `pureSum` instead of `divLoop`.

## Cost and risk

Medium. Monitor ownership across a resume is where a lost or doubled unlock
shows up; the resumed frame must be proven to release nothing and the caller
exactly once (a VM test driving `try_resume_trapped_callee` with a
caller-held flag and asserting the monitor's recursion count before and
after). Two tiers' cold sides change (the single-pass one is in
`x64/op_invoke.rs`). The related general gap — a compiled caller with no
callee-deopt service replaying its own activation — is
`interpreter-L7-proposal-service-bare-callee-traps-at-the-dispatch-helper-FIXED-20260930.md`.

## Staged plan

* Stage 1 (VM): the caller-held flag on `JitInvokeInfo` and the
  `try_resume_trapped_callee` arm, with the VM test above.
* Stage 2 (jit, optimizing tier): the service on the IR sync-direct cold path;
  `SyncDirectTarget` "may leave"; the IR consumer admits it; jit test that a
  caller-held CALL to a body that leaves returns the resumed value and the
  monitor count is unchanged.
* Stage 3 (jit, single-pass tier): the same in `sync_direct_site`.
* Stage 4: drop the exception in `ir_entry_poll_mode_exits`.

## Wave 23 note — lane L2 (not landed; the design refined)

Interpreter round i1 wave 23, lane L2, 2026-09-26. No stage landed: every
stage is codegen or ABI work on two crates that could not be built or run in
the lane, and the one thing it buys (a debugger breakpoint inside a pure
arithmetic loop of a `static synchronized` method that the optimizing tier
compiled at entry) does not justify landing it unverified. What reading the
code for it established, for whoever lands it:

1. **The flag cannot be a `JitInvokeInfo` field.** `JitInvokeInfo`
   (`jit/src/lib.rs`) is built by struct literal in about 120 places across
   the two crates. Pass it the way the service passed its only other flag
   (bit 32 of `num_args`, 2026-09-22, since removed — see the doc of
   `helpers::jit_service_callee_deopt_body`), or give the caller-held cold
   side its own helper entry (`jit_service_caller_held_callee_deopt`, a new
   `JitRuntimeHelpers` slot the lowerer reads as it reads
   `service_callee_deopt`). The second is cleaner: nothing else decodes it,
   and a lowerer without the slot keeps today's cold side.
2. **The cold side's order.** On the IR merged route (`emit_direct_cross_call`
   with `sync_exit`): (a) the existing wide-result `dispatch_threw` peek to the
   keep edge; (b) `emit_callee_deopt_service_cold(info_ptr, num_args, inputs,
   true)`, whose answer is in RAX; (c) `CMP RAX, sentinel; JNO .keep` — a
   resumed callee's value takes the NORMAL edge, whose `Op::MonitorExit`
   releases; (d) for a wide result the peek again (a resumed callee may
   return `Long.MIN_VALUE`); (e) otherwise `emit_sync_exit_then_throw`. The
   service calls the interpreter and can collect, so the result slot hiding of
   `emit_sync_exit_then_throw` has to cover the service's safepoint map too
   (it already republishes and reloads when `after_reload` is false; on the
   merged route it is true).
3. **What the VM service does at a caller-held site.** `try_resume_trapped_callee`
   drops its `ACC_SYNCHRONIZED` refusal for the flagged call only; the frame
   it builds has `monitor_on_exit == None`, so the resumed callee releases
   nothing and the caller's edge releases once. Its other exits need no
   change: `rerun_declined_callee_from_entry` re-invokes the callee through
   `bail_to_interpreter`, which enters the method monitor again, re-entrantly
   (the caller holds it), and leaves it on return — the count the caller
   expects; and since wave 23 a refused frame's own block locks are released
   first (`deopt_resume::CompiledLocksOfAStash`). Every re-run still replays
   what the callee committed, which is why stage 3's binding must admit only a
   body whose boxes are all poll exits (a mode exit always resumes: its frame
   is `guard_exit_point_resumable`) and never one with a guard.
4. **Test for stage 1** stays as written above; the jit half can reuse the
   `i19_l2_monitor_block_exit_tests` lowering harness in `ir_lower.rs` to
   assert that the cold side contains the service CALL before the release.

## Wave 24 note — lane L2 (not landed)

Interpreter round i1 wave 24, lane L2, 2026-09-27. Stage 1 was not landed:
with the wave-23 design (a caller-held service helper instead of a
`JitInvokeInfo` flag) stage 1 is a new `JitRuntimeHelpers` slot
(`jit/src/lib.rs`, lane L6) and a new entry in `vm/src/jit/helpers.rs` (lane
L6) that nothing would call until stage 2's cold side exists in
`ir_lower.rs`. Landed alone it is dead code; landed with stage 2 it is a
codegen change on the monitor-release edge that could not be built, run or
lowered in a unit test in this lane. The benefit is still the one shape (a
debugger stop inside a trap-free, call-free loop of a `static synchronized`
method compiled at entry), so it stays proposed.

One fact for whoever lands it, from reading `helpers::try_resume_trapped_callee`
this wave: its `ACC_SYNCHRONIZED` refusal (`"stash method is
ACC_SYNCHRONIZED"`) runs BEFORE the take, while it resolves the stash's
method by key, so a caller-held arm can drop it without moving any of the
take-side bookkeeping (`CompiledLocksOfAStash` pins, `gc_count_before`). And
since wave 24 the frame that arm builds records its block locks in
`held_monitors` (`build_deopt_frame_inner`) while its `monitor_on_exit`
stays `None`, which is exactly the "releases nothing of the method monitor"
the design needs; the caller's `Op::MonitorExit` / `emit_sync_exit_then_throw`
remains the one release of the method monitor.

## Wave 25 note — lane L2 (not landed; a smaller design that needs no cold side)

Interpreter round i1 wave 25, lane L2, 2026-09-27. Stages 1 and 2 were not
landed together: stage 2 is x86-64 emission on the caller-held site's
monitor-release edge (`ir_lower::emit_direct_cross_call`'s `sync_exit` arm,
`emit_sync_exit_then_throw`), plus a new `JitRuntimeHelpers` slot and VM entry
in lane L6's files, and its only honest test is a lowering of a real caller
graph with a planned `sync_direct_row_key` row, a published callee body and
the service wired — none of which could be built or run here. Reading the
code for it found a design that reaches the same win with no cold-side
change at all, and it is the recommendation now.

**The observation.** The shape the exception protects (`ir_entry_poll_mode_exits`:
an `ACC_SYNCHRONIZED` method whose graph can neither trap nor call) can stop
part-way only at a back-edge poll exit, and a poll exit is taken only when
the VM's safepoint verdict says so (`jit_safepoint_loop_exit_verdict` →
`jvmti_events::compiled_frame_exit_verdict`). So instead of teaching the
caller-held site to service a callee that left, make the VM never tell a
body that a caller-held site may be running to leave, and give the exit to
every such body:

1. **A verdict bit only a NAMED body can get** (jit, `lib.rs` constants +
   `ir_lower.rs`). `SAFEPOINT_VERDICT_POLLING_BODY` is also set, for a body
   the poll did not name, when every compiled frame must leave
   (`compiled_frame_exit_verdict`'s `None => every` arm) — a body with
   compile id 0, or whose id disagrees with the TLS mirror
   (`helpers::polling_body_id`). A caller-held callee must never take that
   branch, so its exit tests a new bit, `SAFEPOINT_VERDICT_NAMED_BODY`, set
   only by `polling_body_must_leave` for a body the VM identified.
   `ir_entry_poll_mode_exits` returns a new mode (`MethodEntryNamedOnly`,
   `verdict_bit()` = the new bit) for the shape it refuses today, and `Off`
   when the compile has no id (`compile_id == 0`: nothing could ever name it).
2. **A per-artifact mark** (jit, `lib.rs`, next to
   `withdrawn_by_redefinition`): `bound_by_a_caller_held_site: AtomicBool`,
   `mark_bound_by_a_caller_held_site()` (Release) and its reader (Acquire).
   Per artifact, not a process global.
3. **The binding** (VM, `jit_bridge::sync_direct_target` /
   `caller_held_body_is_closed`): a body whose `_deopt_point_boxes` are ALL
   poll exits (`reason == OsrExit`, not `rethrow_exception`) of the new mode,
   and which is otherwise closed (`wrapped_body_attempt_never_reruns`, no
   `osr_exit_points`, `ir_osr_sentinel_free`), is admitted; the lookup marks
   it before returning the target, i.e. before the caller's code that CALLs it
   can be published. The text pin `caller_held_body_is_closed` … "no box, a
   poll exit included" changes with it.
4. **The verdict** (VM, `compiled_frame_exit_verdict` / `PollingBody`): the
   named bit is set when the named body must leave AND is not marked. A
   marked body keeps a verdict-blind poll for the rest of its life — today's
   behaviour for every body of the shape; an unmarked one (called from the
   interpreter or a dispatch helper) leaves like every other synchronized
   method-entry body since wave 22 (the door hands it the method monitor).
5. Drop the exception in `ir_entry_poll_mode_exits` (it becomes item 1's mode).

Why it is sound: a marked body is never told to leave; the only way a
caller-held CALL could reach an unmarked body is a binding that did not
mark, and the binding is the one place that decides; the mark is set before
the caller's code is published, and the executing thread reached that code
through its publication (acquire), so it sees the mark. What it gives up:
once any caller binds a body, its interpreter-entered activations lose the
exit too (they never had it before).

Tests: jit — the i19 harness (`i19_l2_monitor_block_exit_tests::lower`) with
a `static synchronized` pure loop (no trivial φ left: collapse the header's
trivial φs as `i23_l2_elided_lock_exit_tests` does) lowered in the new mode
has one poll exit testing the named bit, and none with compile id 0; VM — a
`PollingBody` verdict test for a marked and an unmarked artifact (bit set /
clear), and `caller_held_body_is_closed` of an all-poll-exit body (true) and
of a body with one guard box (false). Measure: `L2W22SyncMethodLoopExit`'s
by-hand `jdb` check on `pureSum`; cost `L2W22SyncLeafLoopBench` rows,
expected equal (nothing on the hot path; the slow path reads one more bit).
