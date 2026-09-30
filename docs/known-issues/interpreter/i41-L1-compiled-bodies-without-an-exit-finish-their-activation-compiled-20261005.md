# Compiled bodies without an exit finish their activation compiled after an agent or a debugger needs the interpreter

**Status: open — filed 2026-10-05 by interpreter round i1 wave 41, lane L1,
for lanes L2 / L3 (the `jit/` crate: codegen, the exit admissions, the
deopt runtime). It carries the JIT remainder of
`docs/internal/fixed-bugs/interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md`
and of
`docs/internal/fixed-bugs/interpreter-L1-jdwp-suspension-does-not-reach-compiled-or-native-code-FIXED-20261005.md`
(item 1), whose debugger- and agent-side work is done. Nothing below is
specific to JDWP or JVMTI: each is a body shape that has no exit at the point
where the VM asks it to leave.** Wave 43 (lane L2): an activation that
finishes compiled across a renumbering redefinition read its constants from
the new pool at old indices under load; fixed (see "Progress (wave 43)");
the shapes below are unchanged. Wave 46 (lane L2): an own-class METHOD-ENTRY
body of a renumbered class leaves too, which narrows shape 1 (b) (see
"Progress (wave 46)").

## Progress (wave 46) — lane L2: an own-class method-entry body of a renumbered class leaves

`docs/internal/fixed-bugs/interpreter-L2-proposal-method-entry-obsolete-bodies-leave-through-the-rebuild-sinks-FIXED-20261010.md`
is built (behind `jit/src/not_entrant.rs::OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`,
on in the lane's last commit). After a redefinition that renumbered the
class's constant pool, a METHOD-ENTRY body of the class's own old bytecode,
of either tier, is forced and told to leave when a stash sink can rebuild its
frame in the bytecode it was compiled from (a compiled source and a pool
stamp, not `ACC_SYNCHRONIZED`, no monitor in its code) and that frame
translates onto the current pool; the interpreter doors' sink and the
compiled callers' call-site service (`helpers::try_resume_trapped_callee`)
now resume it there, restamped, instead of refusing a frame of a redefined
class. Probe `tools/probes/interp/L2/L2W46OwnClassEntryBodyLeavesAtRenumbering.java`.

What this closes here:

* the "Still spared: a METHOD-ENTRY body of the class" line of "Progress
  (wave 45)", for the bodies named above;
* **shape 1 (b)** ("a body compiled BEFORE its class was redefined"),
  narrowed to: a body whose class's redefinition moved no constant (nothing
  to translate, so it is still spared and finishes compiled, HotSpot's shape
  for an obsolete activation); a synchronized or locking method, or one
  whose publish site stamped no compiled source; and the interpreter-only
  requests of an agent or a debugger, whose verdict
  (`jvmti_events::polling_body_must_leave`) still refuses a body compiled
  before its class's last redefinition -- the sinks could now resume it, but
  the verdict and the grant do not ask yet
  (`i46-L2-proposal-interpreter-only-requests-let-obsolete-bodies-leave-through-their-source-20261010.md`).

Shapes 1 (a), (d), (e), 2, 3 and 4 are unchanged.

## Progress (wave 45) — lane L2: an own-class OSR body of a renumbered class leaves

The last bullet of "Progress (wave 44)" below ("a body compiled from the
redefined class's own bytecode is still spared") is narrowed. After a
redefinition that renumbered the class's constant pool, an OSR body of the
class's own old bytecode, of either tier, is forced like any withdrawn body
and told to leave (`jit/src/not_entrant.rs::OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE`,
`CompiledMethod::leaves_as_renumbered_obsolete`); the OSR door transfers the
exit into the obsolete activation's own interpreter frame, which runs the
rest of its old bytecode interpreted against the old constants
(`i44-L2-proposal-an-own-class-compiled-activation-leaves-at-its-constant-pool-sites-20261008.md`,
"Progress (wave 45)"; probe `tools/probes/interp/L2/L2W45OwnClassLoopLeavesAtRenumbering.java`).
Still spared: a METHOD-ENTRY body of the class
(`docs/internal/fixed-bugs/interpreter-L2-proposal-method-entry-obsolete-bodies-leave-through-the-rebuild-sinks-FIXED-20261010.md`),
and any own-class body after a redefinition that moved no constant (nothing
to translate). Shapes 1-4 below are unchanged: this is about bodies that
HAVE exits and were spared, not about bodies without one.

## Progress (wave 44) — lane L2: exits at the constant-pool helper calls

`docs/known-issues/interpreter/i43-L2-proposal-exits-at-a-withdrawn-bodys-constant-pool-helper-calls-20261007.md`
("Progress (wave 44)") gives a single-pass body a forceable exit BEFORE each
run-time constant-pool helper call (`ldc` String / Class, a deferred `new` /
`anewarray`, `multianewarray`), leaving at the instruction itself, behind
`op_invoke.rs::CP_HELPER_EXITS_ENABLED`. What that changes here, per shape:

* **Shape 3** (back edges the single-pass admission refuses) and every other
  loop whose poll cannot leave: a withdrawn body of an OSR-tier compile now
  leaves at the first such constant in the loop body it reaches, when that
  instruction's own map is admissible (`branch_mode_exit_target` at its bci:
  not in a splice, no loop rewrite, a transferable frame, no elided monitor
  or self-lock, not a branch target). A loop that names no such constant is
  unchanged.
* **Shape 1** (single-pass method-entry bodies): the same, for a body that
  already filed a loop-boundary map or keeps a dispatch record (the post-call
  sites' tier rule). A loop-free body with neither (1 (a)'s and 1 (b)'s
  simplest members) gets no site: its first deopt exit is kept off it for the
  synchronized routes (`METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`'s
  doc). 1 (d) (the GC-inert self-tail form) calls no helper; 1 (e) (a loop in
  a splice) is refused as every splice site is.
* **Shape 2** (the optimizing tier) and **shape 4** (a suspension's pause
  timing; a suspension withdraws nothing, so nothing is forced): unchanged.
* **A body compiled from the redefined class's own bytecode** is still spared
  (`JitCache::force_withdrawn_exit_polls`): its activation finishes on its
  old bytecode, as HotSpot's does (JEP 109), and `CpSite` translates its
  constants; the sites leave only for bodies a redefinition of ANOTHER class
  or a debugger request withdrew.

Probe `tools/probes/interp/L2/L2W44CpHelperExitAfterTheCall.java`; unit
test `x64::tests::a_forced_cp_helper_exit_leaves_before_the_helper_runs`.
The page's own probes (`L1W37JdiCompiledLoopMethodEvents`,
`L1W39JdiStepSessionOverCompiledLoop`) move only if their loops read such a
constant.

## Progress (wave 43) — lane L2: what such an activation reads until it finishes

The orchestrator's wave-43 host run tied this page to a wrong answer
(base `cvm-w42f`, eight busy loops, 60 runs per cell): `L3W37HotSwapSpinningReads`
5/60 and `L3W41FirstSwapHoldsTheLoop` 4/60 wrong with the JIT, 0/60 `--nojit`,
one failure printing `JIT ldc: cp#19 of class id 646: class
L3W41FirstSwapHoldsTheLoop$Tgt holds Utf8("length") at that index, not a
StringReference` from a compiled `Tgt.spin`. A compiled body that keeps
running after its class's redefinition is by design here (HotSpot's old
compiled activation runs its old method too); what it must not do is read
the NEW pool at its OLD indices. Its constant-pool helper sites translate
their index from the stamp they were compiled at, but judged the need to
translate before, and without the guard of, the pool read, so a redefinition
landing inside one helper call left the index untranslated. Wave 43 judges
it under the guard the pool is read under, or confirms a lock-free judgement
after a record probe (`vm/src/jit/helpers.rs::CpSite`); the record is
`docs/internal/fixed-bugs/interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md`.
The `multianewarray` and type-check sites that still judge lock-free are
`docs/internal/fixed-bugs/interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008.md`.

No shape below gained an exit this wave: each is an emitted-code change the
lane left for a wave that can build and run the jit crate's tests.

## What the VM side does now (for reference)

Every request an agent or a debugger can arm on this VM either withdraws the
compiled bodies it needs or needs none:

* JVMTI `MethodEntry` / `MethodExit` / `SingleStep` / `FramePop`, and JDWP
  step, method event and field watch requests: every body is withdrawn, made
  not entrant, and has its exit polls and post-call exits forced
  (`jvmti_events::note_every_method_needs_the_interpreter`,
  `JitRealm::withdraw_every_body_for_the_interpreter`; wave 37-38);
* a JDWP or JVMTI breakpoint: its class's compiled dependents, or every body
  while it sits in a JDK class (`jvmti_events::note_breakpoint_classes_gained`,
  `debug::WITHDRAWAL_BY_JDK_BREAKPOINT`; waves 38 and 40);
* an `Exception` request: none (the compiled catch doors post the event,
  wave 21); a JVMTI field watch cannot be armed (the C table offers no
  `SetField*Watch`);
* a JDWP suspension alone: none, by design (a `SUSPEND_ALL` breakpoint
  suspends and resumes at every stop, and a withdrawal per stop would flush
  and recompile the program each time). It keeps the loop-exit pause of
  waves 12-18 (`interpreter::request_compiled_loop_exits`), a handshake with
  a grace slice, repeated at most three times while it froze a peer.

So what still runs compiled is a frame whose body cannot leave where it is
asked to. Each such frame finishes its current activation compiled; its
callees run interpreted.

## The shapes (all in `jit/`)

Carried over from the wave-40 list of the i9-L5 page (its "What remains
(after wave 23)" section has the full history and the functions named):

1. **Single-pass method-entry bodies:**
   (a) a body with no compile id (precise maps or the inline TLS mirror off,
   or every id live; `[jit-compile-id]` says so), which answers only the
   VM-wide question and gets no exit once any class of the process was
   redefined;
   (b) a body compiled BEFORE its class was redefined (its frame would run
   bytecode the class no longer has);
   (d) the GC-inert self-tail form;
   (e) a loop inside a splice (the admission refuses it).
2. **Optimizing (IR) tier:**
   (b) a back edge whose header state names a value with no location at the
   poll, a φ of another merge, a lock whose object has no frame word, a
   scalar-replaced object or a spliced header
   (`Lowerer::back_edge_mode_exit_state` refuses), and a compile whose
   unroller relied on a trap-free body;
   (c) a method-entry body of an `ACC_SYNCHRONIZED` method whose graph can
   neither trap nor call (`ir_lower::graph_cannot_deopt`); see
   `i22-L2-proposal-caller-held-site-resumes-a-callee-that-left-20260926.md`;
   (d) the poll-mode-exit census (`ir_poll_mode_exit_census`,
   `ir_entry_poll_mode_exit_census`) is still unmeasured on a real workload.
   ((a) was closed in wave 40 as unreachable in a shipped layout; (e) is a
   single-pass fallback, item 1's rules.)
3. **Back edges the single-pass admission refuses**
   (`x64/safepoint.rs::mode_exit_target`, `branch_mode_exit_target`): loops
   inside an inlined callee, loop-rewritten bodies (`bci_provenance`), a
   conditional back edge with anything on the stack below its operands, an
   `invokedynamic` loop header, a branch bci that already carries another
   deopt point, and a branch frame the in-place transfer would not take.
4. **A suspension's pause timing** (from the i9-L1 page's item 1): a body
   whose thread was descheduled between the OSR door's re-ask and the entry,
   or a peer frozen by all three pauses, leaves only at the next pause of
   any kind. What would close this and every shape above for a suspension is
   a park inside the compiled poll's slow path (HotSpot suspends a thread in
   compiled code by a handshake without deoptimizing it): the wave-11/12
   "stage 2" of the i9-L1 page ("Progress (wave 12)"), whose three blockers
   are unchanged: the polls take their slow path only during a pause, which
   is requested only while an OSR body is counted in
   (`jit_bridge::osr_bodies_running`), so every suspension would need a
   pause unless running compiled activations are counted; a thread parked
   in a compiled activation must refuse debugger invocations (a "compiled"
   level of `park_for_debugger` that `DebugState::can_take_work` answers
   `false` for invocations); and an OSR frame's locals live in the body's
   slots, so the published frame must come from the compiled-frame oracle
   or omit them (`OPAQUE_FRAME` does the latter since wave 21).

## Evidence

The wave-40 re-read of the i9-L5 page (lane L1) found no debugger- or
agent-side item left: every request withdraws what it needs (above), and each
remaining line is a codegen or admission decision. The probes that would move
when a shape gains an exit: `tools/probes/interp/L1/L1W37JdiCompiledLoopMethodEvents.java`
(a compiled loop entered before the request), `L1W39JdiStepSessionOverCompiledLoop`
and `tools/probes/interp/L5/L2W18IrEntryLoopShapes.java` (the census line).

## What would fix it

Per shape, an exit at the point the VM asks (a mode-exit map at the back
edge, a post-call exit, or a deopt state) — each is the owning lane's; or,
for a suspension, the compiled-poll park of item 4.
