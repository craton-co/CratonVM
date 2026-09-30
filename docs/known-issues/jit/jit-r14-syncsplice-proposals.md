# JIT round 14 wave 2, lane syncsplice -- proposals

Follow-ups to SR-1 (`jit-r13-syncres-proposals-RETIRED-20260929.md`, "Round 14 wave 2 (lane syncsplice): SR-1
landed"): the synchronized splice of a trap-free callee in the optimizing tier. Ranked by expected
benefit over cost. None is measured; the landing itself was not built when this was written.

## SS-1. Measure first: the census and `SyncM` with and without the splice

**What.** Run `R14SyncspliceLeaf` and `bench14.sh` `SyncM` under `CRATONVM_DBG_JITC=1` in the
default arm and with `CRATONVM_JIT_IR_SYNC_SPLICE=0`; count `ir-splice-refused ir-sync-splice-*`
by rule and any `ir-sync-splice-window REFUSED` (the lowerer catching what the builder admitted --
each one is a defect to chase, not a tuning). **Benefit.** Tells whether `SyncM`'s two CALLs are
gone: `step()` is spliced only if its caller is an OPTIMIZING body (the OSR loop door may be
single-pass, where nothing changed), and `staticStep()` only with a caller-held row.
**Cost.** One run. **Risk.** None. **First step.** The two arms, interleaved, 5 reps.

## SS-2. A static callee's mirror without the caller-held row

**What.** A `static synchronized` splice takes its `Op::ConstClass` from the site's
`sync_direct_calls` row, which the VM answers only for a published, closed, WRAPPED body
(`sync_direct_target`: `!compiled.requires_wrapped_entry` refuses). Under
`CRATONVM_JIT_SELF_LOCKING_SYNC_STATIC=1` (the handoff wants it flipped) the published body is
self-locking, the row is refused, and so is the splice. Give the planner the mirror directly: a
`CompileRequest` lookup `(cp_idx) -> (class_cp_idx, mirror_slot)` for a static synchronized method
the Methodref's class declares, initialised (the half of `sync_direct_target` before its
`published`/`compiled` step). **Benefit.** Static splices survive the flip, and no longer depend on
the callee having been compiled at all. **Cost.** Small (VM lookup + one planner branch).
**Risk.** Low: the monitor identity rule is the one the row already applies. **First step.** Split
`sync_direct_target` into `sync_direct_monitor` (identity, init, mirror) and the body half.

## Round 14 wave 3 (lane sync): SS-2 landed

Pending build. VM: `jit_bridge.rs` `sync_direct_target` is split into `sync_direct_monitor` (every
refusal before the body: JVMTI, name refusals, declared by the Methodref's own class, initialised,
`unoverridable`) and the body half; the mirror slot is `sync_direct_mirror_slot`. Both
`sync_direct_lookup` closures (optimizing and background doors) answer, when the body half refuses,
`sync_direct_monitor_answer`: a STATIC callee without an exception table, `entry == 0`, the class
cp index and mirror slot, no pin. Every caller-held route binds only `entry != 0` (the five
consumers in `lib.rs` check it), so nothing calls through that answer. JIT: the splice planner asks
the lookup for a static site with no caller-held row and registers the mirror
(`IrBuilder::add_sync_splice_mirror`, read by `sync_splice_plan`; a row still wins), behind
`CRATONVM_JIT_IR_SYNC_SPLICE_STATIC_MIRROR` (default ON). The splice no longer depends on the callee
having a (wrapped) compiled body at all. Tests `r14w3_sync_planner_tests`. Doc debt: the
`SyncDirectTarget::entry` comment in `compile_request.rs` (not this lane's file) still says
"published entry"; it may now be `0` (monitor half only).

## SS-3. Splice inside an explicit `synchronized` region

**What.** The planner leaves every method with monitor bytecode out and the builder refuses a
splice while its frame holds a monitor, because `elide_nested_monitors` would misclassify the
unstacked pair (`r14w2-syncsplice-nested-lock-elision-unstacked-monitor-ops-patch-FIXED-20260929.md`).
With that patch applied, both refusals can go: a re-entrant `synchronized (o) { o.leaf(); }` is
then eligible for the nested-lock elision of the splice's pair too (JMM: no other thread can hold
`o` inside the outer region). **Benefit.** `Hashtable`/`Vector` callers that lock around calls.
**Cost.** Small once the patch is in. **Risk.** Medium: the pass must delete both halves or none.
**First step.** Apply the patch page, then drop the two refusals behind a sub-switch.

## Round 14 wave 3 (lane sync): SS-3 landed

Pending build. The patch page is applied (FIXED-pending), SS-4 landed first, and both refusals are
dropped behind `CRATONVM_JIT_IR_SYNC_SPLICE_IN_REGION` (default ON, `ir::ir_sync_splice_in_region_enabled`):
`IrBuilder::sync_splice_plan` admits a splice while the frame holds monitors, and the planner's
`top_level` no longer requires `!bytecode_holds_monitor`. The splice's pair on a re-entered `o` stays
a real recursive level (the nested-lock elision skips window ops; eliding it too is left, see
`jit-r14-sync3-proposals.md`). Probe `R14SyncSpliceStatic` (`nested` line).

## SS-4. Lock coarsening across a splice and a caller-held CALL (or a `monitorenter`)

**What.** `coarsen_adjacent_monitors` merges a splice's `MonitorExit o` with an immediately
following `MonitorEnter o` of any kind. Between two splices `ir_lower` re-pairs the merged window
(`sync_splice_window_refusal`); against a caller-held synchronized CALL on the same receiver
(`o.leaf(); o.otherSync();`) both that route's `sync_direct_call_monitor` and the splice's pairing
fail, and the METHOD's optimizing compile is refused. Either refuse such a coarsening in
`coarsenable_monitor_pair` (one half a recorded window op, the other not), or teach both checks the
mixed window (the CALL is admitted inside a window whose exit releases on its exceptional edge).
**Benefit.** Removes a compile-refusal cliff on a plausible shape. **Cost.** Small (the refusal).
**Risk.** Low. **First step.** The refusal, with a unit test on a builder-shaped graph.

## Round 14 wave 3 (lane sync): SS-4 landed

Pending build. `ir_optimize.rs` `coarsenable_monitor_pair` returns `None` when exactly one of the
exit/enter pair is a recorded window op; two windows still merge (the lowerer re-pairs them). No
kill switch: every mixed coarsening it refuses used to end in a refused compile (neither the
window pairing nor the caller-held check could pair the survivor). Test
`r14w3_sync_monitor_pass_tests::a_mixed_window_coarsening_is_refused_and_two_windows_still_merge`.
The EA-side coarsening (`ea_ir_bridge.rs`, `CRATONVM_JIT_LOCK_COARSEN`) has the same mixed case,
fail-closed; exact patch `r14w3-sync-ea-lock-coarsening-mixed-window-patch-FIXED-20260929.md`.

## SS-5. The planner's store fence for synchronized bodies

**What.** `ir_splice_store_fence_refuses_site` (W5-1) still judges a synchronized body by the
replay rule, which the builder no longer applies inside one (`sync_splice_open`: nothing in the
body may trap). A synchronized setter that stores and then branches is refused as a SITE by the
planner although the builder would build it. Skip the planner's fence for a site whose
`callee_is_synchronized` passed `ir_sync_splice_body_scan`. **Benefit.** Setters with a guard
(`if (x > max) max = x;` after another store). **Cost.** One condition. **Risk.** Low (the scan and
the two graph checks stay). **First step.** The condition plus a planner unit test.

## Round 14 wave 3 (lane sync): SS-5 landed

Pending build. `lib.rs` `ir_splice_store_fence_refuses_site` answers `false` for a synchronized
body the scan admits (`ir_sync_splice_skips_store_fence`), only while
`CRATONVM_JIT_IR_SPLICE_COMMITTED_STORE` is on (with it off the builder refuses the store itself),
behind `CRATONVM_JIT_IR_SYNC_SPLICE_STORE_FENCE_SKIP` (default ON). Test
`r14w3_sync_planner_tests::the_store_fence_skips_an_admitted_synchronized_setter` (the `setMax`
shape: the walk still says `Some(true)`, the site is no longer refused). Probe `R14SyncSpliceStatic`
(`setter` line).

## SS-6. Guarded (profile / CHA) synchronized splices

**What.** Only constant-pool resolutions of a monomorphic target are admitted. A CHA-bound or
profile-guarded virtual synchronized site (the caller-held CHA route's `Hashtable`-style shape)
could splice behind its `ExactClassIs` guard, with the enter after the guard (as
`enter_receiver_monitor` orders it) and the miss edge the call. The multi-return join the guarded
splice builds needs the exit placed on the hit edge only, before the join. **Benefit.**
Non-final library classes (`Hashtable.get` is not trap-free, but `size`/`isEmpty` are).
**Cost.** Medium (builder: exit on the hit edge; lowerer: the window region ends before the join,
which the region check already allows). **Risk.** Medium. **First step.** Admit `size()`-shaped
bodies behind the CHA guard only, with `R14SyncspliceLeaf`'s shapes on a non-final class.

## SS-7. Fold `Thread.holdsLock(this)` inside a synchronized splice

**What.** A spliced body cannot call, so a `holdsLock(this)` in it refuses the splice. The
M3-1(a) fold (`self_lock_holds_lock_fold_at`) proves `true` for the method's own monitor; inside
a synchronized splice the same proof holds for the splice's receiver (the window is on it).
**Benefit.** Small; assertion-style code. **Cost.** Small. **Risk.** Low. **First step.** A
builder arm for the `Thread.holdsLock` invokestatic whose argument is the open sync frame's
receiver, constant `1`, admitted by the window predicate as a `Const`.

## Round 14 wave 4 (lane sync4): SS-7 landed

Pending build. Builder: `ir.rs` `IrBuilder::try_fold_holds_lock`, asked by the `invokestatic` arm
right after the site's `invoke_info` row is read: a `Thread.holdsLock(Ljava/lang/Object;)Z` row
(kind 3) whose argument node is the receiver of an open INSTANCE synchronized splice, or an entry
of the frame's own monitor stack (both through trivial φs), pops the argument and pushes `1`; no
call is built. Planner: `lib.rs` `ir_sync_splice_body_scan` admits an `invokestatic` whose row is
that call and whose argument slot is the receiver (`ir-sync-splice-holds-lock-not-receiver`
otherwise); `ir_sync_splice_folds_every_call` lets such a body past the replay fence
(`ir_splice_fence_refuses_site`) and the unbindable-call refusal (`append_ir_inline_site`), since
it leaves no call and traps nowhere. A body the planner admitted and the builder did not fold
builds a call inside the window, which `end_splice` refuses as a site. Switch
`CRATONVM_JIT_IR_HOLDSLOCK_FOLD` (default ON; `0` keeps every call, and the scan refuses such a
body again). Static windows are not matched (`holdsLock(C.class)` is an `ldc` the scan refuses).
Tests `ir::r14w4_sync4_builder_tests`, `r14w4_sync4_holds_lock_scan_tests`; probe
`C:\craton\jitr14-probes\src\R14Sync4HoldsLock.java`. It does NOT make `SyncM.stepNested`
spliceable (two returns and a nested synchronized call): proposal SS8-1 in
`jit-r14-sync4-proposals.md`.
