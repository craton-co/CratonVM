# Proposal: keep assigned locals alive in compiled code while a debugger may read them

**Status: open (proposal) — filed 2026-09-25 by interpreter round i1 wave 19, lane L1.**

## Why

Wave 19 made the debugger's view of a mode exit exact by refusing the exit
(`docs/internal/fixed-bugs/interpreter-resumed-frames-show-dead-locals-as-zero-FIXED-20260925.md`):
under a JDWP agent, a compiled loop whose header has a dead but assigned local
(`int t = 42;` before the loop, never read after) no longer leaves for a
breakpoint set in it while it runs, so that breakpoint is missed for the rest
of the activation — the pre-wave-12 behaviour for those loops. Other deopts
cannot be refused and still show `0`
(`docs/internal/fixed-bugs/interpreter-L2-deopt-resumed-frames-show-dead-locals-as-zero-to-a-debugger-FIXED-20260926.md`).

HotSpot does neither: with `can_access_local_variables`,
`ciMethod::liveness_at_bci` answers "every local live", so C1 and C2 keep
each local's value until it is overwritten and every deopt point describes it.
The cost is register pressure in bodies compiled under a debugger only.

## Design

1. **The fact** exists: `runtime::jvmti::debugger_observes_locals(shared)` →
   `CompileRequest::debugger_observes_locals` → `BackendRequest::
   debugger_observes_locals` (wave 19).
2. **Single-pass tier.** Under the flag, widen every liveness answer the
   backend consumes by the assigned set: `live'(pc) = live(pc) |
   assigned(pc)` (`regalloc::assigned_locals_per_pc_all`, computed under the
   flag since wave 19, same layout). It must feed ALL consumers together, or
   the snapshot will describe a register the allocator reused:
   * register allocation's interference (`build_interference` /
     `color_graph` inputs, `allocate_registers_with_handlers`), so a dead
     assigned local keeps its register or slot;
   * `Compiler::local_liveness` (the snapshot's `Undefined` decision);
   * any dead-store elimination or spill pruning keyed on liveness (audit
     `x64/` for `local_live_at` / `local_liveness_word` readers);
   * the GC maps: a dead reference local that is now published must also be
     in every safepoint's oop map, or a moving collection leaves it stale —
     check `local_oop_mask_at_current_pc` and the precise-map builders for
     liveness pruning.
   Then `exit_hides_an_assigned_local` answers `false` everywhere and can be
   deleted with its three call sites.
3. **Optimizing tier.** Under the flag, the IR builder's frame states must
   keep a node for every assigned local and the lowerer must keep its home
   (`compute_deopt_named_reachable` already does this for loop headers under
   `ir_register_authoritative_enabled`). Equivalent to C2's
   `should_retain_local_variables`.
4. **Narrow what remains to what a debugger can name** (independent, and
   useful even before 2–3): a debugger names a local only through the
   method's `LocalVariableTable` (JDI's `getArgumentValues` also reads the
   parameter slots without one). Hand the compile the LVT scopes (`(start,
   end, slot)` rows, VM side from the method's `Code` attributes, only under
   the flag) and have `exit_hides_an_assigned_local` refuse only for a slot in
   scope at the exit bci or a parameter. JDK classes carry no LVT, so their
   loops would never refuse; user loops refuse only for a genuinely visible
   variable.

## Staged plan

* Stage A (small, VM + jit plumbing): step 4. Measure with a JDWP-attached
  run of a loop-heavy test how many mode exits the wave-19 refusal withholds
  (count the refusals in `Compiler::exit_hides_an_assigned_local` behind a
  `CRATONVM_DBG_*` gate) before and after.
* Stage B (jit single-pass): step 2, with the regalloc/oop-map audit; unit
  tests in `x64/tests.rs` beside
  `a_mode_exit_that_would_hide_an_assigned_local_is_refused_under_a_debugger`
  asserting the exit now LEAVES with `t = 42` in the stashed frame, plus a
  moving-GC test with a dead reference local.
* Stage C (jit IR): step 3.

## Expected benefit

Breakpoints and steps inside already-compiled loops work under a debugger for
every loop shape the mode exits admit, with HotSpot's local values in every
resumed frame. No effect on runs without a JDWP agent.

## Progress (wave 21)

Interpreter round i1 wave 21, lane L3, 2026-09-25. **Stage B landed** for the
single-pass tier; stage A re-assessed and deferred (below); stage C (the IR
tier) is next.

What landed (all only while `BackendRequest::debugger_observes_locals`, i.e.
under a JDWP agent; every other compile takes the old path byte for byte):

* **One table for the allocation and the snapshot.** `x64/driver.rs`
  `compile_with_param_slots` computes `regalloc::assigned_locals_per_pc_all`
  once, with the method's FULL exception table, BEFORE register allocation,
  and hands the same rows to the allocator and (as `Compiler::local_assigned`)
  to the snapshot builder. The snapshot never describes a home the allocation
  did not keep.
* **Register allocation keeps every assigned local's home.**
  `regalloc::allocate_registers_keeping_assigned` (a thin door over the new
  `allocate_registers_inner`; `allocate_registers_with` passes `None`) widens
  the interference graph (`keep_assigned_interference`): the locals assigned
  at a pc pairwise, and every store against every OTHER local assigned on
  entry to it. The doc proves the second edge is sufficient: a local
  assigned at `pc` is assigned at every store between its last store and
  `pc`, for the computed table too (its imprecision only removes bits, and a
  missing bit stays missing along the continuation). The XMM pass reads the
  same graph. `block_live_in` is widened by the assigned row at each block
  start, so the OSR trampoline seeds a kept local and masks a dead local
  coalesced with it (`x64/osr.rs` `osr_dead_mask`) instead of clobbering it.
* **The snapshot describes a dead but assigned local from its home.**
  `x64/deopt_stubs.rs` `build_frame_state_at`: a local liveness drops at the
  bci but `local_assigned` keeps (`local_assigned_word`) is described like a
  live one, and the description is kept only when exact
  (`kept_dead_value_is_exact`): a reference the must-oop mask proves
  (`RegisterRef` / `StackSlotRef`), or a primitive typed by the kind table.
  Anything else (a reference only the kind scan claims, `Unsupported`, a
  width-blind provenance, the missing-metadata scalar-replacement fallback)
  stays `Undefined`, the answer before; a kept local therefore never makes a
  frame unresumable.
* **The refusal now asks the published frame.** `x64/safepoint.rs`
  `exit_hides_an_assigned_local(bci)` is replaced by
  `frame_hides_an_assigned_local(bci, &fs)` (an assigned local published
  `Undefined` in the frame the exit would resume) and
  `recorded_exit_hides_an_assigned_local(pc)` for maps already recorded; the
  three admissions (`mode_exit_target`, `branch_mode_exit_target`,
  `self_tail_mode_exit_target`) ask it after the frame is built. What still
  refuses is a slot no home describes exactly — in practice a primitive
  parameter the method never reads (kind `Unknown`: the single-pass backend
  has no descriptor to type it).

The GC audit the design asked for, answered from the code: nothing needed to
change. The oop maps and the shadow publication are driven by the FORWARD
must-oop analysis (`x64/licm.rs` `compute_local_oop_masks_windowed`,
`for_each_oop_local_at_current_pc`), which liveness never prunes, and a
register-homed reference local is published at every safepoint regardless of
liveness (`emit_pre_safepoint_spill_impl` uses
`register_homed_reference_locals`, not `publish_at`) and reloaded from
`local_oop_masks`. A kept reference is described only when that mask proves
it, i.e. only when the maps keep its home current across a moving
collection. No emitter consumer keys a store or spill on liveness (the only
readers of `local_liveness` are the snapshot and the admissions).

Tests: `x64::tests::a_mode_exit_under_a_debugger_describes_a_dead_assigned_local`
(renamed from `a_mode_exit_that_would_hide_an_assigned_local_is_refused_under_a_debugger`:
the observed body now LEAVES with `t = 42` in the stashed frame; an unread
`int` parameter still refuses), `regalloc::assigned_locals_tests::a_store_interferes_with_every_local_assigned_across_it`,
`regalloc::assigned_locals_tests::the_keep_alive_allocation_keeps_a_dead_assigned_local_home`
(no shared home, the header's block live-in names the kept local, and an
all-zero table allocates exactly as without one). Probe (by hand, needs a
JDWP agent): `tools/probes/interp/L1/L3W21DeadLocalUnderDebugger.java`.

Not measured: register pressure of debugger compiles. It is a debug-session
cost only, bounded by the widened graph (a kept local may be colourless and
live in its frame slot, which is always correct).

**Stage A (LVT narrowing), deferred on purpose.** With stage B the refusal
fires only for an assigned slot that no home describes exactly. Its common
case is an unread primitive PARAMETER, which a debugger can always name (JDI
`getArgumentValues` reads parameter slots without a `LocalVariableTable`), so
narrowing by LVT scope would not admit it. The rest (a reused slot whose kind
the per-bci scan cannot settle, a reference only the kind scan claims) is
rare, and needs VM-side plumbing through three compile doors
(`jit_bridge.rs` twice, `interpreter.rs` once) plus a `CompileRequest` /
`BackendRequest` field. Worth doing only with a measurement: count
`frame_hides_an_assigned_local` refusals on a JDWP-attached run of a
loop-heavy workload (a `jit::metrics` counter rather than a new env gate).
A cheaper closure of the parameter case: hand the backend the method
descriptor's parameter kinds (the VM has them at every door) so an unread
`int` / `long` / `float` / `double` parameter is typed and kept like any
other local — done in wave 22 (lane L1):
`docs/internal/fixed-bugs/interpreter-L1-an-unread-primitive-parameter-has-no-kind-in-single-pass-snapshots-FIXED-20260926.md`.

**Next: stage C (optimizing tier).** Under the flag, the IR builder's frame
states must keep a node for every assigned local and the lowerer its home
(`compute_deopt_named_reachable` already does it for loop headers under
`ir_register_authoritative_enabled`); then `PollExitBytecode` can be handed to
debugger compiles again (`jit/src/lib.rs`, the `!req.debugger_observes_locals`
conjunct). This proposal stays open for the user to triage.

## Progress (wave 22) — lane L2 (the optimizing tier's correctness half)

Interpreter round i1 wave 22, lane L2, 2026-09-26. Stage C's CORRECTNESS is
landed as a check rather than as a keep-alive; its SPEED half is not.

* **What the tier already does.** The IR builder names the node of every
  local in every snapshot (it drops none by liveness, except the opaque OSR
  merge under `CRATONVM_JIT_OSR_OPAQUE_ENTRY`), and the lowerer keeps the home
  of every value a reachable snapshot names. So a dead but assigned local is
  normally described from its home at every IR guard and exception point;
  the tier's `Undefined` comes from a snapshot slot an optimization cleared
  (`NO_NODE`), a merge of two kinds, or a mode exit's dead-local rule, which
  `lib.rs` already withholds from a debugger's compile.
* **What landed.** `ir_lower::ir_artifact_hides_an_assigned_local` asks every
  point of an optimizing artifact whether its method's own scope publishes an
  assigned local (`regalloc::assigned_locals_per_pc_all`) `Undefined`; under
  `debugger_observes_locals` the IR door (`lib.rs` `ir_tier`) discards such an
  artifact and the single-pass body (stage B) runs. Exact, so the page
  `docs/internal/fixed-bugs/interpreter-L2-deopt-resumed-frames-show-dead-locals-as-zero-to-a-debugger-FIXED-20260926.md`
  is closed. Tests: `ir_lower::i22_l2_synchronized_and_debugger_tests`.
* **What stage C still offers.** Keeping the IR artifact instead of
  discarding it: under the flag, keep every assigned local's snapshot node
  through the optimizer (no `NO_NODE` for an assigned slot) and its home
  through lowering; then hand `PollExitBytecode` back to debugger compiles.
  Measure first: on a JDWP-attached run of a loop-heavy workload,
  `CRATONVM_DBG_JITC=1` names every discard (`a resumable frame would show an
  assigned local as 0 to the debugger -- keeping the single-pass body`); if
  that count is small, stage C is not worth its register pressure.
