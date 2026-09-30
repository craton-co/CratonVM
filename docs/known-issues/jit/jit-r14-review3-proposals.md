# JIT round 14 wave 3, lane review3: proposals

Filed while reviewing wave 2 (20a1dbb4f). Ranked by expected benefit over cost.
Findings (defects, hazards) are on `r14w3-review3-wave2-review-findings-FIXED-20260929.md`, not here.

## RV3-1. Synchronized-splice window refusals rebuild without the site instead of losing the method

**Benefit.** Today a window the optimizer spoiled after the build (coarsening across caller code,
a floated trapping node) fails the whole optimizing compile (`ir_lower.rs`
`sync_splice_window_refusal` -> `latch_bailout`). With a site-level answer the method keeps its
tier and only that call site stays a call. Every hot method with a synchronized leaf call is
exposed, and SR-1 is default ON.
**Cost.** Small: the lowering already knows the enter's `bytecode_pc`; route it into the same
refused-site channel the builder's W5-2 rebuild uses and retry the plan once.
**Risk.** Low (the retry path exists for builder refusals).
**First step.** Count them: a `[cratonvm-jitc] ir-sync-splice-window REFUSED` census over the
battery to size the cliff before building the retry.

## RV3-2. A code grace separate from the inline-cache grace

**Benefit.** Keeps every consumer of "every thread has been outside compiled code since g"
(compile-id reissue today; any future code-address reuse) on the strong proof, while M8-1's cheaper
miss-handler evidence keeps speeding up inline-cache way reuse and table reclamation.
**Cost.** One more atomic generation noted at the three grace sites; `reserve_compile_id` switches
predicate.
**Risk.** Low; it can only delay id reuse (bounded by the id table's size).
**First step.** Item 1 of the findings page has the exact patch.

## RV3-3. Bimorphic guarded splice: splice the SECOND body for a receiver proven to be its class

**Benefit.** `guarded_receiver_splice_plan` refuses a site whose receiver is an `Op::New` of any
class but the FIRST planned one (`ir.rs` allocation check), so `Node n = new B(); n.val(x)` at a
site profiled A/B keeps its call although B's body is already in the plan (it is the second body).
Swapping in the second body with no test (as the exact-receiver path does for the first) removes a
call and two compares on such paths.
**Cost.** Small: in the allocation check, when the receiver is `Op::New` of the bimorphic second
class, take `begin_splice` with the second body's row swapped in (the swap
`begin_bimorphic_receiver_splice` already does) and `guarded_splice_pending` set.
**Risk.** Low: exactness is by allocation, the same proof the exact-receiver path uses.
**First step.** Census how often a planned bimorphic site sees an `Op::New` receiver of either class
(`CRATONVM_DBG=jitc` line at the allocation check).

## RV3-4. A lock-free "no bootstrap append" answer for the loader natives

**Benefit.** Removes up to three class-manager read-lock round trips from every `loadClass` in a VM
that never appended to its bootstrap search (almost all of them).
**Cost.** One per-VM `AtomicBool` and two setters (see findings item 2).
**Risk.** Very low.
**First step.** The patch in findings item 2.

## RV3-5. A runtime audit arm for monitor windows with no frame state

**Benefit.** SR-1 and the caller-held synchronized CALL both hold a monitor no frame state names;
their soundness is argued statically at three gates. An audit arm
(`CRATONVM_DBG=sync-window-audit`) that makes the deopt entry and the exception epilogue assert the
thread holds no such monitor (a per-thread count bumped by the enter stub's fast and slow path when
the op belongs to a window, dropped at the exit) would turn a future gate hole into an immediate,
named failure instead of a leaked lock.
**Cost.** Medium: the stubs need a window flag, only in the audit build path.
**Risk.** None with the arm off (nothing emitted).
**First step.** Mark window monitor ops in the lowering (`graph.sync_splice_windows` is already
there) and emit the count only under the audit flag.
