# FIXED: IR frame states recorded no monitor stack, so monitor-bearing methods could not resume precisely

**Status: FIXED 2026-09-12.** Found by the 2026-09-12 JIT review (#46). The
optimizing tier's frame states now carry the builder's monitor stack.
`deopt::MonitorInfo` gained a `relock` marker. The resume re-acquires exactly the
locks lock elision removed.

## The defect

`IrBuilder` kept no abstract monitor stack, and every `FrameState` built by
`ir_lower` hard-coded `monitors: Vec::new()`. A frame reconstructed from a body
that had taken a lock therefore described a frame that believed it held none.

Two guards papered over this, and both cost precision:

* `ir_lower::lower_inner` never set `can_deopt_resume` on a graph that still held
  a `MonitorEnter`/`MonitorExit`.
* `try_compile_inner` latched `had_monitors` before escape analysis, because lock
  elision deletes the monitor ops. Without the latch, a guard deopt inside
  `synchronized (new Object()) { ... }` resumed with no lock held, and the
  interpreter's `monitorexit` threw `IllegalMonitorStateException`.

With both guards in place, a monitor-bearing method could deoptimize but never
resume precisely. It fell back to the whole-method re-run. That fallback is
correct only when nothing before the deopt point escaped, and the precise path
exists precisely for the methods where something did.

## The fix

### Builder (`ir.rs`)

`IrBuilder::monitors` is the abstract monitor stack:

* `monitorenter` pushes the locked reference.
* `monitorexit` must pop the innermost one. `same_monitor_object` matches the
  operand through single-value φs, since a loop header gives the lock temp an
  eager `phi(o, o)`.
* Every snapshot copies the stack into the new `SafepointSnapshot::monitors`.
* `MergeState::monitor_snapshots` records each forward predecessor's stack.
  Joins and loop back edges must agree.

The build is refused, and the single-pass backend still compiles the method, on
any of these:

* a `monitorexit` naming anything but the innermost held reference, or with
  nothing held;
* a method-exit `return` with a monitor held;
* two join edges holding different monitors (`unstructured_monitors`);
* a monitor op inside a splice;
* a `NO_NODE` operand.

### Monitor slots are node references

Everywhere a snapshot's locals are treated as references, its monitors now are
too:

* the new `SafepointSlotKind::Monitor`, in every use-list path of `ir.rs`;
* DCE rooting, stale-slot normalisation, the store-consumer check, unroll
  planning and per-copy frames (`ir_optimize.rs`);
* the frame-state lane (`ir_verify.rs`);
* anchor dominance (`ir_schedule.rs`);
* slot pinning, both `plan_slots` and `regalloc`;
* deopt-named sets and fusion tables (`ir_lower.rs`);
* `ea_snapshot_names` and EA load forwarding (`lib.rs`).

### Lowering (`ir_lower.rs`)

`Lowerer::resolve_monitors` builds the deopt point's `monitors`:

* Entries on one object coalesce into one `MonitorInfo` with `lock_depth` equal to
  the count. The install verifier refuses two entries on one object.
* The object is described like a local. It is `VirtualObject`/`VirtualObjectRef`
  for a scalar-replaced object, sharing the frame's `emitted` set. Otherwise it is
  a reference slot: `monitor_object_value` re-spells an int-typed slot as a
  reference slot, and uses the node's home word instead of an int register.
* `relock` is `true` exactly when no live monitor op names the object
  (`Lowerer::live_monitor_operands`). Elision is all-or-nothing per object and
  clears the killed ops' inputs, so a live op means the compiled code took the
  lock itself.

The `can_deopt_resume` exclusion for monitor-bearing graphs was removed, and so
was the `had_monitors` latch.

### Deopt metadata (`deopt.rs`)

`MonitorInfo::relock` is part of machine-state resolution and of interning
hash/equality. Every earlier producer, the single-pass backend's scalar-monitor
snapshots (`x64/deopt_stubs.rs`), emitted only elided locks and sets it `true`.

### VM (`deopt_resume.rs`)

* `build_deopt_frame_inner` pins every monitor object. It refuses a null one,
  and enters each `relock` monitor `lock_depth` times, after materialization. A
  `relock == false` lock is still held by the thread (the monitor table is the
  interpreter's record of block monitors) and is left alone.
* `has_virtual` now also looks at monitor objects.
* Paths with no relock path refuse only `relock` monitors: `resume_from_ir_deopt`,
  `caller_frame_values`, both in-place OSR-exit transfers, and in `lib.rs`
  `resume_after_exit`, `osr_exit_policy` and the OSR entry contract. That keeps a
  live `synchronized` block from newly refusing those paths. It also closes a
  latent hole: an OSR entry into an elided-lock region used to be admitted, and
  the interpreter's lock would never have been released.

## Deliberately NOT changed

* `sink_precise_resume_allowed` still refuses a body whose bytecode takes a
  monitor (`bytecode_holds_monitor`). The additive sink arm serves both backends
  and cannot tell them apart, and the single-pass backend's non-scalar elision
  (`has_elided_monitor`) still leaves no trace in its frame. Optimizing-tier
  bodies resume through the `can_deopt_resume` arm. Its docs were updated.
* An `ACC_SYNCHRONIZED` method carrying virtual objects still refuses. Its
  method monitor is not a frame-state entry.

## Regression coverage

* `ir.rs`: `a_synchronized_blocks_snapshots_carry_its_monitor`,
  `a_monitor_held_across_a_loop_is_matched_through_the_header_phi`,
  `unstructured_locking_refuses_the_ir_build`,
  `a_join_whose_edges_hold_different_monitors_refuses_the_build`.
* `ir_lower.rs`: `a_synchronized_region_lowers_through_the_monitor_helper` now
  asserts the deopt point at the `monitorexit` holds the lock with
  `relock == false`. Also
  `a_held_monitor_is_marked_relock_only_when_its_ops_were_elided` and
  `a_reentrant_lock_coalesces_into_one_entry_with_its_depth`.
* `deopt_resume.rs`:
  `resumed_frame_keeps_a_monitor_the_compiled_code_holds_without_relocking` and
  `resumed_frame_relocks_an_elided_monitor_on_a_real_object`. The existing
  `resumes_frame_with_held_monitor` covers a virtual object materialized and
  relocked.

Design notes: `lock-elimination.md` §8.
