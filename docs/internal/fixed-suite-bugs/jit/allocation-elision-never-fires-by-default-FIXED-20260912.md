# FIXED: allocation elision never fired in a default run

**Status: FIXED 2026-09-12.** Found by the 2026-09-12 JIT review (#49). A
non-escaping allocation that no consultable deopt point needs is now removed in a
default run. One that a real deopt point does name is removed with a
materialization recipe whenever precise resume is on (the default).

## The defect

Scalar replacement forwarded field loads, but the `Op::New` itself and its
`putfield`s stayed in the emitted code. There were two causes.

1. **Every allocation was snapshot-named.** `IrBuilder` records a snapshot at
   every bytecode boundary, and the fresh reference sits on the stack or in a
   local at several of them. `plan_scalar_replacement` kept any snapshot-named
   allocation unless a `FrameValue::VirtualObject` recipe would exist. The recipe
   was gated on `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL` through
   `sr_map`, and `CRATONVM_SCALAR_DEOPT` was off by default.
2. **`o != null` escaped the object.** The null literal made the bridge map the
   compare to `EaOp::Other` (a global escape), and the live compare was a value
   use the planner refuses to strand.

## The fix

Everything is in `try_compile_inner` (`lib.rs`) unless stated otherwise.

### Dead snapshot locals are cut

`ir_prune_dead_snapshot_locals` runs right after the build, before any pass
roots snapshots.

A standard backward local-variable liveness pass over the method's own bytecode
clears every snapshot local that is not live-in at that bci. Nothing reads such
a local before writing it, so resuming with `Undefined` (which the sinks turn
into `Int(0)`) is exactly as correct as resuming with the old value. This is the
cut HotSpot's `MethodLiveness` makes.

It does not run, or changes nothing, in these cases:

* a method with an exception table (the caller checks);
* a loop header, meaning any backward-branch target, because an OSR entry seeds
  the compiled frame from that snapshot;
* subroutines, `wide ret`, a malformed switch, or a local index past the
  snapshot width;
* a fixed point that has not converged in 64 rounds.

### Only consultable snapshots count

`ir_consumable_snapshot_bcis` computes the bcis where a deopt can arrive:

* the resume bci of every live node with `!op_cannot_deopt`, its `bytecode_pc`
  mapped through the spliced ranges, plus a `Guard`'s own `bci`. Every deopt
  stub and exceptional exit the lowerer emits comes from such a node, at that
  bci;
* every `Merge`/`Region` bci;
* the snapshots claimed through `Node::frame_snapshot`.

A node that can deopt with no `bytecode_pc` makes the graph unattributable, and
then every snapshot counts, as before. Without the spliced ranges, so does a
resume pc no snapshot describes.

`plan_scalar_replacement` asks `ea_consumable_snapshot_names`, which treats the
candidate's own allocation, stores and loads as already gone. After the escape
analysis rounds, `ir_prune_unconsumable_snapshots` drops every other snapshot via
the new `Graph::retain_safepoints`, which renumbers `frame_snapshot` and rebuilds
the use lists. So no deopt point is built from a snapshot that names a removed
allocation.

The planner's set is never narrower than what the prune keeps. The prune sees the
same graph minus every plan's victims, with exact splice mapping.

### The recipe is on whenever precise resume is

`scalar_deopt_descriptor_available()` is `deopt_real_enabled()`, default on. It
replaces `scalar_deopt_enabled() && deopt_real_enabled()` in
`apply_ea_to_ir_pinned`, `build_scalar_replacement_map`, and the `sr_map` capture.

**The exact rule.** An allocation named by a consultable snapshot is elided only
if `scalar_deopt_descriptor_available()` holds and `virtual_object_info_for`
describes it. Otherwise it keeps its allocation. With precise resume off, no
elision ever relies on the whole-method re-run.

`CRATONVM_SCALAR_DEOPT` is superseded but still declared, and still read by
`scalar_deopt_descriptor_available`. Its doc comment says so.

### The recipe describes the common shapes (`ir_lower.rs`)

* `deopt_sites_by_bci` now indexes every transferring node under its resume bci.
  It used to index only guards and divisions, so an object live at a call, a
  field access or an allocation had no deopt block and was always
  `MaterializationRequired`.
* `eliminated_node_precedes_deopt` accepts the allocation or a store in the SAME
  block as the deopt when its node id is below the first deopt site's there
  (`deopt_first_site_in_block`). Control-pinned nodes of one block are created in
  walk order. `VirtualObjectInfo::store_nodes` carries the store ids. Same-block
  used to refuse outright, which is the straight-line shape
  `o = new; o.f = x; ...; guard`.

### Null checks on a fresh allocation fold

`ir_fold_null_checks_on_fresh_allocations` runs before the optimizer. It folds
`Cmp(Eq|Ne)` of an `Op::New`/`Op::NewArray` against the `Ref`-typed null literal
to `Const(0|1)`. That is sound because an allocation never yields null; it throws
first.

### Backstop

The lowerer can still refuse a recipe at some point, for example an ambiguous
deopt block. That point would be unresumable, and its sink re-runs the method. In
that case `ir_artifact_names_an_undescribable_elided_object` plus
`bytecode_commits_side_effect` discard the IR artifact for a side-effecting body.
The single-pass body keeps the allocation.

## Known costs, deliberately accepted

* A resumed or JVMTI-inspected frame shows `Int(0)` for a local that is dead by
  liveness. This matches HotSpot's C2.
* The backstop can discard an IR artifact that used to be kept, in exchange for
  never duplicating a side effect. Its frequency has not been measured.
* The prune removes `ir_osr_entries` at non-join block starts (`Proj` blocks).
  The VM only enters at loop headers, which are joins.

## Regression coverage

* `lib.rs`:
  * `dead_snapshot_locals_are_cut_and_live_ones_kept`.
  * `an_allocation_dead_at_a_later_guard_is_elided_by_default`: `new`,
    `putfield`, `getfield`, then an `idiv` guard. No `Op::New` survives, only the
    guard's snapshot survives, and it names no dead node.
  * `a_null_check_on_a_fresh_allocation_does_not_pin_it`.
  * `an_allocation_live_at_a_guard_is_elided_with_a_materialization_recipe`: the
    guard's deopt point carries a resumable `VirtualObject`.
  * `ir_new_scalar_replaces_end_to_end` and
    `ir_new_scalar_replaces_through_astore_local` now assert elision in every
    configuration.
* `ir_lower.rs`:
  `test_scalar_deopt_describes_an_object_stored_earlier_in_the_guards_block`
  (new). `test_scalar_deopt_bails_when_store_not_dominating` now puts the guard
  before the allocation in the same block.
* The VM materializer and relock path are covered by the existing
  `deopt_step3_tests` (`resumes_frame_with_held_monitor` and the object/cycle
  tests).

Design notes: `escape-analysis.md` §5.4 and §7.
