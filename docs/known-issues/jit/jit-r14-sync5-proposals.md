# JIT round 14 wave 5, lane sync5 -- proposals

Follow-ups to SS8-3 (the compiling synchronized method's own monitor as a graph fact,
`Graph::method_monitor_param`), SS8-1 b (multi-return synchronized splices) and the loop-φ fix of
the `holdsLock` fold. Nothing here was built or measured by the lane. The two resolver-bound
residuals (SS8-1 a, SS8-5) are on `r14w5-sync5-sync-splice-resolver-residuals-FIXED-20260929.md`, not
here.

Ranked by expected benefit over cost: **S5-1, S5-2, S5-3, S5-4**.

## S5-1. Audit the SS8-3 entry invariant at run time (debug switch)

**What.** SS8-3 rests on one claim: an optimizing body of an `ACC_SYNCHRONIZED` instance method
is only ever executed with its receiver's monitor held by the executing thread (every
publication stamps `requires_wrapped_entry`; every raw consumer filters on it; OSR enters from
a frame that holds it). Add `CRATONVM_DBG_SYNC_METHOD_ENTRY_AUDIT=1`: the wrapped-body doors
(`jit_bridge.rs` `execute_wrapped_body_door_locked`, `execute_jit_call*`'s monitor guard) and
the OSR transfer assert `holds_lock(receiver)` before the first instruction of a body whose
artifact records `method_monitor_param` (a new `CompiledMethod` bit), and count/print a miss.
**Benefit.** Turns the one soundness argument of SS8-3 from "checked by reading" into a soak
signal; the same audit guards the future SS8-2 self-locking optimizing body. **Cost.** Small
(one bit on the artifact, three call sites, a counter with a reader). **Risk.** None (debug
only). **First step.** The artifact bit, stamped in `lib.rs` next to `method_monitor_param`.

## S5-2. Fold `holdsLock` after the build, on the finished graph

**What.** The builder's fold now refuses a loop-header φ whose back edges are pending (the wave-5
fix), so `synchronized (o) { while (..) { holdsLock(o) .. } }` with `o` re-read from a local
keeps its call. After the build every φ is complete: a small `ir_optimize` pass could replace an
`Op::Call` of `Thread.holdsLock` whose argument strips (`strip_trivial_monitor_phis`) to the
method monitor (`Graph::method_monitor_node`) by `Const(1)`, bypassing its memory token. The
region case needs "dominated by the region's enter, not by its exit" and is harder; start with
the method monitor. **Benefit.** Assertion-heavy synchronized code in loops
(`assert Thread.holdsLock(this)` in collection internals). **Cost.** Small. **Risk.** Low (the
fact is whole-body). **First step.** Count the `holdsLock` calls left in optimizing bodies of
synchronized methods under `CRATONVM_DBG_JITC`.

## Round 14 wave 6 (lane sync6): S5-2 landed

`ir_optimize::fold_method_monitor_holds_lock`, run first in `optimize_passes` (before the scalar
cleanup, which then folds the branches the constant fed), behind
`CRATONVM_JIT_IR_HOLDSLOCK_METHOD_FOLD` (default ON; also off under
`CRATONVM_JIT_IR_HOLDSLOCK_FOLD=0`). The method-monitor case only, as proposed: an `Op::Call` whose
row is `Thread.holdsLock` (`ir::is_holds_lock_info_row`, now shared with the builder's fold) and
whose argument strips through trivial φs of the finished graph to `Graph::method_monitor_node()`
becomes `Const(1)`; memory users take the call's incoming token, value users (snapshot slots
included) the constant. Census row `holdslock-folded-on-finished-graph`, a `CRATONVM_DBG_JITC`
line per compile. The region case (dominated by the enter, not by the exit) is not built.
Tests: `ir_optimize::r14w6_sync6_holds_lock_finished_graph_tests`. Probe
`C:\craton\jitr14-probes\src\R14Sync6NestedHeld.java` (`holdsLoop`, `holdsMixed`).

## S5-3. A `synchronized (this)` region inside a synchronized method

**What.** Treat the method monitor as an outer region for `ir_optimize::elide_nested_monitors`:
a bytecode region on `Param(method_monitor_param)` is recursive. Unlike a splice window it is on
frame states, so the elision must keep the frames' monitor lists consistent exactly as the
existing nested elision does for a region nested in a region (read how it marks the inner
region's snapshots before reusing it). **Benefit.** Code that locks `this` again inside its own
synchronized methods (common in hand-written "belt and braces" locking and in generated code).
**Cost.** Medium. **Risk.** Medium (a deopt inside the elided region must resume with the right
recursion count: the interpreter's later `monitorexit` must not release the method's hold).
**First step.** A census of such regions in the Spring run.

## S5-4. Multi-return synchronized splices by block walk

**What.** SS8-1 b admits only pc-order bodies; a body the planner gives a block walk (a rotated
loop -- none today, since the scan admits no back edge) is refused. If a later wave admits
counted loops in synchronized splices (the window rule would need a safepoint-free loop), the
exit must go after `finish_multi_return_splice` on the block-walk path too.
**Benefit.** Only with loops admitted. **Cost.** Small once loops are. **Risk.** Low.
**First step.** None until the window admits a back edge.
