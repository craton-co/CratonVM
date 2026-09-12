# FIXED: OSR-exit resume tolerated `Unsupported` locals without per-bci liveness

**Status: FIXED 2026-09-12.** Found by the 2026-09-12 JIT review as finding #88.
It was latent, with no reproducer: a gap in the soundness argument for
leaving an `Unsupported` local at the live frame's value on an OSR exit.

## The gap

`OsrEntryPlan::resume_after_exit` (`jit/src/lib.rs`) and
`transfer_osr_exit_into_live_frame` / `transfer_osr_exception_exit_into_live_frame`
(`vm/src/runtime/interpreter/deopt_resume.rs`) do not refuse a reconstructed
frame with an `Unsupported` local. They leave that slot at the interpreter's
current value.

The documented justification was a whole-method argument. `classify_local_kinds`
marks a slot reused as two kinds `Ambiguous` at every bci, and verified bytecode
stores a logical local before loading it. That argument does not cover a slot
that is live at the exit bci in one of its kind regions. There the compiled
code may have changed the value, and the interpreter's copy is stale.

Refusing at exit time is not the answer either. It replays the exit's committed
side effects from pre-entry state (`jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`).

## The fix: decide it at compile time, where the point is published

The single-pass backend publishes every deopt / OSR-exit point through
`x64::deopt_stubs::build_frame_state_at`. For each local:

1. **Liveness first.** `regalloc::live_locals_per_pc_all` computes live-in
   locals per pc. It is the handler-aware, more-than-64-locals form of
   `live_locals_per_pc_with_handlers`, and window 0 is bit-identical to it. It
   runs once per compile in `x64/driver.rs` under the same gate as the kind
   table. A local that is not live-in at a covered bci is published `Undefined`,
   whatever its whole-method kind.
2. **A live slot is described precisely where it can be.** When the
   whole-method kind is `Ambiguous`, the per-bci reaching-kind dataflow
   (`refine_ambiguous_local_kinds`) supplies the kind at that bci. That is a
   concrete primitive kind, or `Ref` where the oop mask has no opinion. The
   value is then read from the slot's machine home. A slot the dataflow settles
   as `Ambiguous` (JVMS `top`, unreadable) in an exact CFG is published
   `Undefined`.
3. **Otherwise the artifact is non-resumable at compile time.** A live local
   that still cannot be described is published `Unsupported`. `value_blocks_resume`
   treats it as unresumable, and `CompiledMethod::osr_exit_policy` walks every
   deopt point. So `validate_osr_entry` refuses the OSR entry
   (`osr-entry-unresumable-exit`) before the body runs, and nothing is committed
   that would need a refusal later.

Together, these mean an admitted artifact never carries a live `Unsupported`
local. The only remaining producer at exit time is `deopt::resolve_value`, which
maps a metadata defect (a register index outside the spilled file) to
`Unsupported`. That is not a classification gap, and `DeoptVerifier` checks it
at compile time.

The exit-time code keeps its tolerance, as the review required. It never
refuses after side effects. What changed is the documented argument:
`resume_after_exit`'s doc and loop comment, and the comments in both VM
transfers, now state the compile-time guarantee above and cite this record.
There is no behaviour change at exit time.

This work was already in `build_frame_state_at` and `osr_exit_policy` when the
record was written (the liveness-first drop, the per-bci refinement, the
settled-`top` relaxation, and the artifact-wide admission veto). What was
missing was a test that pins the compile-time contract on a slot reused as two
kinds, and a soundness argument that names it. This change adds both.

## Regression coverage

`jit/src/x64/tests.rs`:

- `osr_exit_snapshot_describes_a_reused_slot_that_is_live_at_the_exit`: slot 1
  holds an `int`, then a reference, and the loop header reads the reference.
  The whole-method kind is `Ambiguous`. At the OSR-exit point the slot must
  meet two conditions:
  - it is published as a reference home, or else as `Unsupported`, and then
    `osr_exit_policy` must refuse;
  - it is never published `Undefined`.

  No snapshot in an admitted artifact carries an `Unsupported` local.
- `osr_exit_snapshot_publishes_a_reused_slot_that_is_dead_at_the_exit_as_undefined`:
  the same reuse, with the slot dead at the loop header. It is published
  `Undefined`, and `first_unresumable_local` finds nothing at that point.

Existing tests that keep passing with no logic change:

- `jit/src/lib.rs` `an_undescribable_exit_refuses_to_name_a_resume_point`.
- `vm/src/runtime/interpreter/deopt_resume.rs`
  `osr_exit_transfer_tolerates_unmappable_local` and
  `unsupported_is_tolerated_where_materialization_required_refuses`. Their
  fixtures hand the transfer a reconstructed frame directly, with a hand-built
  plan whose deopt point describes no locals. The `Unsupported` slot is neither
  live nor dead in any bytecode sense, so they pin the no-refusal rule, not a
  liveness verdict. Only their doc comments were updated.
