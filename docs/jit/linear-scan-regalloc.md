# Linear-scan register allocation

An IR-level register allocator for the CratonVM optimizing JIT, living at the
bottom of `jit/src/regalloc.rs` (the top of that file is the older,
bytecode-local graph-colouring allocator; the two are independent).

**Why this exists.** The C2 review
(the C2 review) has a P0 lane *"Implement
linear-scan register allocation"*. Before this, `ir_lower.rs` gave every IR
value a frame slot and every operand a load: a `+` on two locals is three memory
accesses, and the only thing that ever reached a register was a bytecode local
promoted by the file's older colouring pass.

---

## What is here

| Item | What it is |
|---|---|
| `build_live_model` | Positions, intervals, uses, loop weights for one scheduled graph. Total: never panics, never fails. |
| `LiveModel` | The result. `converged == false` ⇒ nothing is promotable. |
| `RegClass` / `PhysReg` / `RegSpec` / `RegFile` | The physical register file, GP vs XMM, with the caller-saved bit. |
| `MachineModel` | Clobber sites, safepoints, fixed (ABI) constraints. `for_graph` derives the first two; `pin_entry_params` adds the third. |
| `allocate_linear_scan` | The scan: intervals → segments, with splitting, a spill cost model and rematerialization. |
| `Allocation` | Per-value location timeline, home-slot colouring, spill/reload/remat/move events and counts. |
| `verify_allocation` | Proves the allocation before anyone can use it. |
| `resolve_parallel_copy` / `phi_edge_copies` | Phi-web resolution as a *parallel* copy, cycles included. |
| `record_allocation_metrics` | Publishes `spills` / `reloads` / `peak_live_values` to a `CompileRecorder`. |

`allocate_linear_scan` calls `verify_allocation` on its own result and returns
the bailout rather than the allocation when the check fails. There is no way to
obtain an unverified `Allocation` from this module.

## The position model

The allocator and `ir_lower::plan_slots` must not disagree about liveness — an
allocator and a slot allocator that disagree about who is live where is a
miscompilation, not a missed optimization. `build_live_model` therefore
reproduces `plan_slots`' model exactly:

* one linear position per scheduled node, in block order, then one for the block
  terminator, then one for the block's **outgoing edge**;
* a phi's value inputs are consumed at the **predecessor's edge position**,
  never at the phi — that is where `ir_lower::emit_phi_copies` reads them;
* ranges are widened to whole block spans wherever the backward liveness fixed
  point says the value is live-in / live-out, which is what makes a loop-carried
  value live across the whole loop;
* overlap is **endpoint-inclusive**: `[4, 7]` and `[7, 9]` overlap, because a
  node's lowering may write its result before it reads its operands.

This is a deliberate re-implementation rather than a call: `plan_slots`,
`SlotPlan`, `LiveRange` and `SlotClass` are all private to `ir_lower.rs`.

> **Reconcile:** making `ir_lower`'s `LiveRange`, `SlotClass` and the
> `plan_slots` entry point `pub(crate)` would let both files share one
> implementation. That is the right end state and it is a visibility change
> only. Until then, the two lists that must stay in lockstep are
> `regalloc::ir_op_defines_value` ↔ `ir_lower::op_defines_result_slot`, and the
> pinning rules in `build_live_model` step 4 ↔ `plan_slots`' `SlotClass::Pinned`.

## The subset that is promoted

A value takes a register only if **all** of these hold. Everything else keeps
the home slot `ir_lower` gives it today, which is always correct:

1. the liveness fixed point converged (`LiveModel::converged`);
2. it is not pinned — not a phi (its home is what the edge copies write) and not
   named by any deopt frame state (the home must hold the value at *any*
   recorded bci, which is not a property this pass establishes);
3. its type has a register class (`Void` / `Control` / `Memory` do not);
4. **if it is a `Ref`, its live range contains no safepoint** (below);
5. the register file has at least one register of its class.

Not modelled, on purpose:

* **Lifetime holes.** An interval is one contiguous `[lo, hi]`, as in
  `plan_slots`. A value dead across the middle of its range still holds its
  register there. This over-approximates liveness, which is the safe direction:
  it costs registers, it cannot alias two live values.
* **Register pairs / sub-registers.** One value, one register. `Long` and `Int`
  are both one GP register on x86-64; `Float` and `Double` both one XMM.
* **Coalescing.** Phi webs are resolved with explicit copies, not by biasing the
  allocation.

## The `Ref`-at-a-safepoint decision

**References are kept in memory across every safepoint.** A `Ref` whose live
range covers a safepoint position is not promoted at all; one that lives and
dies strictly between safepoints may take a register.

The alternative — a `Ref` in a register across a safepoint, with that register
named in the oop map — needs a register bank in the oop map, in the GC's frame
walker and in the deopt frame reconstructor, and it needs the collector to be
able to **update** the register on a moving collection.
`ir_lower::emit_safepoint_map` publishes frame slots and nothing else. Doing it
without that would either hide a live oop from the collector or hand it a stale
one after evacuation — both are silent heap corruption, and this branch has
already had one moving-GC use-after-free from a location the collector could not
see.

The conservative rule costs GP registers in reference-heavy code and costs
nothing in the arithmetic and loop kernels this pass exists for.
`verify_allocation` re-checks it independently of the allocator, so relaxing the
rule later requires deleting a test, not forgetting a condition.

> **Since 2026-09-09 the rule has one explicit knob.**
> `MachineModel::refs_may_cross_safepoints` (default `false`) lifts exactly this
> refusal, both in `allocate_linear_scan`'s candidate filter and in
> `verify_allocation`'s proof 5, and nothing else. Test:
> `refs_may_cross_safepoints_lifts_that_refusal_and_only_that_one`. The only
> caller that sets it is `ir_lower::ir_lower_machine_model`, and only when
> `CRATONVM_JIT_IR_REF_RESIDENCY` (default off) and
> `CRATONVM_JIT_IR_REF_RESIDENCY_CROSS_SAFEPOINT` (default on within it) are
> both on. That caller does not add a register bank to the oop map. The register
> is a write-through copy of a home slot the map still names, and
> `Lowerer::invalidate_ref_residency` drops the copy wherever a collector could
> have run. See `docs/jit/linear-scan-wiring.md`, *Spill slots and the oop map*.

## Splitting, spilling and rematerialization

Poletto–Sarkar linear scan with three additions.

**Splitting at a blocking position.** When the only free register of the right
class is unavailable from position `p` onward — a call clobbers it, or somebody
else is pinned to it there — the interval keeps that register over `[lo, p-1]`
and its suffix is re-queued at the first use at or after `p+1`. Between the two
the value sits in its home slot. This is what makes a call cost a store/reload
pair for exactly the values that could not be given a callee-saved register, and
nothing for the ones that could.

**The spill cost model.** With nothing free, the victim is chosen among the
current interval and the actives of the same class that started *strictly
earlier*, maximising

```
next_use_distance × 1024 / frequency_weight
```

so a value whose next use is far away and whose uses are cold goes first, and a
value used on the next instruction inside a nested loop goes last. The
frequency weight is the same `LOOP_WEIGHT_PER_DEPTH` model the bytecode
allocator in the same file uses.

**Rematerialization.** `Op::Const`, `Op::ConstF` and `Op::Param` are
rematerializable: evicting one costs no store at all, and its next use is an
immediate (or a load from the incoming argument slot the prologue already
wrote). They score `u64::MAX` in the eviction contest, so they are always
evicted first, and `ls_finish` emits `SpillKind::Remat` for them instead of a
store/reload pair. `Allocation::remats` is reported separately from `spills` so
a dashboard cannot mistake a free re-materialisation for memory traffic.

**Termination.** Every re-queue strictly advances the interval's start, so the
scan terminates regardless. `split_budget(intervals) = 4 × intervals + 16`
bounds *compile time* on top of that: a graph that needs more splits than this
is one where every promotion immediately costs a reload, i.e. one where linear
scan buys nothing, so it bails with `BailoutReason::RegisterPressure` rather
than spending the compile budget arriving at the frame layout `ir_lower` already
had.

## Bailing, never guessing

Every refusal is a structured `Bailout`; nothing in this module panics.

| Situation | Reason |
|---|---|
| Two values require the same register at the same position | `RegisterPressure` |
| One value requires two registers at one position | `RegisterPressure` |
| A pinned register is clobbered inside the pinned value's range | `RegisterPressure` |
| The split budget is exhausted | `RegisterPressure` |
| `verify_allocation` rejects the result | `Internal(...)` |

`Internal` is the right variant for a verifier failure: it is a compiler bug,
not a property of the input program. The method loses its optimized body; the VM
does not die.

## What `verify_allocation` proves

1. **Shape** — one timeline per graph node; a value's segments tile its whole
   live range in order, with no gap and no overlap.
2. **Register class** — every register a value holds is in that value's class
   and in the allocatable file.
3. **No aliasing** — two *different* values never hold the same `PhysReg` at the
   same position, under the endpoint-inclusive overlap rule. This is the
   invariant the pass exists to keep.
4. **ABI** — no value is live in a register across a position that clobbers it;
   no value occupies a register another value is pinned to at that position; a
   pinned value never holds a *different* register at its pinned position. (A
   pinned value that was not promoted at all is not a violation — the prologue
   leaves it in its home slot, which is where every reader looks today.)
5. **The GC rule** — no `Ref` in a register across a safepoint.
6. **Homes** — two values that share a home word have disjoint live ranges, and
   a home word never mixes `Ref` / `Prim` / `Pinned` pools, so a word the oop map
   names can never come to hold a primitive.
7. **Bookkeeping** — the event list is in position order and its kinds sum to
   the reported spill / reload / remat / move counts.

## Phi resolution is a parallel copy

The copies on one CFG edge all read their sources **before** any destination is
written. Emitting them in list order is wrong whenever a destination is also
somebody's source. The canonical case is a loop that swaps two locals:

```java
for (…) { int t = a; a = b; b = t; }
```

whose two loop-header phis are each other's back-edge value, so the edge asks for

```
slot(a') ← slot(b)
slot(b') ← slot(a)
```

Emitted in order, the second copy reads the `slot(a')` the first one just
overwrote and both locals end up holding `b`.

`resolve_parallel_copy` returns an order in which every read still sees the
pre-copy value: destinations nothing else reads go first, and when only cycles
remain one element is parked in a scratch, which frees its predecessor and
unwinds the rest of the cycle. It costs at most one `Save` / `Restore` pair per
cycle and nothing at all on the acyclic majority. The scratch is deliberately
unnamed in `CopyOp` — it is whatever the backend already keeps free at an edge.

`phi_edge_copies` builds the `(destination, source)` list for one predecessor
block from an `Allocation`, reading each source at the predecessor's **edge
position**.

> **Reconcile — this is a live bug in `ir_lower.rs`, not only a design note.**
> `ir_lower.rs:1090` emits the gathered `copies` sequentially:
>
> ```rust
> for (dst_slot, src_slot) in copies {
>     self.load_to_rax(src_slot);
>     self.store_rax(dst_slot);
> }
> ```
>
> `prealloc_phi_slots` gives every phi its own frame word, and a loop that swaps
> two locals makes each phi the other's back-edge value, so `copies` on that back
> edge is exactly the two-element cycle above and the emitted code computes the
> wrong answer. The fix is to route `copies` through
> `regalloc::resolve_parallel_copy` and emit `Move` / `Save` / `Restore` (RAX is
> already the copy scratch there, so `Save`/`Restore` need one extra frame word
> or a second scratch register — RAX cannot be both). This wave may not edit
> `ir_lower.rs`; the primitive and its tests are in place for the wave that can.

## Metrics

`CompilationReport::spills` and `::reloads` are declared `Measured<T>` and
report *not measured* because nothing computed them.
`record_allocation_metrics(&recorder, &alloc)` is the producer, using the
existing `CompileRecorder::set_spills` / `set_reloads` /
`set_peak_live_values` setters.

> **Reconcile:** `peak_live_values` is also fed *ambiently*, through the free
> function `metrics::note_current_peak_live_values` (`jit/src/metrics.rs:802`),
> because `ir_lower::lower_inner`'s signature is pinned and cannot take a
> recorder. The optimizing pipeline will want the same for the other two:
>
> ```rust
> /// Record the spill count against the innermost in-flight compilation.
> pub fn note_current_spills(n: usize) { … }   // body identical to
> pub fn note_current_reloads(n: usize) { … }  // note_current_peak_live_values
> ```
>
> added to `jit/src/metrics.rs` next to `note_current_peak_live_values`, and
> called from `allocate_linear_scan` on success. This wave may not edit
> `metrics.rs`, so the recorder-taking form is what exists today.

## Tests

`jit/src/regalloc.rs`, `mod linear_scan_tests`. Fixtures hand-build a
`LiveModel` so the intervals under test are exact; the end-to-end cases run a
real `IrBuilder` graph through `ir_schedule::schedule`.

* non-overlapping values share a register; overlapping ones (including two that
  merely touch at one endpoint) do not;
* a call clobbers the caller-saved registers, and a value that crosses it lands
  in a callee-saved one when there is one;
* with no callee-saved register the interval is split and the reload lands on
  the *use*, not on the split point;
* a rematerializable constant is evicted first and is never stored;
* an entry parameter keeps its ABI register;
* a `Ref` live across a safepoint is not promoted; one that lives and dies after
  it is;
* `verify_allocation` rejects a planted alias, a class violation, both ABI
  violations, a `Ref` in a register at a safepoint, and a timeline gap;
* an unsatisfiable fixed constraint and an over-pressure graph both bail with
  `RegisterPressure`;
* parallel copies: an acyclic chain, a two-element cycle, a three-element cycle,
  a cycle with a fan-out, two disjoint cycles, a self-copy, and a duplicate
  destination — each simulated against the parallel semantics;
* end to end on the swap loop: phi arguments are charged to the predecessor
  edge, the back edge really does produce a copy whose destination is also a
  source, and the whole allocation verifies.
