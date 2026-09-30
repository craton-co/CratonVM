# JIT round 14 wave 2, lane statics: proposals

Status: OPEN (proposal book; ideas, not work items)
Area: per-VM state in `vm/src` (process statics keyed by `vm_identity`), dead opt-in switches
Severity: proposals
Found by: round 14 wave 2 lane statics

Ranked by value for the risk.

## ST14-1. Move the three remaining `vm/src` `vm_identity`-keyed tables onto `SharedVm`

MISC11-1/2 showed the pattern is cheap: a table whose rows are one VM's facts, keyed by
`vm_identity` and purged by `release_vm_native_state`, becomes a field of the VM and loses both
the key half and the purge. Three such tables are in the `vm` crate itself (so no native-api
accessor is needed):

* `runtime/interpreter.rs` `multianewarray_plan_cache()` (`forget_vm_multianewarray_plans`,
  `invalidate_multianewarray_plans_naming` both take a `vm_identity`);
* `runtime/invokedynamic.rs` `GENERIC_INDY_SITES` (`forget_vm_generic_indy_sites`; raw
  `CallSite` addresses, and a registered root source);
* `runtime/instrument.rs` `transformer_chains()` and its load-time offer memo
  (`forget_vm_transformers`).

* Benefit: -3 or more on `VM_BASELINE`; no cross-VM lock traffic on the plan cache (read on every
  `multianewarray` miss); the teardown list in `release_vm_native_state` shrinks, and a row can no
  longer be left behind by a teardown path that skips it.
* Cost: a field each (`SharedVm::jit` or a realm); every reader already has `&SharedVm`.
* Risk: low for the plan cache; medium for the indy sites (a root source walks them during GC:
  the walk must reach the per-VM table from the VM it is collecting, which it already knows).
  `interpreter.rs`, `invokedynamic.rs` and `instrument.rs` are interpreter-round files: coordinate.
* First step: the plan cache (a pure memo, no roots), with a two-VM unit test like
  `helpers.rs::each_vm_has_its_own_ldc_slot_table`.

## ST14-2. A dedicated kill switch for the back-edge poll's FP reload

`ir_lower.rs::ir_poll_preserves_fp` (round 11 wave 6: the back-edge poll's slow path reloads the
FP values the residency plan keeps in caller-saved XMM registers, instead of modelling the poll as
an XMM clobber) had two kill switches: `CRATONVM_JIT_IR_POLL_OUTLINE=1` and
`CRATONVM_JIT_IR_LS_SPLITS=1`. Wave 2 deleted the first (dead, superseded by the default-on
entry-poll outline). The remaining one also changes the register allocator, so it cannot isolate
the reload.

* Benefit: a one-variable A/B / bisect lever for a change that touches every FP loop's GC point.
* Cost: one `runtime_flag_default_on("CRATONVM_JIT_IR_POLL_FP_RELOAD")` in `ir_poll_preserves_fp`.
* Risk: none on the default arm.
* First step: add it, run `R11LowerFpPollReload` / `R11IrcoreFpCarried` (jitr11 probes) with it
  at `0`.

## ST14-3. Retire `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH` and `CRATONVM_JIT_C2_ALLOC_UPGRADE` properly

Both are on the r14 opt-in triage's delete list and both were KEPT by wave 2 because something
still runs an arm with them:

* `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH=1`: `C:\craton\jitr11-probes\src\R11IrcoreSinkTies.java`
  documents it as its second arm (the W11-3 tie-break is only reachable under it). Its phi-edge half
  is already separable (`CRATONVM_JIT_IR_SINK_PHI_EDGE`).
* `CRATONVM_JIT_C2_ALLOC_UPGRADE=1`: `vm/tests/jit_guarded_join_and_entry_trap_param.rs` sets it to
  pin an allocating method on the optimizing tier; `R11W18TierCalleePromotion` and two
  `tools/probes` files name it.

* First step: give the test another way to pin the tier (`CRATONVM_C2_ACCEPT=always`, or a
  non-allocating fixture), re-point the probes' arms, then delete the readers
  (`ir_schedule.rs::sink_equal_depth_enabled`, `lib.rs` ~6990) with their dead arms.
* Risk: low; the default arms are unchanged.

## ST14-4. `CRATONVM_JIT_SYNC_METHODS` needs its A/B, or a retirement

The triage lists it as superseded (self-locking bodies, door-locked routes), but
`regression-suite/src/RSyncMethodJit.java` documents `CRATONVM_JIT=sync-methods` as a required
second arm and jitr11's `R11TierSyncResume` ran under it, so wave 2 left it. It has been
"default-OFF pending the A/B and the concurrency soak" since 2026-08-04.

* First step: run `RSyncMethodJit` and `R11TierSyncResume` default vs `sync-methods` on the Linux
  host; if the default arm now reaches compiled code for every synchronized shape the flag admits
  (the self-locking census says so), drop the arm from `RSyncMethodJit`'s doc and delete
  `jit_bridge.rs::jit_sync_methods_enabled`.
