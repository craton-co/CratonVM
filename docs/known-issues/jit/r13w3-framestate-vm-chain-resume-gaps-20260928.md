# Optimizing-tier caller chains: what the VM's resume side still cannot take

Status: OPEN
Area: `vm/src/runtime/interpreter/deopt_resume.rs` (`materialise_inlined_chain`, `caller_frame_values`, `resolve_inlined_callee`, `transfer_osr_guard_exit_into_live_frame`), `jit/src/osr_exit.rs` (`resumable_in_place_but_for_monitors`), `vm/src/runtime/interpreter/jit_bridge.rs` (`hand_monitor_to_resumed_frame` callers)
Severity: MEDIUM (blocks `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` / `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES` from becoming defaults; no wrong answer with the switches off)
Found by: round 13 wave 3 lane framestate (by reading, while landing W5-3)

Round 13 wave 3 made the optimizing tier publish inlined caller chains for guards inside
spliced bodies (`r13w2-irhash-putval-getnode-need-callee-frame-states-FIXED-20260929.md`). The
producer only publishes a chain the VM can rebuild (`jit/src/deopt.rs`
`inlined_chain_refusal`), and falls back to the flat frame otherwise -- except under the
chain fences, where it must refuse the compile instead. Four things on the VM side decide how
often that happens and how a refusal at RUN time ends. None is a defect with the switches off.

## 1. A chain naming a scalar-replaced object is never rebuilt

`caller_frame_values` maps every scope through `ir_deopt_locals` / `ir_deopt_frame_values`,
whose `fv_to_value` answers `None` for `VirtualObject` / `VirtualObjectRef`
(`deopt_resume.rs` ~97): the chain is refused, where the single-frame builder
(`build_deopt_frame_inner`) materialises the same object. So the producer treats such a chain
as unpublishable: with stage 2 the guard keeps the flat frame (and escape analysis may still
remove an object only a chain names -- `ea_ir_bridge::ea_snapshot_pins`); with the stage-3
fences EA must keep every object a chain names (`ea_claimed_chain_snapshots`), which blocks
exactly the optimization W5-3 is the precondition for (W3-5: the get key's box).

**Fix**: in `materialise_inlined_chain` / `materialise_inner_scopes`, run the virtual-object
materialiser per scope (ids are scope-local, as `deopt.rs`'s verifier already treats them) the
way `build_deopt_frame_or_refusal` does for one frame, under the same pin discipline, before
anything is pushed. Then relax `inlined_chain_refusal` for `VirtualObject`/`Ref` and drop
`ea_claimed_chain_snapshots`' "all chains when required" arm.
**Confirm**: a unit test in `deopt_resume.rs`'s chain tests with a `VirtualObject` in the
caller scope; then `R13FramestateNestedTraps` with `CRATONVM_SCALAR_DEOPT=1` in the fence arm.

## 2. The planless OSR guard exit refuses every chain

`osr_exit::resumable_in_place_but_for_monitors` requires `fs.caller.is_none()`, so
`guard_exit_fact` answers `None` for a body with ONE chain guard box, and
`transfer_osr_guard_exit_into_live_frame` refuses a chain outright ("inlined caller chain",
`deopt_resume.rs` ~3658). Under the switches an optimizing OSR body whose spliced body has a
guard therefore loses `ir_osr_guard_exit_bcis`, and the OSR door stops entering it (the
single-pass OSR body runs instead). A performance cost, in exactly the loops W5-3 is for.

**Fix**: the planned OSR exit already has a multi-frame sibling
(`transfer_osr_exit_chain_into_live_frame`: outermost scope written into the live frame, inner
scopes pushed). Give the planless guard exit the same shape, then let
`resumable_in_place_but_for_monitors` admit a chain the chain sinks accept (the outermost
scope's bci is the one `guard_exit_resume_bci` / the admitted list should carry). The
classification half (`classified_guard_exits`) needs no change: a chain guard is exact exactly
when its innermost reason is.
**Confirm**: `R13FramestateHashmapKernel`'s timing lines in the OSR arms with and without the
switches; `CRATONVM_DBG_IR_LINEAR_SCAN=1` prints `osr entries SKIPPED` for the body today.

## 3. A chain refused at run time is not the flat frame, it is a whole-method re-run

`materialise_inlined_chain` resolves every inner scope BY NAME from the enclosing scope's
class (`resolve_inlined_callee` -> `find_class_by_name_for_class`). A spliced body whose owner
the planner SUBSTITUTED (`SubstitutedOwners`, not a class constant of the caller) may not be
findable that way. Any refusal there falls through to the whole-method re-run; the replay rule
then answers for the whole body (`replay_from_entry_is_observably_equivalent_for_stash`, the
chain arm), so a side-effecting method gets `InternalError: precise deoptimization
unavailable` where the flat frame (stage 2) would have resumed precisely. Under the fences no
flat frame exists to fall back to at all.

**Fix** (either): carry the scope's `ClassId` in the chain (`FrameState` has no field for it;
`IrInlineSite::class_id` is known at compile time and the artifact could keep a
`method_key -> class_id` side table beside `_deopt_point_boxes`), or resolve an inner scope
through the artifact's own inline-frame map (`IrInlineFrameSites`, which already carries
`(method_key, class_id)` per level). Count refusals: `DeoptFrameBail::InlinedChainUnmaterialisable`
is the census row to read in a soak.
**Confirm**: `CRATONVM_DBG_DEOPT=1` on the probes in the switch arms must print
`PRECISE resume of a N-frame inlined chain` and never `inlined chain not materialisable`.

## 4. `ACC_SYNCHRONIZED` methods keep flat frames

A door that runs a synchronized body hands the method monitor to "the resumed frame"
(`hand_monitor_to_resumed_frame(guard, thread, frame_index)`). For a pushed chain that must be
the OUTERMOST frame (the synchronized method itself), and which index the callers pass after
`push_inlined_chain` was not established by reading. So the planner sets no
`Graph::scope_method_key` for a synchronized method (`lib.rs`, the inline planner), which
turns every chain off for it, and `deopt::point_needs_unsound_replay_of_method` takes a chain
point of a synchronized method as not resumed.

**Fix**: confirm (or make) every door pass the index of the FIRST frame `push_inlined_chain`
pushed, add a VM test that resumes a chain for a synchronized outer method and checks
`monitor_on_exit` lands on the outermost frame, then drop the `!cached.is_synchronized`
condition in `lib.rs` and the monitor clause in `point_needs_unsound_replay_of_method`.

## Round 13 wave 5 (lane resume)

**Item 1: landed (pending build), behind a new default-OFF switch; the page's
proposed fix was unsound as written.** Virtual-object ids are NOT scope-local.
`ir_lower::splice_chain_frame_state` resolves the WHOLE chain snapshot with one
`resolve_frame_values` call and one `emitted` set, then splits it per scope, so
an object two scopes name (the caller's local and the spliced callee's
argument) is `VirtualObject(id)` in one scope and `VirtualObjectRef(id)` in the
other. Materialising "per scope, ids scope-local" would find a dangling
reference, or give the two frames two different objects (a caller that later
reads the field its callee stored would see the old value). And the install
verifier (`DeoptVerifier::check_scope`) checked ids per scope, so it would have
refused every such chain with `UndefinedVirtualObjectRef`. What landed:

- `jit/src/deopt.rs`: `inlined_chain_refusal` admits `VirtualObject` /
  `VirtualObjectRef` under `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS=1`
  (`chain_virtuals_enabled`, default OFF, read only for a chain that names
  one), refusing still: any monitor in the chain, a reference no scope
  defines, an id defined twice, a field the materialiser cannot store, a
  reference-array recipe. The verifier reads ids chain-wide
  (`ChainVirtualIds`; a flat frame is one scope, unchanged). OSR admission
  refuses a chain point naming a virtual object (`first_unresumable_slot` /
  `first_unresumable_local`, `chain_value_blocks_osr`): the in-place OSR
  chain transfer has no materialisation path. `frame_state_is_resumable` is
  unchanged (it answers for the method-entry sinks). Tests
  `r13w5_resume_chain_virtual_tests`.
- `vm/src/runtime/interpreter/deopt_resume.rs`: `build_deopt_frame_chain`
  (every chain sink's builder) validates such a chain over null placeholders
  and records a `ChainVirtuals`: all scopes' locals/stacks concatenated into
  ONE `ReconstructedFrame` (outermost first). `materialise_chain_virtuals`
  runs the existing materialiser ONCE over it (one shell per id, every
  ordinary reference of every scope pinned across the allocations and re-read),
  re-slices the scopes and rebuilds every frame's values; `push_inlined_chain`
  calls it right before the push and releases its pins after. A refusal
  there pushes nothing: the tier-up sink and the call-site service already
  answer `None` from the push with an error and no replay;
  `resume_real_ir_deopt_or_throw` calls it first, so a full heap is the
  single frame's `OutOfMemoryError`. Refused at the VM too: a monitor in any
  scope (a failed re-allocation would abandon a held lock with no release
  path), an `ACC_SYNCHRONIZED` outermost method. Tests
  `r13w5_resume_chain_virtual_tests` (one object for two scopes, survives a
  forced GC through the pushed frames alone; a malformed graph pushes nothing
  and leaves no pin).
- Found on the way and fixed: under `CRATONVM_DEOPT_VERIFY` the chain builder
  checked no scope's reference words and the OSR-exit chain transfer only
  the trapping scope's (`verify_chain_oops`, test
  `the_chain_oop_check_reads_every_caller_scope`).

Still needed before the switch can mean anything: escape analysis must be
allowed to remove an object a chain names and the chain snapshot must carry
its recipe (today `ea_snapshot_pins` / `ir_prune_unconsumable_snapshots` pin
or drop instead). `r13w5-resume-ea-chain-virtuals-producer-patch-FIXED-20260928.md`.

**Item 2 (planless OSR guard exit): not landed.** The producer half is
`jit/src/osr_exit.rs` (not this lane's). The VM half is also more than the page
says: after a chain transfer the OSR door
(`jit_bridge.rs` ~9469) rewrites the LIVE frame's lock record from the exit
frame (`replace_live_record_with_retaken_levels_and_unpin`), which for a chain
must use the OUTERMOST scope's monitors only, and the elided-level re-take in
`transfer_osr_guard_exit_into_live_frame` belongs to the outermost scope too.
That is monitor/lock-record code on the deopt path, which this wave leaves to
lane sync2. What a later wave must do, in order: (a) `osr_exit::guard_exit_fact`
admits a chain box whose innermost reason is exact and whose chain
`inlined_chain_refusal` accepts, carrying the OUTERMOST scope's bci in
`ir_osr_guard_exit_bcis`; (b) `transfer_osr_guard_exit_into_live_frame`
delegates a chain to `transfer_osr_exit_chain_into_live_frame` (same shape:
outermost in place, inner scopes pushed), returning the outermost resume pc;
(c) the door's lock-record rewrite reads `stash_identity_scope(rframe)`.
Also note for (a): a guard in a spliced body resumed by the interpreter runs
the rest of the CALLEE, then returns into the outer frame at the invoke's
successor; re-entering the OSR body later at a loop header is sound exactly
as for a flat exact guard.

**Item 3 (inner scopes resolved by name): not landed**, needs a class id per
scope from the compile: `r13w5-resume-chain-scope-class-ids-patch-FIXED-20260928.md`.

**Item 4 (synchronized methods), VM half confirmed by reading.** Every door
that holds the method monitor takes `resume_depth = thread.frames.len()`
BEFORE `resume_or_despeculate_stash` (`jit_bridge.rs` `jit-callsite-a` ~21420,
the `_decoded` twin ~21590, `sync-door-oneshot` ~22170), and
`push_inlined_chain` pushes the OUTERMOST frame first at exactly that index
(`first_pushed = thread.frames.len()`), so `hand_monitor_to_resumed_frame`
lands `monitor_on_exit` on the synchronized method's own frame, whose return
releases it after the inner frames returned. The tier-up sink holds the
monitor for the whole `execute` and runs the chain to completion inside it.
Not done: the `lib.rs` planner still gives a synchronized method no
`scope_method_key`, and `point_needs_unsound_replay_of_method` still takes a
synchronized method's chain point as not resumed. Both stay until lane sync2's
synchronized-body arms (the sync-direct calls, which may hand the monitor to a
frame by another rule) are read against a pushed chain. The VM chain builder
refuses chain virtuals for a synchronized outermost method either way.

Status stays OPEN (items 1-producer, 2, 3, 4-planner).

**Found by reading, a fifth gap (partly fixed this wave): a spliced callee's
class redefined after the compile.** Inner scopes are resolved NOW, so a chain
trapping after `P` was retransformed rebuilt `P.m`'s frame at the old bci in
`P.m`'s new bytecode (the outer class's redefinition check never looked at
`P`). The doors now refuse to resume such a chain and re-run as for a
redefined class (`chain_inner_scope_redefined_since_compile`, kill switch
`CRATONVM_DEOPT_CHAIN_INNER_REDEFINED_REFUSES`); the rest is
`r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`.

## Round 13 wave 6 (lane chain2)

**Item 1 (chains naming scalar-replaced objects): the producer is complete** (pending build and the
one-line `lib.rs` hand-over `r13w6-chain2-lib-prune-described-patch-FIXED-20260928.md`); see
`r13w5-resume-ea-chain-virtuals-producer-patch-FIXED-20260928.md`'s wave-6 section. The blocker was in the
lowerer: a chain snapshot's objects were described against the flat frames' site index, which never
answers for a chain, so every such object was `MaterializationRequired`.

**Item 2 (planless OSR guard exit): not landed**; the three parts and who owns each are an exact
design now, `r13w6-chain2-osr-guard-exit-chain-lock-record-patch-FIXED-20260928.md` (the lock-record half is
lane sync3's). Found while writing it: `transfer_osr_exit_chain_into_live_frame`'s provenance check
(`artifact.deopt_points` holding `rframe.bci`) refuses every optimizing-tier chain -- chain points live
in the boxes only, and `rframe.bci` is the callee's -- so the planned OSR exit could not take one either.
That is a refusal, not a wrong answer; the page says how the planless caller bypasses it.

**Item 3 (inner scopes resolved by name): fixed** by the artifact's scope class ids
(`r13w5-resume-chain-scope-class-ids-patch-FIXED-20260928.md`), in every sink this lane owns. Also fixed
there: a rebuilt inner frame took its class NAME from the scope's key and its id and bytecode from the
declaring class the key resolves to, so a method inherited through the key's class (`invokestatic
Sub.helper`, a `final` class's inherited method) made a frame whose name and code disagreed (stack
traces, JVMTI); it now carries the declaring class's name.

**Item 4 (synchronized methods): unchanged** -- the planner half waits for lane sync3's sync-direct
arms, as wave 5 said.

**New, found by reading and fixed: every frame a chain pushes reported `last_instr_pc` 0.** The
unwinder searches a CALLER frame's exception table at its `last_instr_pc` ("the invoke itself, since
`pc` has already advanced", `interpreter::unwind_to_handler`), and a frame the chain builder built had
never executed an instruction. So an exception the resumed callee threw -- the ordinary outcome of a
JVMS-check trap inside a splice: the interpreter re-executes the `getfield` / `iaload` and it throws --
was matched against the caller's handlers covering pc 0: a `catch` around the call that does not start
at pc 0 missed it (the exception escaped the method), and one that starts at pc 0 but does not cover
the call caught it. The OSR-exit chain transfer left the live frame's stale value the same way.
`InlinedChainFrame::executing_pc` (each scope's bci) now becomes the pushed frame's `last_instr_pc`,
and the live frame's; kill switch `CRATONVM_DEOPT_CHAIN_LAST_INSTR_PC` (default on). Test
`r13w6_chain2_scope_hint_tests::a_pushed_caller_frame_reports_the_invoke_it_is_parked_in`; probe
`R13Chain2HandlerAroundSplice` (its `around` / `atzero` / `nested` / `wide` cases).

**New, found by reading, exact patch (HIGH in the chain arm): the first-call tier-up sink re-runs its
own chain from entry.** It judged the stash by the innermost key (the spliced callee's), read every
chain of its own body as a foreign stash, and re-ran the method from bci 0 with no replay check:
`r13w6-chain2-tierup-sink-reruns-its-own-chain-patch-FIXED-20260928.md`, probe `R13Chain2TierupChain`.

Status stays OPEN (items 2 and 4, and the tier-up sink patch).

## Round 13 wave 6 hand-back (lane chain2): a pushed chain ran from its INNERMOST frame

**Observed** (orchestrator, build w6c = wave 6 plus both lane-chain2 patch pages):
`R13CrashNullReceiverFields` passes in the default arm and fails in
`CRATONVM_JIT_IR_SPLICE_FRAME_STATES=1 CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES=1` (with or without
`CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS=1`, with `CRATONVM_C2_ACCEPT=always`): HotSpot prints
`ok=3105785 npes=408` / `done`, CratonVM an uncaught `NullPointerException: Cannot load from
byte/boolean array because "this.hb" is null` at `main(99) -> bufGet(59) -> Buf.get(29)`. Shape:
`bufGet` = `try { return b.get(i); } catch (NullPointerException e) { return -4000; }`, `Buf.get`
= `return hb[off + i];` spliced into `bufGet`, `hb` null every 512th call.

**Why the `last_instr_pc` fix did not cover it: the handler was never searched at all.** The chain
IS resumed (the trace shows the rebuilt `Buf.get` frame), `Buf.get` re-executes the `baload` and
throws, and then the sinks that run a pushed chain to completion ran it through
`interpreter::run_pushed_frame_to_completion`, whose `execute_frame` takes the TOP frame -- the
innermost scope -- as the interpreter entry's floor. `unwind_to_handler` searched `Buf.get`, found
nothing, and returned the exception OUT of the entry (`frame_idx == initial_frame_idx`);
`pop_root_frames_keeping_result` then popped `bufGet`'s pushed frame unsearched, and the exception
reached `main`. A normal return was wrong the same way: the entry ended at the callee's `ireturn`
and the door handed the CALLEE's value back as the outer method's, the outer frame popped without
running the rest of its body (e.g. `readAfterStore(c, b) + y` returned only `readAfterStore`'s value).
`last_instr_pc` only matters once the unwinder reaches the caller frame, which it never did.

**Fixed in lane chain2's file** (pending build): `deopt_resume::run_deopt_frame_chain_to_completion`
(the call-site service `helpers::try_resume_trapped_callee` and the first-call tier-up sink) runs the
chain through `run_pushed_chain_to_completion`, whose floor is the OUTERMOST pushed frame
(`execute_frame_from_index(.., depth_before)`): an inner return continues its caller at the invoke's
successor, an exception is offered to every pushed frame's handlers, and the entry ends when the
outermost frame returns or throws. Kill switch `CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST` (default on).
Tests `r13w6_chain2_scope_hint_tests::a_chain_returns_through_its_outer_frame` (6, not the callee's
5) and `an_exception_from_the_inner_frame_reaches_the_outer_handler` (the outer catch-all runs).

**Left, exact patch (HIGH):** the `jit_bridge` one-shot doors (`resume_deopted_body_at_point`) and the
synchronized one-shot door resume a stash and then call `run_pushed_frame_to_completion(..,
frames_depth_on_entry)` themselves, with the same innermost floor. One line in `interpreter.rs`
(`execute_frame` -> `execute_frame_from_index(.., frames_depth_before_push)`, identical for every
one-frame caller): `r13w6-chain2-run-pushed-frame-floor-patch-FIXED-20260928.md`. Which of the two paths
`R13CrashNullReceiverFields` took on w6c (a direct compiled call's callee-deopt service, or a dispatch
helper's one-shot door) is not known from the report; both are needed. The doors that return
`FramePushed` to the interpreter's own loop were never affected: their chain runs inside the caller's
entry, whose floor is below it.

**This blocks every chain switch default**: a chain resumed through any run-to-completion sink turned a
caught exception into an uncaught one, and a normal resume into a wrong return value.

## Round 13 wave 8 (lane chain3): what landed, and the soundness argument for the defaults

### Landed (pending build)

1. **Item 2, the planless OSR guard exit, parts (a) and (b)** (part (c), the live frame's lock
   record, by lane sync5): `jit/src/osr_exit.rs` admits a monitor-free, virtual-free chain box and
   lists it at the OUTERMOST scope's bci; `deopt_resume::transfer_osr_guard_exit_into_live_frame`
   delegates a chain to the planned exit's in-place chain transfer, with provenance by point
   address. Kill switch `CRATONVM_JIT_OSR_CHAIN_GUARD_EXITS` (default ON). Details, and why a
   monitor-holding chain stays refused: `r13w6-chain2-osr-guard-exit-chain-lock-record-patch-FIXED-20260928.md`.
2. **Found and fixed: a chain transfer inside the OSR door continued in the wrong frame**
   (`interpreter.rs` `try_osr_offer`). Both in-place chain transfers push the spliced callees above
   the live frame; the door answered `Skip` and the dispatch loop went on in the LIVE frame, parked
   after its `invoke` with no return value, the callee frames stranded above it. Latent until (b)
   (the planned chain transfer was unreachable); now the loop moves to the top frame
   (`ContinueDispatch`).
3. **Item 5 (redefinition), the outermost scope too.** `chain_outer_scope_redefined_since_compile`
   (kill switch `CRATONVM_DEOPT_CHAIN_OUTER_REDEFINED_REFUSES`, default ON): a chain whose OWN
   method's class was redefined after the compile is not resumed by the doors
   (`real_frame_deopt_resume_or_throw_and_despeculate`; they then re-run as for any redefined
   class's frame) nor by the first-call tier-up sink (which refuses the replay). A single frame has
   the own-source resume and the constant-pool restamp for this; a chain has neither, and its
   outermost frame was pushed from the CURRENT bytecode at the old bci.
4. **The call-site service** (`helpers::try_resume_trapped_callee`, the last chain sink with neither
   the artifact's scope class ids nor a redefinition check): `build_deopt_frame_chain` now refuses
   any chain once a class of the VM was redefined, and the new `build_deopt_frame_chain_for_point`
   resolves by the trapping body's class ids with both redefinition checks; wiring it in is the
   one-line patch `r13w8-chain3-helpers-callsite-chain-by-point-patch-FIXED-20260928.md` (kill switch
   `CRATONVM_DEOPT_CHAIN_CALLSITE_BY_POINT`, default ON).
5. **A full heap during a chain's re-allocation** at the run-to-completion sinks (the call-site
   service, the tier-up sink) is now `OutOfMemoryError` at the caller, as for a single frame and as
   the doors already answered; it was an `InternalError` ("could not push a chain").

Tests: `jit/src/osr_exit.rs` `r13w8_*`; `deopt_resume.rs` `r13w8_chain3_tests`. Probes
`C:\craton\jitr13-probes\src\R13Chain3{LockedTrapTwoDeep,OuterCatch,ChaBreak,VirtualAcrossTrap}.java`.

### The argument: when is a trap inside a spliced body resumed correctly?

Setting: `CRATONVM_JIT_IR_SPLICE_FRAME_STATES=1` (stages 1-2) and
`CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES=1` (stage 3), `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` either way.
Claim: every deopt point inside a spliced body of an installed optimizing body either resumes the
exact JVM state (callee frames at their own bcis over callers parked mid-`invoke`), or ends in an
answer no worse than the same trap gets in the default build -- except the residuals under "Still
refused", none reachable without a class redefinition, a full heap, or the one open patch.

**A. What a spliced body can contain** (`jit_bridge::resolve_inline_site_from`): never an
`ACC_SYNCHRONIZED` method, never a `Code` attribute with an exception table
(`callee-exception-table`), never `monitorenter` / `monitorexit`. So an INNER scope holds no monitor
and has no handler; the only monitors a chain can name are the outermost method's own, and the only
handlers the unwinder can meet are the outermost method's (and its callers').

**B. What the producer publishes.** A chain snapshot is recorded at every spliced pc of a method with
a `scope_method_key` (every non-synchronized method). A guard publishes a chain iff
`deopt::inlined_chain_refusal` is `None`: depth <= 9; every value a constant, a spilled word,
`Undefined` or (with CHAIN_VIRTUALS) part of a chain-wide consistent virtual graph; no elided lock; no
monitor outside the outermost scope; no monitor at all beside a virtual. Otherwise stage 2 keeps the
flat frame at the outermost `invoke` (correct, because stage 2 relaxes no replay fence) and stage 3,
where the fence was relaxed on the strength of the chain, refuses the compile
(`splice_chain_for_site` latches a bailout): a body that cannot describe a guard is never installed.
The innermost scope's semantics are the trap's (`REEXECUTE` for every guard); every caller scope is
`RESUME` after its `invoke`. The install verifier checks every scope against its method's limits and
the virtual ids chain-wide. Exceptional (reason-9) frames are always flat, and correct flat: by A a
throw inside a splice ends every inner frame, and the outer frame's handler lookup happens at the
`invoke` bci the flat frame names.

**C. The compile-time replay check agrees with B.** `deopt::point_needs_unsound_replay_of_method`
takes a chain point as resumed exactly when `inlined_chain_refusal` is `None` and the method holds no
method monitor; so the installed bodies whose correctness depends on a chain being resumed are
exactly those whose chains the producer vouched for.

**D. Every sink rebuilds exactly what B published, or refuses before anything is pushed.**

| sink | identity | inner-scope resolution | redefinition | how the chain runs | refusal answer |
|---|---|---|---|---|---|
| method-entry doors (`jit-callsite-a/-b`, `_decoded`, `sync-door-oneshot`), lambda direct arm (`resume_deopted_body_at_point`) | outermost scope, point owned by the body run | artifact's class ids | inner (w5) and outer (w8) refuse | `FramePushed` into the caller's loop, or run from the OUTERMOST pushed frame (w6 floor patch) | re-run as a redefined class's frame; OOME for a full heap |
| first-call tier-up sink (`interpreter.rs`) | outermost scope (w6 patch) | artifact's class ids | inner (w6) and outer (w8) refuse | `run_deopt_frame_chain_to_completion`, outermost floor | `InternalError`, no replay; OOME for a full heap (w8) |
| call-site service (`helpers::try_resume_trapped_callee`) | outermost scope (i1 w26) | BY NAME until the w8 patch, then the trapping body's class ids | any redefinition refuses (w8); with the patch, inner and outer | outermost floor | restash, the sentinel travels out (see "Still refused") |
| OSR door, planless guard exit (w8) | point address among the body's boxes, admitted bci | artifact's class ids | inner refuses; the live frame reads its own code | outermost scope in place, inner scopes pushed, dispatch moves to the top frame (w8) | admission refuses first: the body is not entered |
| OSR door, planned exit | recorded bci | artifact's class ids | inner | as above | unreachable: no single-pass chain is published |
| IR int-only sink (`resume_from_ir_deopt`, opt-in `CRATONVM_IR_DEOPT_RESUME`) | outermost scope | by name | any redefinition refuses | `FramePushed` | falls through to the real sink |

Per frame the rebuilt state is the JVM's: the innermost at its own bci (`scope_resume_pc`), callers
at their `invoke`'s successor with `last_instr_pc` = the `invoke` (w6: the unwinder searches a
caller's handlers there), locals by JVM slot, the operand stack below the popped arguments, the
outermost frame's taken locks in its `held_monitors` (inner scopes: none, by A), ONE heap object per
virtual id shared by every scope that names it, and every reference rooted from the materialiser's
pins to the pushed frames with nothing allocating in between (`push_inlined_chain`).

**E. The continuation after the resume is the interpreter's.** An inner frame's return continues its
caller at the successor with the value on its stack; an exception unwinds the inner frames (no
handlers, by A) into the outermost frame's table at its `invoke`. That is the execution the
un-inlined program has from that JVM state, so correctness reduces to the state being right: B + D.
For an OSR'd body re-entered later, the planless exit's argument is the flat one: an EXACT guard's
re-executed bytecode throws out of the method (every listed bci, the outermost `invoke`, lies outside
the exception table), a VALUE-EQUIVALENT guard's continuation computes what the body would have.

### Still refused (and what each costs)

* **`ACC_SYNCHRONIZED` compiling methods get no chains** (item 4's planner half: `lib.rs` sets no
  `scope_method_key`, so their call and store fences stay; lane hashsplice wave 5 item 3). Sound,
  only slower. **Hook needed from lane sync5** to lift it: for a SELF-LOCKING synchronized body (the
  compiled code takes the method monitor itself), where a pushed chain must record the method
  monitor. Today `push_inlined_chain` seeds the outermost frame's `held_monitors` from the outermost
  scope's taken locks, and a door that holds the monitor hands it to that same frame
  (`hand_monitor_to_resumed_frame` at `resume_depth`); sync5 should state which of the two owns it
  for a self-locking body and that no path records it twice (or give a predicate the planner can
  ask). Then the planner condition and the monitor clause of `point_needs_unsound_replay_of_method`
  can go.
* **A chain naming a scalar-replaced object with `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` off**: the
  producer keeps the allocation (stage 3) or the flat frame (stage 2). With it on, such a chain with
  any monitor is refused by the producer.
* **OSR planless guard exits**: a chain holding a monitor or naming a virtual object is refused at
  ADMISSION, so the body is not entered (never a refusal after the body ran).
* **A class redefined after the compile** (a spliced callee's, or the method's own): the chain is not
  resumed; doors re-run as every redefined-class frame does, the tier-up sink raises
  `InternalError`, the call-site service restashes. The per-scope own-source resume is proposal
  CH3-1.
* **A full heap during a chain's re-allocation**: `OutOfMemoryError` at the caller (HotSpot's
  answer).
* **The call-site service, until `r13w8-chain3-helpers-callsite-chain-by-point-patch-FIXED-20260928.md`
  is applied**: an inner scope whose class the enclosing class's loader cannot name (a JDK method
  splicing an application callee, `HashMap` and a user key's `hashCode`) is refused, the frame
  restashed, and the next method-entry door re-runs ITS method from entry as a foreign stash -- the
  one path by which the chain arm turns a precise resume (the flat frame's, today) into a replay.
  **That patch is a precondition for the flip.**

### Verdict

With `r13w8-chain3-helpers-callsite-chain-by-point-patch-FIXED-20260928.md` applied, I judge
`CRATONVM_JIT_IR_SPLICE_FRAME_STATES` and `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES` sound to flip to
default ON: their readers are `ir::ir_splice_frame_states_enabled` and
`ir::ir_splice_chain_fences_enabled` (`jit/src/ir.rs`, both `runtime_flag_on` today ->
`runtime_flag_default_on`), and their rows in `types/src/flag_groups.rs` (`ir-splice-frame-states`,
`ir-splice-chain-fences`) need an `off_word`. Keep `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` default OFF
until it is measured on its own (sound either way; it only decides whether escape analysis may
remove an object a chain names). Measure in the handoff's order -- FRAME_STATES alone, then
+ CHAIN_FENCES, then + CHAIN_VIRTUALS -- on the R13 battery (with `R13Chain3*`, `R13Chain2*`,
`R13Resume*`, `R13Framestate*`, `R13Irhash*`, `R13CrashNullReceiverFields`), each also with
`CRATONVM_C2_ACCEPT=always`, and read `deopt_frame_bail_counts()`
(`inlined-caller-chain-unmaterialisable`) and `door_rerun_census()` (`foreign-stash` and
`own-stash` unsound columns): both should stay 0 outside redefinition tests. Tests pinned to the old
default must pin the switch (`with_thread_overrides`), as the handoff's trap list says. Not flipped
here.

Status stays OPEN: item 4 (synchronized methods, waiting on sync5's statement above), the helpers
patch, and the measurement.

## Round 13 wave 9 (lane chain4)

### Item 4 (synchronized compiling methods): the question to lane sync5, answered

Chain3 asked which of two paths owns the method monitor of a synchronized method whose trap is
resumed as a pushed chain -- `push_inlined_chain`'s seeding of the outermost frame's
`held_monitors` from the outermost scope's taken locks, or the door's
`hand_monitor_to_resumed_frame` -- and whether any path records it twice. By reading, after this
wave's hand-over (`r13w4-sync2-self-locking-bodies-with-deopt-exits-20260928.md`, wave 9):

* **The optimizing tier never describes a method monitor.** Its frame states name the builder's
  `monitorenter` stack only, and a synchronized method it compiles is always a WRAPPED body (no IR
  self-locking body exists; `single_pass_self_lock_preferred` keeps self-locking methods on the
  single-pass tier). So `push_inlined_chain` seeds no method monitor for an IR chain, and the only
  owner is the door's guard, handed to `frames[resume_depth]`, which is the OUTERMOST pushed frame
  (wave 5's reading, unchanged). No double record.
* **A self-locking body (single-pass) hands its own hold over, as the frame's only
  `MonitorInfo`**, and only for a flat frame (`handed_method_monitor_index` refuses any chain; the
  single-pass tier publishes none). The builders make it `monitor_on_exit`; a door that also holds
  a level now releases its own instead of overwriting the frame's
  (`transfer_to_resumed_frame`). No double record either.
* **So the doors could take chains of a synchronized method today.** What still blocks lifting
  the planner condition (`lib.rs`, no `scope_method_key` for `ACC_SYNCHRONIZED`) is the CALL-SITE
  SERVICE: `helpers::try_resume_trapped_callee` refuses a synchronized callee's stash unless it
  carries a handed hold, because it cannot tell whether the call it serves is caller-held (the
  resumed frame must then own nothing) or not; a refused chain there re-runs the callee from entry
  (`rerun_declined_callee_from_entry`), which for a body whose fences the chains relaxed is an
  unsound replay (an `InternalError`, `CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES`). The replay
  check's monitor clause in `deopt::point_needs_unsound_replay_of_method` (a synchronized method's
  chain point is "not resumed") is exactly what keeps such a body from being installed, so lifting
  the PLANNER condition alone would be sound but would refuse the relaxed compiles (a slowdown),
  and lifting the clause as well would be unsound until the service resumes under a caller-held
  hold. Proposal CH4-1 (`jit-r13-chain4-proposals-RETIRED-20260929.md`) is the service half. Item 4 stays open; it
  is not a precondition of the flip (a synchronized method keeps flat frames and its fences).

### The flip, re-read

Nothing found this wave that changes chain3's verdict. Read again for this wave, with the
multi-return arm in mind: the two chain producers (`emit_deopt_unless`, the `Op::Guard` arm) and
every other exit a spliced node can take under the chain fences -- a call's exception exit and
rethrow pad (the outer frame at the outermost `invoke`, whose locals a splice cannot change; an
exception out of a spliced body is never a replay), the getfield/putfield helper sentinels
(exceptions), back-edge poll mode exits (refused inside a spliced body,
`back_edge_mode_exit_state`), a compiled callee's unserviced deopt (a FOREIGN stash at the door:
the whole-body replay rule, whose spliced half reads the relocated bytecode,
`spliced_bodies_side_effect_free`, and raises instead of replaying,
`CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES`; the same answer a call in the outer body gets).
A multi-return splice changes none of them: its caller scopes are the same `SpliceFrame` state on
every return path. The `r13w8-chain3-helpers-callsite-chain-by-point-patch` is applied (helpers.rs
~8540). Verdict recorded on `r13w2-irhash-putval-getnode-need-callee-frame-states-FIXED-20260929.md`.

Probe added: `C:\craton\jitr13-probes\src\R13Chain4VirtualChainThrow.java` (a three-frame chain
with side effects at every level, a receiver-guard resume that continues and a bounds resume that
throws into the outermost frame's handler, one scalar-replaceable box shared by all three frames).

Status stays OPEN (item 4, and the measurement).

## Round 13 wave 11 (lane chain5): two holes closed, CH-1 landed, and the verdict

Re-read by this lane: the chain producer (`ir.rs` `splice_scope_chain` / `push_splice_state` /
`relax_fence_by_chain`, `ir_lower.rs` `splice_chain_for_site` / `splice_chain_frame_state`), the
producer's gate (`deopt::inlined_chain_refusal`), every chain sink of the table above
(`deopt_resume.rs` `build_deopt_frame_chain_for_point` / `_hinted`, `materialise_inlined_chain`,
`push_inlined_chain`, the run-to-completion pair, both OSR chain transfers,
`real_frame_deopt_resume_or_throw_and_despeculate`), and the HS-2 halves (planner, resolver,
builder). Items 1-3 and 5 hold as the earlier sections say. Landed (pending build):

1. **HIGH in the chain arm, fixed: a guarded splice's exact fact leaked past its join.** The I8-2
   step of the HS-2 builder patch (wave 10, `SpliceFrame::receiver_exact_class`) let a nested
   exact-receiver proof lean on a GUARDED frame -- correct inside the hit edge -- but
   `exact_receiver_splice_admits` then recorded the receiver NODE in
   `IrBuilder::exact_receiver_nodes`, the build-wide "exactly C" record, and that node (a field
   load, an array element) flows on past the join, including from the miss edge. A later splice
   handed the same node -- `m.put(k, v); helper(m)`, `helper`'s nested `m.size()` -- spliced the
   base class's body with no test, so a subclass receiver got the wrong method (a silent wrong
   answer; only with `CHAIN_FENCES`, which is what creates nested exact rows). Now the node is
   recorded only when the proof holds without any guarded frame
   (`exact_receiver_node_proof_scoped`); kill switch `CRATONVM_JIT_IR_GUARDED_EXACT_FACT_SCOPED`
   (default ON). Tests `ir::r13w11_chain5_guarded_exact_tests`; probe
   `C:\craton\jitr13-probes\src\R13Chain5GuardedLeak.java` (bad > 0 in the chain arm with the
   kill switch at 0, 0 otherwise).
2. **Fixed: the production run-to-completion sinks lost wave 8's full-heap answer.** Item 5 of the
   wave-8 section (a full heap during a chain's re-allocation is `OutOfMemoryError` at the caller)
   lived in `run_deopt_frame_chain_to_completion`, which gcd d1/b made test-only when it moved the
   call-site service and the tier-up sink to `run_deopt_frame_chain_to_completion_releasing_pins`;
   there `push_inlined_chain`'s own re-allocation answered `None`, and both sinks turn that into an
   `InternalError` ("could not push a chain"). The `_releasing_pins` twin now re-allocates first and
   answers `OutOfMemoryError` for a heap failure (the existing `CRATONVM_DEOPT_REMATERIALISE_OOM`
   rule, `frame_build_heap_failure_is_answered`). Only with `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS=1`.
3. **CH-1, forced chain deopts** (`CRATONVM_DEOPT_EAGER_CHAINS=1`, debug, default OFF): every chain
   point's guard also takes its pad on every 64th PASSING arrival, so every probe becomes a
   chain-resume test that keeps running after the resume. Details on
   `jit-r13-chain2-proposals-RETIRED-20260929.md` CH-1; soak probe `R13Chain5ForcedChainSoak`.

### Verdict on flipping the three switches (soundness only)

**Sound to flip `CRATONVM_JIT_IR_SPLICE_FRAME_STATES`, `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES` and
`CRATONVM_JIT_IR_SPLICE_MULTI_RETURN` together, in a build that carries fix 1 above** -- without
it the chain arm has a wrong-answer path (HS-2 + I8-2, any field- or array-held receiver passed on
after a guarded splice). The argument is chain3's (A-E above), unchanged by this wave, plus: HS-2's
nested splices add no deopt state (their traps publish chains like any spliced body's) and their
proofs are now scoped (fix 1); `MULTI_RETURN` adds no deopt state (chain4). Keep
`CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS` OFF. Conditions and residuals:

* **Soak first with CH-1**: the R13 battery (`R13Chain*`, `R13Resume*`, `R13Framestate*`,
  `R13Irhash*`, `R13Iropt8*`, `R13CrashNullReceiverFields`, the three `R13Chain5*`) in the chain
  arm with `CRATONVM_DEOPT_EAGER_CHAINS=1` and `CRATONVM_C2_ACCEPT=always`; no probe's checked
  lines may change. `deopt_frame_bail_counts()` `inlined-caller-chain-unmaterialisable` 0 and
  `door_rerun_census()` unsound columns 0.
* **Residual 1, agents only**: after a hot swap of a class whose method a RUNNING compiled
  activation spliced, a trap in that splice is not resumed (item 5): the doors re-run (the replay
  rule may then raise `InternalError`), the tier-up sink raises `InternalError`, the call-site
  service restashes. The default arm re-executes the callee from its new entry instead. Proposal
  CH-5 / CH3-1 (the per-scope own-source resume) is the fix;
  `r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`.
* **Residual 2**: synchronized compiling methods keep flat frames and their fences (item 4 below;
  sound, slower).
* **`MULTI_RETURN`'s reader** is a process `OnceLock` (`ir::ir_splice_multi_return_enabled`): a flip
  has to change it to `runtime_flag_default_on` (proposal CH5-4), and tests pinned to the old
  default must pin it.
* **Performance is the blocker, not soundness**: w11a measured the chain arm 7% SLOWER on
  `CratonBench hashmap` than the default arm although it removes two calls per pair:
  `r13w11-chain5-chain-arm-hashmap-kernel-slower-than-default-20260928.md` (phi residency, chain
  snapshots that keep dead locals alive -- proposal CH5-1). Not flipped here.

**Item 4 (synchronized compiling methods), unchanged**: the planner condition (`lib.rs`, no
`scope_method_key` for `ACC_SYNCHRONIZED`) and the monitor clause of
`deopt::point_needs_unsound_replay_of_method` stay until the call-site service resumes a chain
under a caller-held hold (proposal CH4-1). Not a precondition of the flip.

Status stays OPEN (item 4; the perf page above decides the flip).

## Round 13 wave 13 (lane syncres): item 4, re-read

Status stays OPEN (item 4 unchanged; nothing built or run by the lane). Not landed: the two lines
that would lift it (the `lib.rs` planner's "no `scope_method_key` for `ACC_SYNCHRONIZED`" and the
monitor clause of `deopt::point_needs_unsound_replay_of_method`) are outside this lane's regions,
and lifting them is still unsound without proposal CH4-1 (chain4's reading, which this lane
confirms: a chain of a synchronized callee refused by `helpers::try_resume_trapped_callee` is
re-run from entry, an unsound replay for a body whose fences the chains relaxed).

What the reading adds, for whichever lane takes CH4-1:

* **The service's population is probably empty, and CH4-1 should start by proving it.** A
  WRAPPED synchronized body (the only kind the optimizing tier produces, so the only kind with
  chains) is never entered raw from compiled code: the dispatch cache refuses it
  (`helpers.rs` ~26438, `.filter(|c| !c.requires_wrapped_entry)`), the IC / direct-bind doors bind
  self-locking bodies only (`jit_bridge.rs` `is_self_locking_body`), the caller-held route binds
  only a callee that cannot deopt (`op_invoke.rs` ~2876, "the VM lookup's promise"), and the
  virtual/special bytecode-callee arms run it through the doors' own monitored frame
  (`install_and_run_cached_frame_monitored`), whose stash sink is a door. If
  `CRATONVM_DBG_DEOPT=1` shows no `callee-resume refused (stash method is ACC_SYNCHRONIZED)` line
  on the R13 battery in the chain arm with the planner condition lifted, the service refusal is
  unreachable for chains and CH4-1 reduces to making it restash (never re-run) a synchronized
  chain, so a future route that reaches it fails safe.
* **The hand-over flip does not interact with it.** Self-locking bodies are single-pass, publish
  no chains, and their hand-over is recognised only on a single-scope frame
  (`handed_method_monitor_index` refuses a non-empty `caller_frames`), so
  `CRATONVM_JIT_SELF_LOCK_DEOPT_HANDOVER` changes nothing here, in either direction.
* The doors' side is as wave 9 read it: `resume_depth` is taken before the chain is pushed and
  `push_inlined_chain` pushes the outermost frame first, so `hand_monitor_to_resumed_frame` lands
  the door's monitor on the synchronized method's own frame; and since wave 9 a frame that
  already owns a handed method monitor keeps it (`transfer_to_resumed_frame`).

## Round 14 wave 1 (lane chain)

No change to items 1-4. Two things landed that the flip verdict above depends on:

* CH5-4: `ir_splice_multi_return_enabled` is read per call (`runtime_flag_on`), no process
  `OnceLock`; the flip is `runtime_flag_default_on` in the three readers.
* `CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS` (default ON, inert without chain snapshots):
  `plan_slots` no longer gives every value a chain snapshot names a whole-method frame word; the
  word is held through every node that could publish that chain (a use in `plan_slots`' dataflow)
  and then shared. This is a change to what a chain deopt READS, so the soundness soak of the
  wave-11 verdict must be re-run on a build carrying it: the chain arm with
  `CRATONVM_DEOPT_EAGER_CHAINS=1` over the R13 chain battery plus
  `C:\craton\jitr14-probes\src\R14ChainHashShapes.java`, and the same with the switch at `0` as the
  control. Details on `r13w11-chain5-chain-arm-hashmap-kernel-slower-than-default-20260928.md`.

Status stays OPEN (item 4).

## Round 14 wave 2 (lane deopt)

Item 4 (synchronized compiling methods): **unchanged, not landed.** Re-read: lifting it is two
lines outside this lane's reach or soundness budget -- the `lib.rs` planner's "no
`scope_method_key` for `ACC_SYNCHRONIZED`" (lane-owned elsewhere) and the monitor clause of
`deopt::point_needs_unsound_replay_of_method` (this lane's file, but lifting it ALONE installs
bodies whose fences a chain relaxed while the call-site service still refuses a synchronized
callee's stash and the caller then re-runs it from entry, an `InternalError` under
`CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES`). The precondition is still CH4-1, and its first step
is still the census wave 13 described (`CRATONVM_DBG_DEOPT=1`, the line `callee-resume refused
(stash method is ACC_SYNCHRONIZED)`, R13 battery in the chain arm with the planner condition
lifted in a scratch build). Nothing this wave changes it: R13RP6-1's own-source arm refuses a
synchronized source too (`callsite_trap_own_source`).

The redefinition residual ("Still refused", fifth bullet of the wave-8 argument) is narrower now
(pending build): a chain whose OUTERMOST class alone was redefined since the compile is resumed by
the doors with its outermost frame rebuilt from the body's own template
(`deopt_resume::chain_outermost_own_source`, `CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE`, default ON);
the tier-up sink gets it through `r14w2-deopt-tierup-sink-chain-outer-own-source-patch-FIXED-20260929.md`.
A redefined SPLICED callee's class is still refused everywhere
(`r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`, "Left").

Status stays OPEN (item 4).

## Round 14 wave 3 (lane resume)

Re-read every item against the tree:

* **Items 1-3: hold as closed** (item 1 behind `CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS`, default OFF
  by decision; item 2's parts (a)-(c) in `osr_exit.rs` / `transfer_osr_guard_exit_into_live_frame`
  / the door's lock record; item 3 by the artifact's scope class ids). Nothing to do.
* **Item 2, one gap closed this wave (pending build): an OSR chain guard exit was never charged.**
  `jit_bridge::charge_osr_guard_exit` returned `false` for any chain, so a spliced guard that
  failed on every OSR entry was not even recorded (the flat exit of the same guard is). Proposal
  CH3-4 landed: `deopt_resume::charge_osr_chain_guard_exit` charges it against the OSR'd method at
  the OUTERMOST scope's bci under the stash's own reason (`BoundsCheck` / `ReceiverTypeChanged`
  only, as the flat charge), and after four charges records the wildcard de-spec at that call
  site, as the doors' chain arm does. The exit is NOT reported as a committed entry, so the per-pc
  OSR rejection budget stays its brake: a call-site de-spec does not withdraw the spliced guard
  itself (see `r14w3-resume-chain-trap-despec-never-withdraws-the-spliced-guard-FIXED-20260929.md`).
  Kill switch `CRATONVM_JIT_OSR_CHAIN_GUARD_EXIT_CHARGE` (default ON). Test
  `deopt_resume::r14w3_resume_tests::an_osr_chain_exit_is_charged_at_its_call_site`; probe
  `C:\craton\jitr14-probes\src\R14ResumeOsrChainExit.java` (chain arm, with and without the switch).
* **Item 4 (synchronized compiling methods): unchanged.** The two lines that lift it (the `lib.rs`
  planner's "no `scope_method_key` / `splice_scope_class_ids` for `ACC_SYNCHRONIZED`", ~40271 and
  ~40279, and the monitor clause of `deopt::point_needs_unsound_replay_of_method`) still wait for
  CH4-1. Re-read the fail-safe half CH4-1 asked for: the call-site service
  (`helpers::try_resume_trapped_callee` ~9004) refuses a synchronized callee's stash BEFORE it takes
  it (nothing pushed, nothing restashed), and the declined-stash re-run that follows
  (`handle_compiled_callee_deopt_sentinel` -> `rerun_declined_callee_from_entry`) judges a chain by
  the whole-body rule (`callee_rerun_replay_is_exact` drops a chain's frame to the `(None, ran)`
  arm, `spliced_bodies_side_effect_free`) and raises instead of replaying an unsound one
  (`CRATONVM_JIT_CALLEE_UNSOUND_RERUN_RAISES`). So a future route that reached it with a chain would
  fail loud, not wrong; the census (wave 13's first step) is still what decides the lift. This
  wave's R14DP-1 takes no template for a synchronized method (`callsite_trap_current_template`
  refuses `is_synchronized`), so the hand-over question stays on the named path.

Status stays OPEN (item 4).

## Round 14 wave 3 (lane chain)

* **Item 4: unchanged, not landed.** Re-read the three `!cached.is_synchronized` conditions in the
  IR planner (`ir_chain_fences`, the `splice_scope_class_ids` / new `splice_scope_sources` block,
  `scope_method_key`) and the monitor clause of `deopt::point_needs_unsound_replay_with_handlers`
  (`monitor == SinkMethodMonitor::None && inlined_chain_refusal(fs).is_none()` for a chain). Lifting
  the planner half alone stays sound but is a slowdown for relaxed compiles (the post-lowering
  replay check discards them); lifting both still waits on CH4-1's census. To make that census a
  flag rather than a scratch build: proposal CH3W-4 of `jit-r14-chain3-proposals.md` (a
  default-OFF `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS` at those four sites).
* **Redefinition residual narrowed again (pending build):** the doors now resume a chain whose
  SPLICED callee's class was redefined since the compile, rebuilding that scope from the bytecode
  the compile spliced (R14DP-5; `r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`,
  wave-3 lane-chain section). The other chain sinks still refuse it.
* **Chain charges no longer retire the outer method (pending build):** a call site the chain
  charges withdrew with the wildcard is no longer spliced again
  (`r14w3-resume-chain-trap-despec-never-withdraws-the-spliced-guard-FIXED-20260929.md`, FIXED pending),
  which the item-2 OSR charge (CH3-4) relied on.

Status stays OPEN (item 4).

## Round 14 wave 4 (lane resume2)

* **Item 4: the census switch (CH3W-4), VM/producer half landed (pending build).**
  `jit/src/deopt.rs` `sync_method_chains_enabled()` reads `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS`
  (default OFF, per call, no static), and the monitor clause of
  `point_needs_unsound_replay_with_handlers` is now `(monitor == SinkMethodMonitor::None ||
  sync_method_chains_enabled()) && inlined_chain_refusal(fs).is_none()`. Off: unchanged. The three
  planner conditions in `jit/src/lib.rs` are not this lane's: exact patch
  `r14w4-resume2-lib-sync-method-chains-patch-FIXED-20260929.md`. Until it is applied the switch changes
  nothing (no synchronized compile publishes a chain point). Test:
  `deopt::r14w4_resume2_sync_method_chain_tests::a_synchronized_methods_chain_is_resumed_only_under_the_census_switch`.
* **Then decide** (orchestrator, needs runs): with the lib.rs patch applied, run the R13 battery and
  `C:\craton\jitr14-probes\src\R14Resume2SyncChain.java` in the chain arm
  (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES=1 CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES=1
  CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1`) with `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS=1
  CRATONVM_DBG_DEOPT=1`, and count `callee-resume refused (stash method is ACC_SYNCHRONIZED)`.
  Zero, and matching output: the lift is safe to make default with CH4-1's fail-safe half (the
  service already refuses before taking the stash). Any hit: CH4-1's service half first.
* The redefinition residual of the wave-8 argument ("Still refused", fifth bullet) is closed for
  every chain sink (`r13w5-resume-chain-inner-scope-of-a-redefined-class-FIXED-20260929.md`, wave-4
  section).

Status stays OPEN (item 4: the census).
