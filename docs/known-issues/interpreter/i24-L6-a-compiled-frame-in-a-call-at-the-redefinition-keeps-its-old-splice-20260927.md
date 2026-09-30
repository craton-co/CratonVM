# A compiled loop that is inside a call when its splice's class is redefined keeps running the old splice after the call returns

**Status: narrowed — filed 2026-09-27 by interpreter round i1 wave 24, lane
L6; the wave-24 host run reproduced it (the probe differed in every JIT
mode). Wave 25 (lane L6) forces the exit polls of every withdrawn body, so
both shapes leave at their first exit-capable back edge after the call
returns. Lane L6c also stops a frame that predates a redefinition from
re-entering an optimizing OSR body that folded the new constant. Wave-25
host run (`1055df23b`, fat and no-LTO): `parked after-old=0 after-new=true`
and `self after-old=0 after-new=true` in every mode, as on HotSpot; only
`value-now=12347` (HotSpot 12348) still differed, in `--nojit` too (fixed in wave 26 by lane L3: all rows match now), and was
outside this page. What remains: the code between the call's return and
that back edge, and loops with no exit-capable poll (see "What remains").
Wave 26 (lane L6) narrowed it again: a body no map, root or memo names any
more (superseded, evicted, forgotten by the OSR memo) while a frame still
runs it is now found, withdrawn and forced too; aarch64 is closed as not
applicable; the rest-of-the-iteration gap has its own probe
(`L6/RedefineSpliceAfterTheCallProbe`) and design
(`i26-L6-proposal-a-patchable-post-call-exit-for-withdrawn-bodies-20260928.md`).
Wave 27 (lane L6) closed the rest of the iteration for single-pass OSR
bodies: a patchable post-call exit, forced with the polls, leaves at the
call's successor. The optimizing tier's calls and single-pass method-entry
bodies remain (see "What remains"). Wave 28 (lane L6): the wave-27 probe
turned out not to exercise the single-pass exit at all (a single-pass OSR
compile splices nothing); a real positive control
(`L6/RedefineElidedCtorAfterTheCallProbe`), `post-call exit verdict` lines and
counters now show whether a site fires, and the optimizing tier's OSR-door
compiles got post-call exits too (stage 3 of the proposal). Method-entry
bodies of both tiers, and optimizing frames holding a lock at the call,
remain. Wave 29 (lane L3): single-pass METHOD-ENTRY bodies that already carry
a loop-boundary exit map before the call, and optimizing OSR frames holding a
lock at the call, get post-call exits too; what remains is the optimizing
tier's method-entry bodies, a call before a method-entry body's first loop
header (or in a loop-free one), and calls made inside a splice (see "What
remains"). Wave 37 (lane L3): an IDE-HotSwap-shaped redefinition reaches
these shapes exactly as a retransform does; nothing changed (see its
note). Wave 38 (lane L3): the optimizing tier's method-entry bodies get
post-call exits too (written, not yet measured: see "Progress (wave 38)").
Wave 39 (lane L3): a single-pass method-entry call before the body's first
loop header, or in a loop-free body, gets its site too (see "Progress (wave
39)"); calls inside a splice remain. Wave 40 (lane L3): the splice case is
handed to its proposal; the page stays open for the five smaller shapes
"Progress (wave 40)" lists, none of which the proposal covers. Wave 46
(lane L2): item 2 narrowed -- a call whose successor is a `getstatic` or
`getfield` gets its post-call site (see "Progress (wave 46)").**

## Progress (wave 46) — lane L2: a call followed by a field read gets its site

Item 2 of "Progress (wave 40)" below ("a single-pass call whose successor
files a deopt point of its own") is narrowed for field READS. Wave 45's next
step assumed a `getfield` / `getstatic` files a `REEXECUTE` point at its own
bci; read at `55834015b`, neither does. Every arm of both (`x64/op_field.rs`:
the inline `getstatic`, the scalar-replaced and compact inline `getfield`,
the guarded inline `getfield`'s bail to the checked helper, and the helper
calls) files at most the `RETHROW` `PendingException` frame of
`emit_post_invoke_exception_check`, in its own by-bci map
(`exc_frame_box_ptr_by_bci`), and the loop pre-header's field hoists file
their reason-2 guards at the pre-header's position (the header, or a rotated
loop's entry `goto`), never at the in-body read. That is the shape wave 44
admitted for the constant-pool helper sites: neither by-bci reason lookup
confuses a `RETHROW` point with the site's `REEXECUTE` `OsrExit` map
(`osr_exit::deopt_reason_at_bci` skips rethrow points,
`exceptional_reason_at_bci` keeps only them), so no point-identity change to
`deopt_reason_at_bci` is needed for them. `post_call_exit_successor_admitted`
now admits `getstatic` and `getfield` behind
`x64/op_invoke.rs::POST_CALL_EXIT_FIELD_READ_SUCCESSORS` (on in the lane's
last commit): the frame leaves at the read, before it runs. Unit test
`x64::tests::a_call_followed_by_a_getstatic_gets_a_post_call_exit` (the
site, its map at the read, a stay and a leave verdict, and the one
non-rethrow point at the read's bci); probe
`tools/probes/interp/L2/L2W46FieldReadAfterTheCall.java` (HotSpot
`getstatic ctor-ran-after-the-call=true`, `getfield ctor-ran-after-the-call=true`;
positive control and the base's expected `false` in its header).

Array loads stay refused: their precise exception frames are `RETHROW` too,
but their speculative-BCE and hoist guards are the point-identity question
wave 45 named, and they were not traced arm by arm. Field and static stores,
another invoke, `ldc` (its own wave-44 site leaves) and conditional branches
are unchanged. Items 1 and 3-5 are unchanged.

Found on the way and filed:
`i46-L2-a-post-call-exit-before-a-rotated-loops-entry-goto-shares-its-bci-with-the-pre-header-guards-20261010.md`
(the admitted `goto` successor can be a rotated loop's entry, whose
pre-header files reason-2 guard points at the same bci after the site's map).

## Progress (wave 45) — lane L2: nothing of the five shapes moved

Wave 45 built the i44-L2 proposal
(`i44-L2-proposal-an-own-class-compiled-activation-leaves-at-its-constant-pool-sites-20261008.md`,
"Progress (wave 45)"): an OSR body of the REDEFINED class's own old bytecode
is now forced and leaves after a renumbering redefinition. This page is about
a body withdrawn because it spliced (or elided a constructor of) the
redefined class, which was forced already; its five shapes in "Progress
(wave 40)" are unchanged. The next step for item 2 (a successor that files a
point of its own), the only one with a single-pass fix in reach: admit a
post-call site whose successor is a `getfield` / `getstatic` / array load
when that instruction's own point at the same bci is a `REEXECUTE` point
with the same frame state (the successor has not run), which needs
`osr_exit::deopt_reason_at_bci` to tell the two reasons apart by the point
the stash names rather than by bci (wave 44 did this for the allocation
guard's `RETHROW` point by keeping it in its own by-bci map).

## Progress (wave 44) — lane L2: a successor that is a constant-pool helper leaves itself

Item 2 of "Progress (wave 40)" below ("a single-pass call whose successor
files a deopt point of its own") is narrowed for one family of successors:
an `ldc` of a String or Class, a deferred `new` / `anewarray` and
`multianewarray` now carry a forceable exit of their own BEFORE their helper
call, with a map at the instruction itself
(`jit/src/x64/op_invoke.rs::emit_cp_helper_exit_site`, behind
`CP_HELPER_EXITS_ENABLED`;
`docs/known-issues/interpreter/i43-L2-proposal-exits-at-a-withdrawn-bodys-constant-pool-helper-calls-20261007.md`,
"Progress (wave 44)"). The post-call site stays refused there, and the
frame leaves at the successor itself, before it runs: nothing after the
call ran compiled. Probe
`tools/probes/interp/L2/L2W44CpHelperExitAfterTheCall.java` (rows `string`
and `class`: HotSpot `ctor-ran-after-the-call=true`; the base prints `false`
under `CRATONVM_C2_SUPERSEDE=0`). A successor that is a field or array
access, another invoke or a conditional branch is unchanged; items 1 and 3-5
are unchanged.

## Progress (wave 40) — lane L3: what remains beyond the splice proposal

Nothing changed in code. The wave-40 brief asked whether only
`i39-L3-proposal-post-call-exits-inside-a-splice-20261003.md` remains; it
does not. That proposal owns a real call made INSIDE a splice (the two-frame
exit). Outside it, "What remains" still names five shapes in which the rest
of one iteration after a call runs the old splice once (HotSpot's frame
returns into the interpreter at the call's return point), each refused by a
named test:

1. a single-pass METHOD-ENTRY call before the body's first loop header (or in
   a loop-free body) of a compile that keeps no dispatch record
   (`Compiler::keeps_a_call_record` false: a call through an intrinsic helper
   only) -- `emit_post_call_exit_site`'s `tier_admitted`;
2. a single-pass call whose successor files a deopt point of its own or is a
   branch target -- `post_call_exit_successor_admitted`, `branch_targets`;
3. an optimizing call whose frame-state snapshot names a value with no
   location -- `ir_lower.rs::post_call_exit_state`;
4. a call past a compile's 64th site -- `POST_CALL_EXIT_SITES_MAX`;
5. loops with no exit-capable poll (an elided monitor, a live operand stack
   at a `goto` the arm could not canonicalise, a loop rewrite) --
   `x64/safepoint.rs::mode_exit_target`.

Items 1-4 are each one iteration's code after one call; item 5 is the whole
rest of such a loop. The wave-38 and wave-39 sites (optimizing method-entry
bodies; single-pass calls before the first map): the wave-39 host run
reports every wave-39 probe matching HotSpot, `L3W39PostCallExitBeforeTheLoop`
included; whether its sites engaged (that probe's positive control) and
their cost rows are not recorded on this page. None of 1-5 has a probe that
reaches it on the host yet.

## Progress (wave 39) — lane L3: single-pass calls before the first map (code written, not measured)

**What changed.** `jit/src/x64/op_invoke.rs::emit_post_call_exit_site` admits
a METHOD-ENTRY single-pass call whose site files the body's FIRST
loop-boundary exit map (no loop header before the call in bytecode order, or
no loop at all) when the compile keeps a dispatch record
(`Compiler::keeps_a_call_record`: `invoke_info`, `mic_slots` or `pic_slots`
non-empty, which the artifact owns as `_jit_invoke_infos` /
`_jit_mic_slots` / `_jit_pic_slots`), under the kill switch
`METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`. Everything else about the
site (successor rules, `branch_mode_exit_target`, the stub, the force pass,
the verdict bit) is wave 29's.

**Why the wave-29 concern does not apply (traced).** Wave 29 kept a body's
first deopt exit away because of two readers:

* `jit_bridge::caller_held_body_is_closed` (the caller-held direct `CALL` of
  a synchronized callee) and `door_admits_wrapped_body` (the synchronized
  door) both require `wrapped_body_attempt_never_reruns`, which refuses any
  body with a non-empty `_jit_invoke_infos`, `_direct_callee_entries`,
  `_jit_cycle_cells`, `_jit_mic_slots` or `_jit_pic_slots`. Each
  `invoke_info` row the single-pass compiler reads points into the
  `owned_invoke_infos` arena that becomes `_jit_invoke_infos`
  (`jit/src/lib.rs`, the `SinglePassParts` table), so a body the new rule
  admits was refused on both routes already, whatever its deopt exits (the
  wave-38 argument for the optimizing tier, made here for the single-pass
  one; `keeps_a_call_record` is the explicit test, since a call through an
  intrinsic helper alone keeps no record).
* `CompiledMethod::can_osr_exit`, which the first `osr_exit_points` entry
  sets (`x64/driver.rs`): its only VM reader is the OSR door's transfer
  (`jit_bridge.rs`, `compiled.can_osr_exit && transfer_osr_exit_into_live_frame`),
  which reads the OSR body it entered, never a method-entry one. The other
  `osr_exit_points` reader in the VM is `wrapped_body_has_no_deopt_exit`
  (the first bullet).
* `can_deopt_resume` becomes true for such a body, as it does for every
  method-entry body with a loop since wave 15's header maps; the replay check
  (`single_pass_first_trap_needing_unsound_replay`) sees a resumable
  `OsrExit` point (`branch_exit_frame_is_transferable` admitted it).

**Probe and positive control.** `tools/probes/interp/L3/L3W39PostCallExitBeforeTheLoop.java`
(agent jar): rows `free` (a loop-free body) and `before` (the call precedes
the loop header), each parked in a call while the constructor it elided is
retransformed. HotSpot 25, JIT and `-Xint`: `free ctor-ran-after-the-call=true`,
`before ctor-ran-after-the-call=true`. Under `CRATONVM_C2_SUPERSEDE=0
CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1` each body's `exit polls candidate:
... post-call-sites=N` must show N >= 1 and a `post-call exit verdict #k: ...
named=true withdrawn=true verdict=<non-zero>` line must name it; on the
wave-38 binary both rows print `false` there with `post-call-sites=0`. Unit
test: `x64::tests::a_method_entry_body_with_a_loop_gets_a_post_call_exit`
(its loop-free body now gets one site and leaves at the call's successor on
a polling-body verdict).

**Cost for the orchestrator to measure.** Every single-pass method-entry body
with a call and no loop before it now carries, per admitted call, a five-byte
`NOP` after the return (plus at most a four-byte alignment `NOP`), a 16-byte
stub and one `DeoptimizationPoint` out of line, and the shared ~150-byte
verdict tail once. Fat LTO, JIT on, `CRATONVM_C2_SUPERSEDE=0`, this binary
against itself rebuilt with `METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED =
false`: `InvokeDoorCostBench` (`static-call`, `virtual-call`, `super-call`)
and `L7W28VirtualDoorSplitBench` (`staticCall`, `syncMono`). Expect noise.

**Still open:** a call made INSIDE a splice, a successor that files a point
of its own or is a branch target, a call past the 64th site, and loops with
no exit-capable poll (see "What remains").

## Progress (wave 38) — lane L3: optimizing method-entry bodies (code written, not measured)

**What changed.** `jit/src/ir_lower.rs::maybe_emit_post_call_exit_site`
admits a METHOD-ENTRY compile (`IrPollModeExits::MethodEntry`) as well as an
OSR-door one, under the kill switch `IR_ENTRY_POST_CALL_EXITS_ENABLED`;
`post_call_exit_state` gates on the sink the exit reaches (the stash sinks'
resume for method entry, as `back_edge_mode_exit_state` does) and takes the
method-entry exits' lock rules (`IR_ENTRY_EXITS_HOLDING_A_LOCK_ENABLED`,
`IR_ENTRY_EXITS_WITH_AN_ELIDED_LOCK_ENABLED`). The stub, the verdict tail
(`TEST AL, SAFEPOINT_VERDICT_POLLING_BODY`, the bit the body's own back-edge
polls test), the force pass and the stash sinks are the ones the method-entry
back-edge exits use since wave 18. Unit test:
`ir_lower::i28_l6_post_call_exit_tests::a_method_entry_compile_puts_a_forceable_exit_after_a_call`.
Commit `b2957bb0d`.

**Why the wave-27/29 concern does not apply.** The cost those waves held back
for was the caller-held direct `CALL` of a `synchronized` callee
(`jit_bridge::caller_held_body_is_closed` refuses a body with any deopt box).
But that route's predicates -- `caller_held_body_is_closed` and the door's
`door_admits_wrapped_body` -- both require `wrapped_body_attempt_never_reruns`,
which refuses a body that calls anything (`_jit_invoke_infos`,
`_direct_callee_entries`, cycle cells, inline caches), and a post-call site
exists only after an `Op::Call` whose info the artifact keeps
(`ir_call_infos` -> `_jit_invoke_infos`, `jit/src/lib.rs`). So every body that
gains a site was refused there already. The same argument would lift the
single-pass tier's "a map already filed" condition
(`op_invoke.rs::emit_post_call_exit_site`), except that a single-pass body's
first `osr_exit_points` entry also sets `can_osr_exit` (`x64/driver.rs`), a
fact the lane did not trace through its readers; left as it is.

**The A/B for the orchestrator (not run: no build here).**

1. Positive control: `tools/probes/interp/L6/L6W29PostCallExitShapesProbe.java`
   row `entry` with the DEFAULT settings (no `CRATONVM_C2_SUPERSEDE=0`), JIT
   on, `CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1`: expected
   `entry ctor-ran-after-the-call=true` (HotSpot's line), an `exit polls
   candidate: L6W29PostCallExitShapesProbe.entryBody... ir=true ...
   post-call-sites=N` line with N >= 1, and a `post-call exit verdict #k: ...
   entryBody... named=true withdrawn=true verdict=0x..` line (non-zero). On the
   wave-37 binary the same run prints `post-call-sites=0` for the IR body
   (and `false` when the optimizing body superseded the single-pass one).
2. Cost: fat LTO, JIT on, interleaved on two cores, the wave-37 binary against
   this one, and this one rebuilt with `IR_ENTRY_POST_CALL_EXITS_ENABLED =
   false` (byte-identical method-entry compiles to wave 37) as the in-binary
   control: `InvokeDoorCostBench` (`static-call`, `virtual-call`,
   `super-call`), `L7W28VirtualDoorSplitBench` (`staticCall`, `syncMono`) and
   `R12MonitorContended` `sync-method-1t` -- the row wave 27 feared for. What
   is added per compiled call: one five-byte `NOP` (plus at most a four-byte
   alignment `NOP`) after the call's return, a 16-byte stub and the shared
   ~150-byte verdict tail out of line, and one `DeoptimizationPoint` box; no
   register or slot change (the invoke's snapshot is already kept: a call can
   deopt). Expect noise-level differences; a `sync-method-1t` move would
   contradict the argument above.
3. The suite and Mockito as usual: a method-entry exit now fires only after a
   redefinition withdrew the body (the sites are `NOP`s until forced).

## Wave 37 note — lane L3: HotSwap reaches these shapes as a retransform does

Scoped as the round asked, against an IDE-HotSwap-shaped redefinition
(`javac` output of the edited callee class, installed with
`Instrumentation.redefineClasses`): what goes stale here is the SPLICE -- the
caller's compiled copy of the callee's old bytecode -- not a constant-pool
index, so a renumbering redefinition reaches exactly the shapes a same-pool
retransform reaches, and the existing probes
(`L6/RedefineSpliceAcrossACallProbe`, `L6/RedefineSpliceAfterTheCallProbe`,
`L6/RedefineElidedCtorAfterTheCallProbe`, `L6/L6W29PostCallExitShapesProbe`)
measure them; a renamed-donor copy of them would add no row. Wave 37's
redefinition fence
(`docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`)
holds INTERPRETER loops only; a compiled caller running an old splice is
unaffected by it, and leaves as before at its forced exits.

What remains is unchanged (see "What remains"); not attempted this wave: the
optimizing tier's method-entry post-call exit needs the IR lowering to know,
at the call, whether the body will carry a deopt exit anyway (the
`wrapped_body_has_no_deopt_exit` / `sync-method-1t` cost the wave-29 section
records), which cannot be settled without a build and an A/B.

## Progress (wave 29) — lane L3

**Single-pass method-entry bodies (the proposal's "next single-pass step").**
`jit/src/x64/op_invoke.rs::emit_post_call_exit_site` now admits a
method-entry compile (`mode_exit_polls == false`) when the walk has already
filed a loop-boundary exit map (`Compiler::osr_exit_points` non-empty at the
call: a loop header before it, a self-tail or conditional back edge's map, an
`invokedynamic` trap map), under the kill switch
`METHOD_ENTRY_POST_CALL_EXITS_ENABLED`. Such a body already has a deopt exit,
so `jit_bridge::wrapped_body_has_no_deopt_exit` (the synchronized direct-`CALL`
route the proposal's "Why OSR compiles only" names) answers exactly as before;
a loop-free body, or a call before the first header in bytecode order, gets no
site. Nothing else changes: the stub's verdict tail already tests
`SAFEPOINT_VERDICT_POLLING_BODY` for a method-entry body (the bit its own
back-edge polls test), `branch_mode_exit_target` already admitted
method-entry maps (`METHOD_ENTRY_MODE_EXITS_ENABLED`, wave 15), the deopt
register region is reserved for every compile under `deopt_real`, and the
force pass (`force_withdrawn_exit_polls`) and the exit (`jit_safepoint_slow_path`
→ `note_post_call_exit_verdict` → reason-7 stub → the method-entry stash
sinks, which take a granted mode exit uncharged) are the same as for the
body's back-edge exits. Traced: javac puts a `for` / `while` loop's test at
its top, so the header precedes every call in the loop body and the walk has
filed the header's map (`bytecode_walk.rs`, `osr_exit_map_headers`, built from
`escape_analysis::detect_loops` for every compile) when it reaches the call.
Unit test: `x64::tests::a_method_entry_body_with_a_loop_gets_a_post_call_exit`
(a real method-entry compile and run: one site; an innermost-frame verdict
stays compiled, a polling-body verdict leaves at the call's successor with the
iteration's `iinc` not run; the loop-free method gets no site).

**Optimizing OSR frames holding a lock at the call.**
`jit/src/ir_lower.rs::post_call_exit_state` refused any snapshot with a
monitor. It now admits one under the switches the OSR door's back-edge poll
exits use (`osr_exit::OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED`,
`OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED`) and with the same checks as
`back_edge_mode_exit_state`: each locked object current right after the call,
described by `monitor_object_value` as a reference frame word
(`StackSlotRef`), `lock_depth > 0`, an elided level only where the transfer
re-takes it. The point then goes the way a lock-holding back-edge exit goes:
`guard_exit_resume_bci` → `poll_exit_point_resumable` (which accepts such
monitors) → `deopt_resume::transfer_osr_guard_exit_into_live_frame`, which
leaves each hold with the live frame and re-takes an elided level. An invoke
takes and releases no lock, so the invoke's own snapshot's monitors are the
successor's. The caller-held synchronized direct call (the lock no frame
state describes) is still refused (`sync_direct_row_key`). Unit test:
`ir_lower::i28_l6_post_call_exit_tests::a_call_inside_a_synchronized_block_gets_a_site_naming_the_lock`
(javac's shape of a loop calling `g()` inside `synchronized (o)`: one site,
its point names the one lock the compiled code took, and the planless
transfer's predicate resumes it at the successor).

**Probe and positive control.**
`tools/probes/interp/L6/L6W29PostCallExitShapesProbe.java` (agent jar):
`entry` (a method-entry body with a loop, parked in a call inside it while
the constructor it elided is retransformed) and `locked` (the parent probe's
OSR loop with the call and the `new` inside `synchronized`). HotSpot 25, JIT
and `-Xint`: `entry ctor-ran-after-the-call=true`,
`locked ctor-ran-after-the-call=true`. The header names the dbg lines that
show each site firing (`exit polls candidate ... post-call-sites=N`,
`post-call exit verdict ... named=true withdrawn=true verdict=<non-zero>`)
and the settings: `entry` needs `CRATONVM_C2_SUPERSEDE=0` to keep the
single-pass body, because the optimizing tier's method-entry compiles get no
site yet.

**Not done, and why.** The optimizing tier's METHOD-ENTRY compiles
(`maybe_emit_post_call_exit_site` keeps its `OsrDoor` test): the IR has no
equivalent of the single-pass walk's "a map is already filed" fact at the call
(its back-edge poll exits are lowered at the back edge, after the loop body's
calls in block order), and an IR body's first `_deopt_point_boxes` entry
flips `wrapped_body_has_no_deopt_exit` for a synchronized callee -- the
bimodal `sync-method-1t` cost that function's doc records. It needs either a
pre-pass that decides the body's poll exits before its calls are lowered, or
an A/B that shows the new box costs no route; neither was attempted without a
build.

## Progress (wave 28) — lane L6

**Why the wave-27 host run showed nothing.** Traced on the new base: a
single-pass OSR compile splices NO callee -- `jit_bridge.rs::compile_osr_body`
passes `compile_with_param_slots` an empty `inline_sites` map -- so under
`CRATONVM_JIT_OSR_OPTIMIZING=0` `RedefineSpliceAfterTheCallProbe`'s
`Callee.value()` is a real call that reaches the new bytecode by itself. The
loop's body copies nothing of `Callee`, the (scoped) retransform does not
withdraw it, nothing is forced, and both rows print `true` with or without any
exit: exactly the host log (no `exit polls candidate` line; the wave-26 binary
printing `true` too). The `tier=C2 optimized=true osr_bci=10` line the run
quoted is the background TASK's line (`jit_bridge.rs::background_compile_task`),
printed before `background_osr_optimizing_body` declines under
`CRATONVM_JIT_OSR_OPTIMIZING=0` and the single-pass `compile_osr_artifact`
builds the body. The one redefinition dependency a single-pass OSR body does
carry is an ELIDED constructor: `compile_osr_body` asks
`jit_bridge::is_elidable_construction` for every `new C; invokespecial
C.<init>()V` site, drops the call of a proven-empty constructor, and marks `C`
copied (`JitCache::note_bytecode_copied`), so a redefinition of `C` flushes
the whole cache and withdraws the body. Nothing was broken in the wave-27 code
path itself; the probe could not reach it.

**Positive control.** `tools/probes/interp/L6/RedefineElidedCtorAfterTheCallProbe.java`
retransforms an empty constructor into one that counts (`made += 1`; the
transformer rewrites the `Code` attribute of `<init>()V` on a RETRANSFORM
only) while a compiled loop that elided it is inside a call (a parked
worker's `await()`, the redefining thread's own `retransformClasses`), and
constructs one right after the call; the successor of each call is a local
load, which a single-pass site admits. HotSpot 25 (JIT and `-Xint`):
`parked ctor-ran-after-the-call=true`, `self ctor-ran-after-the-call=true`.
Path, single-pass (`CRATONVM_JIT_OSR_OPTIMIZING=0`): background OSR task →
`compile_osr_artifact` → `compile_osr_body` (elides `MarkerP.<init>`, marks it
copied) → `bytecode_walk.rs::compile_bytecode`'s invoke arm →
`op_invoke.rs::emit_post_call_exit_site` (successor `iload`, admitted) →
published by `put_osr` (registered for the scan); the retransform →
`redefine_class_with` → `JitRealm::redefine_and_flush` (whole cache:
`scoped_redefinition_admitted` refuses a copied class) →
`redefinition_stale_bodies(whole_cache)` → `make_not_entrant` →
`force_withdrawn_exit_polls` (the candidate line names the body with
`post-call-sites=N`) → the call returns into the site's `JMP` → stub → tail →
`jit_safepoint_slow_path(.., id | POST_CALL_EXIT_VERDICT_ONLY)` → the new
`note_post_call_exit_verdict` → `JNZ` the successor's reason-7 stub → the
OSR-exit transfer → the interpreter runs `new MarkerP()` with the new
constructor.

**Observability.** `vm/src/jit/helpers.rs::note_post_call_exit_verdict`
counts every verdict a forced post-call site asks for, per VM
(`JitCache::note_post_call_exit_verdict`: verdicts asked, exits taken), and
under `CRATONVM_DBG_DEOPT=1` prints `[cratonvm-deopt] post-call exit verdict
#n: <label> id=.. named=<bool> withdrawn=<bool> verdict=0x..` for every
"leave" and the first 64 "stay"s (`named=false`: this thread's compile-id
mirror did not confirm the id, so the verdict could not be about the body --
the first thing to look at when a forced site never leaves). The
`exit polls forced:` line (`CRATONVM_DBG_JITC=1`) now ends with
`post-call-verdicts-so-far=.. post-call-exits-so-far=..`, so the next
redefinition's line says what the previous one's sites did.

**The optimizing tier (stage 3).** `jit/src/ir_lower.rs::maybe_emit_post_call_exit_site`,
called by `lower_data_node_tracked` after each `Op::Call` node of an OSR-door
compile (after every route of the call's arm and a prefix's hit edge joined),
emits the same aligned five-byte `NOP`; `emit_post_call_exit_stubs` (after the
poll exits' pads, before `emit_deopt_stub`) lays down `CALL tail; JZ back;
MOV DEOPT_ARG0, point; JMP <shared deopt stub>` and the same register-preserving
verdict tail; `lower_inner` publishes `CompiledMethod::post_call_exit_sites`,
which the existing force pass and shape check take unchanged. The frame state
(`post_call_exit_state`) is the INVOKE's own snapshot with the arguments
popped and the result pushed, resumed (`REEXECUTE`) at the successor: an
invoke touches no local, and every value that snapshot names is pinned by
`plan_slots` (a call can deopt, so the snapshot survives
`ir_prune_unconsumable_snapshots`, and every value a kept snapshot names gets a
colour of its own) -- which disposes of the proposal's colouring hazard for
those values without touching the colouring. Each value must be current right
after the call (constant, parameter, or emitted in / a φ of a block
dominating the call's), and the frame must pass `guard_exit_point_resumable`;
otherwise the site is not emitted. The point is an `OsrExit` box kept out of
the guard facts like the poll exits' (so no OSR admission changes), which
`guard_exit_resume_bci` accepts without admission. Refused (fail closed, the
call keeps wave 27's behaviour): a frame holding a monitor, an unrolled copy
with no snapshot of its own, a value with no location, a call inside a splice.
Unit test: `ir_lower::i28_l6_post_call_exit_tests::an_osr_door_compile_puts_a_forceable_exit_after_a_call`.
So `RedefineElidedCtorAfterTheCallProbe` is expected to print HotSpot's lines
with the DEFAULT settings too (its calls are wrapped in methods with an
exception table, which the optimizing tier's inline resolver refuses to
splice, so they stay real calls). `RedefineSpliceAfterTheCallProbe` may not:
`CountDownLatch.await()` and `InstrumentationImpl.retransformClasses` have no
exception table, the optimizing tier may splice them, and the blocking call
is then made INSIDE the splice, where no site is emitted (see "What remains").

## Progress (wave 27) — lane L6

**The rest of the iteration, single-pass OSR bodies.** Stages 1-2 of
`i26-L6-proposal-a-patchable-post-call-exit-for-withdrawn-bodies-20260928.md`
(its "Progress (wave 27)" has the traced path, the patch-safety argument and
the costs): an OSR-tier single-pass compile emits a five-byte `NOP` after
each real call whose successor state a conditional back edge's rules admit,
with an `OsrExit` map at the SUCCESSOR bci (a re-execute point there with the
call's result on the stack is "resume after the invoke"); the redefinition's
force pass rewrites it into a `JMP` to a stub that asks the slow path for the
verdict without parking (`POST_CALL_EXIT_VERDICT_ONLY`) with every register
preserved, and leaves through that map. So a frame whose thread was inside a
call -- shape 1's parked worker, shape 2's redefining thread -- leaves as soon
as the call returns, before the rest of its iteration, as HotSpot's patched
return address does. Verified by
`x64::tests::a_forced_post_call_exit_leaves_at_the_calls_successor` (a real
compile and run); on the host: `RedefineSpliceAfterTheCallProbe` under
`CRATONVM_JIT_OSR_OPTIMIZING=0` should print HotSpot's two lines.

**Also fixed:** a body spared as its own class's obsolete activation was
forced by the next whole-cache redefinition of any other class, and its loop
then paid a slow-path call per back edge for a verdict that is always "stay"
(`docs/internal/fixed-bugs/interpreter-L6-an-obsolete-body-is-forced-by-every-later-redefinition-FIXED-20260928.md`).

## Progress (wave 26) — lane L6

**Bodies nothing names any more.** The redefinition's candidate lists were
the cache's maps, the baked callee roots reachable from them and the
optimizing OSR memo. A body a frame is still running can be in none of them:

* a method-entry or single-pass OSR body **superseded** by a tier-up (or
  evicted) while a frame kept running its loop. A scoped redefinition did not
  even withdraw it (so no pause could make it leave); a whole-cache one
  withdrew it through its barrier but forced nothing;
* an optimizing OSR body the **memo forgot** (a code-state epoch move from an
  unrelated redefinition or flush, a rebuild for the same key) -- the case
  wave 25 listed.

`JitCache` now keeps a weak list of every body with exit-poll sites
(`exit_poll_bodies`, filled by `put` / `put_osr` and, for the optimizing OSR
bodies the VM builds and never publishes, by
`jit_bridge::build_osr_optimizing_artifact` through
`JitCache::note_exit_poll_body`; pruned at each power-of-two length).
`JitCache::redefinition_stale_bodies` adds the live published ones to its walk
(so they are made not entrant, marked withdrawn and forced like any stale
body), and `JitRealm::withdrawn_osr_memo_candidates` adds the live unpublished
ones (`JitCache::live_unpublished_exit_poll_bodies`) to the memo's list (marked
and forced, never patched: they have no identity to re-dispatch by). Path: the
redefinition (`vm_exec.rs::redefine_class_with` →
`JitRealm::note_class_redefinition_of` → `redefine_and_flush` →
`not_entrant_candidates` / `withdrawn_osr_memo_candidates` → `make_not_entrant`
/ `mark_withdrawn_by_redefinition` → `force_withdrawn_exit_polls`), then the
frame's next back edge → `jit_safepoint_slow_path` →
`jvmti_events::polling_body_must_leave` (withdrawn) → the poll's mode exit.
Test: `not_entrant::tests::a_superseded_body_a_frame_still_runs_is_found_and_its_polls_forced`
(a real `put`, a superseding `put`, the real scan and force pass) and
`an_unpublished_exit_poll_body_is_listed_while_it_lives`.

**aarch64: not applicable.** The aarch64 backend splices nothing (every
`invoke*` goes through `jit_invoke_dispatch`, and it emits `BL` only to its own
labels;
`docs/internal/fixed-bugs/interpreter-L6-a-running-splice-of-a-redefined-callee-runs-its-old-bytecode-FIXED-20260926.md`,
item 3), so no aarch64 frame runs a splice of a redefined class, and a call it
makes reaches the new bytecode through the VM door. Its safepoint poll also has
no mode exit (`aarch64_backend.rs::emit_safepoint_poll` ignores the verdict), so
no site would be there to force; if the backend ever splices, it needs both.

**The rest of the iteration, pinned by a probe.**
`tools/probes/interp/L6/RedefineSpliceAfterTheCallProbe.java` reads the spliced
callee right after the call, in the same iteration, with no branch in between
that could have been compiled as an unreached-code trap. HotSpot 25 (JIT and
`-Xint`): `parked first-after-new=true`, `self first-after-new=true`. Expected
on CratonVM with the JIT on until the post-call exit exists: `false` on each row
whose loop ran compiled at the call. No fix this wave: it needs an exit at the
call's return point, i.e. a frame state after the invoke with the result pushed.
The cheapest shape found is a patchable five-byte `NOP` after each real call of
a body that spliced a class, rewritten into a `JMP` to an exit keyed on the
invoke's SUCCESSOR bci (a `REEXECUTE` point there with the result on the stack
is exactly `RESUME` after the invoke, which the resume sinks already handle) --
see the proposal.

**Fewer pauses.** Stage 2 of
`i25-L6-proposal-retire-the-loop-exit-retries-for-forced-bodies` landed with it:
the loop-exit handshake takes no retry pause when every body withdrawn since
the redefinition (or the `retransformClasses` batch) began had its exit polls
forced (`JitCache::every_withdrawal_forced_since`).

## Progress (wave 25) — lane L6

**Forced exit polls instead of a return barrier.** A withdrawn body is never
entered again (its entry is not entrant, the OSR door refuses it), so the
only frames that still reach its polls are the ones that should leave. The
redefinition now rewrites the flag-clear branch of every exit-capable
back-edge poll of each body it withdrew, on every thread at once:

* both x86-64 tiers record the condition-code byte of each poll a mode exit
  was admitted at (`CompiledMethod::exit_poll_sites`: the single-pass
  `emit_safepoint_poll_leaving_to`, the IR tier's `emit_safepoint_poll`,
  inline `JZ` and outlined `JNZ` alike) and the flag it tests;
* `JitCache::force_withdrawn_exit_polls` (`jit/src/not_entrant.rs`), called
  by `JitRealm::redefine_and_flush` after the not-entrant pass and the OSR
  memo's marks, checks each site's bytes against the three poll shapes and
  the flag's address (`forced_exit_poll_cc`), then clears bit 2 of the
  condition code in the same batched protection window the entry patch
  uses: `JZ` becomes `JO`, `JNZ` becomes `JNO`, and since `TEST` clears OF
  the poll always calls `jit_safepoint_slow_path`, whose verdict (unchanged
  since wave 23/24) sends the withdrawn frame out. One byte per site: a
  thread executing the poll during the write runs the old or the new branch,
  both correct;
* spared: a body compiled from the redefined class's own bytecode (its
  obsolete activation, which `withdrawn_body_may_leave` never lets leave),
  so no loop pays a slow path per back edge for a verdict that is always
  "stay".

Shape 1 (a peer blocked in a call) leaves at the first back edge after its
call returns; shape 2 (the redefining thread) likewise after
`retransformClasses` returns. The handshake is kept for peers that were
mid-iteration (HotSpot stops them at the redefinition's safepoint), and since
wave 25 it is taken once per `retransformClasses` / `redefineClasses` call
rather than once per redefinition (`NativeClassAccess::begin_redefinition_batch`;
each class of a retransform call is redefined twice, so N classes cost up to
2N pauses before).

Verified by `jit/src/x64/tests.rs::a_forced_exit_poll_asks_the_slow_path_with_the_flag_clear`
(the real single-pass emitter's poll, with the flag clear: no slow path
before forcing, one per back edge after, and out through the header's exit
map on a non-zero verdict), `ir_lower.rs::an_osr_door_compile_records_its_exit_polls_where_they_are`
(the IR tier's recorded sites pass the shape check on real code) and the
probe on the host.

## Wave 25 host result, and the follow-up (lane L6b)

The merged wave-25 build (198e98b95), JIT, default mode:
`parked after-old=1996998 after-new=false`, `self after-old=1995998
after-new=false` (HotSpot: 0 / true on both), and no `exit polls forced`
line under `CRATONVM_DBG_JITC=1`. About 3,000 / 4,000 iterations after the
call saw the new value and every later one the old: the frame left compiled
code once, ran interpreted until its back-edge counter reached the OSR door,
and then ran the value the class had BEFORE that retransform (12345 for
`parked`, 12346 for `self` -- not the retransformation base, which is 12345
for both). The loop re-entered code compiled before the redefinition, or
code that reached pre-redefinition bytecode, through a path the lane could
not pin down by reading: every OSR entry passes `try_osr`'s
`body_withdrawn_by_redefinition` refusal, which answers for a memo body
marked at the redefinition and for any body compiled before the whole-cache
barrier. A plausible first half: `parked`'s `if (i == WARM)` arm was never
taken while the body was compiled, so the optimizing OSR body may leave at
an unreached-code trap at the call itself, before the retransform -- then no
compiled frame runs during it, and forcing polls is moot for this probe.

The follow-up adds what the next host run needs to decide, and one guard:

* `exit polls forced:` prints on every redefinition, zeros included, with
  `not-withdrawn=`, `no-sites=` and `forced-before=` counts, and an
  `exit polls candidate:` line per OSR body the pass saw (label, compile id,
  tier, withdrawn, own class, recorded sites);
* the OSR door prints `OSR entry REFUSED (withdrawn by a redefinition)` and,
  once any redefinition withdrew a body, `OSR entry after a redefinition:
  <label> id=.. install_epoch=.. exit_sites=..` for every body it starts --
  the body the loop re-entered is named there;
* `osr_optimizing_cached` forgets a memo entry whose body a redefinition
  withdrew (`osr optimizing memo FORGETS a withdrawn body`), whatever its
  stamps say, instead of relying on the code-state epoch alone.

## Wave 25 second follow-up (lane L6c): the re-entered body, found

The L6b lines on the host (008e7cdf1) showed every step working -- forced
polls (`bodies=6 sites=11`), `withdrawn body told to leave: ...parked`, the
`OSR-exit TRANSFER`, `osr optimizing memo FORGETS` -- and then the loop
re-entering a NEW optimizing OSR body (`parked` id=12, `self` id=17, both
`install_epoch=7`, i.e. compiled after the first retransform) that still
counted every iteration "old". The body was not stale: it compared the NEW
value against the NEW value. The optimizing tier compiles the whole method
and enters through OSR stubs that seed only the loop header's live homes; a
loop-invariant local whose value the graph proved from the method-entry path
-- `int before = Callee.value()`, with `Callee.value` spliced to a constant
-- is folded into that constant and has no home, so the frame's own `before`
(computed before the retransform, with the old code) is ignored and the new
constant stands in for it. HotSpot's OSR compile takes every local from the
frame, so its body assumes none.

Such folding is only wrong when the frame computed the value with code that
changed since, i.e. across a redefinition of a class the body copied. Fixed
at the door: `try_osr` refuses to enter a body when a class it copied
(`CompiledMethod::copied_classes`, or named by `inlined_methods`) was last
redefined after the frame began (`Frame::redefine_stamp` against
`RedefinitionHistory::latest_redefinition`;
`jit_bridge::osr_body_may_fold_code_the_frame_predates`). The frame then runs
the rest of its loop interpreted, as it does under HotSpot's -Xint; a frame
pushed after the redefinition OSR-enters as before. Dbg:
`OSR entry REFUSED (the frame predates a redefinition of a class the body
copied)`. Unit test: `jit_bridge::i25_l6c_osr_frame_predates_redefinition_tests`.

Expected host result now, JIT: `parked after-old=0 after-new=true`, `self
after-old=0 after-new=true`. Cost: a frame that was already running when a
class its loop's body spliced was redefined finishes that loop interpreted.
Removing that cost needs the optimizing OSR compile to seed every
header-live local from the frame (no entry-path facts at an OSR entry), an
`ir_lower` / IR-builder change (lane L2).

**`value-now=12347` (HotSpot 12348), all modes including `--nojit` (fixed in wave 26, lane L3: `docs/internal/fixed-bugs/interpreter-L3-a-retransform-probe-sees-one-transform-fewer-than-hotspot-FIXED-20260928.md`):** not a
compiled-code question. The transformer bumps once per call; one call fewer
than HotSpot means the initial load of `Callee` did not run the
retransform-capable transformer (loaded before `addTransformer`, or the load
hook skipped it). Class loading / agent timing, outside lane L6; the
retransform-twice issue (i25-L3) adds a base swap but no transformer call.

## What remains

* **The rest of the iteration, outside OSR-door bodies.** HotSpot's frame
  returns into the interpreter at the call's return point; since wave 27 a
  single-pass OSR body does too, since wave 28 an optimizing OSR-door body,
  and since wave 29 a single-pass METHOD-ENTRY body with a loop-boundary map
  before the call and an optimizing OSR-door frame holding a lock (see
  "Progress (wave 29)"). Still running on from the return to the next
  exit-capable back edge (an optimizing METHOD-ENTRY body too until wave 38's
  sites are measured on the host): a single-pass
  method-entry call before the body's first loop header in bytecode order, or
  in a loop-free body, of a compile that keeps no dispatch record (since wave
  39 one that keeps one gets its site; the rest of such an activation runs its
  old splice once; a new call re-dispatches, the body being not entrant); a
  single-pass call whose successor files a deopt point of
  its own (a field or array access, another invoke, a conditional branch) or
  is a branch target; an optimizing call whose snapshot names a value with no
  location; a real call made INSIDE a
  spliced callee (the optimizing tier splices small callees without an
  exception table, e.g. `CountDownLatch.await()`; a deopt inside a splice
  may only re-execute the enclosing invoke, which a call that already
  returned forbids, so leaving there needs a caller-chain frame state the
  in-place transfer does not take); and a call past a compile's 64th site.
  Code after such a call in the same iteration that runs a splice of the
  redefined class runs its old bytecode once.
* **Loops with no exit-capable poll** (an elided monitor, a live operand
  stack at a `goto` the arm could not canonicalise, a loop rewrite;
  `x64/safepoint.rs::mode_exit_target`): these leave at no poll at all, pause
  or not, until the loop ends. (A splice's OWN loop is not a gap: HotSpot
  lets an activation of the old method that is already running finish on its
  old bytecode, JEP 109, and the caller's continuation after the splice
  leaves at the caller's next exit-capable back edge. aarch64 and bodies no
  list named: closed in wave 26, above.)

## Evidence

Wave 23 (lane L6) made a frame running a body a class redefinition withdrew
(one that spliced the redefined class's old bytecode) leave for the
interpreter at a back-edge poll: `helpers.rs::jit_safepoint_loop_exit_verdict`
answers "leave" for such a body (`jvmti_events.rs::withdrawn_body_may_leave`),
and `redefine_class_with` (`vm/src/vm/vm_exec.rs`) takes a loop-exit
handshake (`jvmti_events::request_withdrawn_body_exits`) so every running
peer passes its poll's SLOW path once, after the withdrawal. Wave 24 fixed the
optimizing OSR body's naming
(`docs/internal/fixed-bugs/interpreter-L6-a-running-splice-of-a-redefined-callee-runs-its-old-bytecode-FIXED-20260926.md`,
"Wave 24 correction").

The verdict is only ever computed on a poll's slow path, and a poll takes its
slow path only while a pause or handshake is requested. Two frames miss the
handshake:

1. **A peer blocked in a call.** A worker whose compiled loop (with the splice)
   is inside `CountDownLatch.await()`, `queue.take()`, `Thread.sleep`, ... is in
   a blocking region during the handshake, so the pause does not wait for it
   and it polls nothing. When the call returns into the compiled loop the
   handshake is over, every back-edge poll takes its fast path, and the loop
   runs the old splice until some unrelated safepoint request (a GC, the next
   agent pause) sends it through the slow path.
2. **The redefining thread itself.** `request_withdrawn_body_exits` is taken
   ON the redefining thread (`requester`), which does not poll during its own
   pause; when `retransformClasses` returns into its compiled caller, that
   caller's loop keeps the old splice the same way.

`tools/probes/interp/L6/RedefineSpliceAcrossACallProbe.java` has one loop of
each shape (`parked`, `self`). Expected on CratonVM with the JIT on:
`after-old=` large on both rows; HotSpot 25 prints `after-old=0` for both.

Wave 24's skip of the handshake when no other thread holds a JIT entry
(`conservative_roots::peer_threads_hold_jit_entries`) does not change either
case: shape 1's peer holds a JIT entry (so the pause is still taken, and
still misses it), shape 2 is not a peer.

## What HotSpot does

`Deoptimization::deoptimize_all_marked` runs at the redefinition's safepoint
and deoptimizes every frame of a marked nmethod on every thread, the current
one included: a frame that is not at the top of its stack gets its return
address patched to the deopt blob (`frame::deoptimize`), so the call returns
into the interpreter at the call's bci + length.

## Recommended fix

A return barrier for a withdrawn compiled frame parked in a call (i9-L5's
stage 3, "return-address patching"):

* At the handshake, the redefining thread walks each peer's JIT chain
  (`conservative_roots` already records each entry's `exact_rbp` and the
  innermost published compile id) and, for each frame whose body
  `JitCache::body_withdrawn_by_redefinition` answers for, replaces the saved
  return address its callee will return to (`[callee rbp + 8]`) with a
  per-VM return-barrier stub; the original address goes into a per-thread
  side table. The stub calls a helper that looks the original up and either
  resumes there (no longer withdrawn) or takes the body's exit at the call's
  return point: a frame state at the call with `ResumeSemantics::RESUME`
  (continue after the invoke, its result pushed), which neither tier
  publishes today -- every deopt point re-executes
  (`jit/src/deopt.rs`, `ResumeSemantics::for_reason`) -- so the call sites
  that the callee-deopt service already brackets (the single-pass
  `emit_inline_callee_deopt_check`, the IR tier's
  `emit_inline_callee_deopt_service`) would each need one. For the
  redefining thread, the same walk of its own stack before
  `redefine_class_with` returns.
* Cheaper, and covering shape 2 only: before `redefine_class_with` returns,
  when this thread's own compiled frames (`active_compiled_frames`) include a
  withdrawn body, request the loop-exit pause from a short-lived thread (as
  the retry path of `request_withdrawn_body_exits` does) so that this thread
  is a peer and polls on its way back -- but the request must be visible
  BEFORE the thread returns into compiled code, or a few iterations still run
  the old splice, so `NonMovingPause` needs a "request, then wait on another
  thread" split.
* Not recommended: keeping the poll byte raised until every such thread has
  polled -- every compiled poll of every thread would take its slow path
  meanwhile.

## How to verify

The probe above with the JIT on: `parked after-old=0`, `self after-old=0`,
under the default and `--compatible`; `--nojit` prints the same (the
interpreter reads the new method on its next call).

## Risk

High for the return barrier (stack rewriting on another thread, GC root walks
that must see the barrier's frame as the original caller's); low for the
shape-2 variant.

## Wave 27 host run (orchestrator)

`RedefineSpliceAfterTheCallProbe` with `CRATONVM_JIT_OSR_OPTIMIZING=0`
(`CRATONVM_JIT=osr-optimizing=0`) prints HotSpot's two lines in the default
mode and `--compatible` — but so does the wave-26 binary (three runs), and the
`CRATONVM_DBG=jitc` log shows the probe's `parked()` loop compiled at the
optimizing tier (`tier=C2 optimized=true osr_bci=10`) and no
`post-call-sites=` candidate line. So the host run gives no evidence yet that
the wave-27 post-call exit fires; the jit unit tests
(`a_forced_post_call_exit_leaves_at_the_calls_successor`, the shape check)
are its only proof. With default settings the `parked` row still prints
`first-after-new=false` (the optimizing tier, stage 3). A probe whose loop
provably runs a single-pass OSR body with the callee spliced is the next
step.
