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
| `split_edge_conflicts` | Which split values a linear-order consumer would get wrong on a non-fall-through CFG edge (r9). |
| `split_edge_resolution` / `EdgeResolution` | The per-edge parallel copy that repairs exactly those values, with where it may be placed (r9 wave 2). |
| `LiveModel::live_in_at` | Per-block live-in, kept from the fixed point (`None` when unknown) (r9 wave 2). |
| `MachineModel::reads_precede_clobber` / `mark_call_operand_reads_first` | Opt-in: call positions whose operands are read before the clobber, so a value whose last use is there keeps its register (r9 wave 2). |
| `LiveModel::pin_values` / `pin_scalar_replacement_fields` | Caller-supplied pins, including `plan_slots`' scalar-replacement set (r9). |

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
>
> **The second pair is NOT in lockstep (r9).** `plan_slots` also pins every
> scalar-replacement field value (`sr_map`), which `build_live_model` cannot see
> — it takes no `sr_map`. Harmless for `ir_lower`, which releases the deopt pins
> and adopts `plan_slots`' layout anyway, and wrong for a consumer that uses the
> allocator's own colouring. Filed as
> `docs/known-issues/jit/regalloc-live-model-misses-scalar-replacement-pins-20260918.md`.
> The `fresh_only` rule (a helper-produced `Ref` may donate a home word but never
> receive a recycled one) was out of step too — `ls_color_homes` applied it to
> `Op::Call` alone, `plan_slots` to `ConstString` / `ConstClass` / `LoadStatic`
> as well — and is fixed.

## The subset that is promoted

A value takes a register only if **all** of these hold. Everything else keeps
the home slot `ir_lower` gives it today, which is always correct:

1. the liveness fixed point converged (`LiveModel::converged`);
2. it is not pinned — not a phi (its home is what the edge copies write) and not
   named by any deopt frame state (the home must hold the value at *any*
   recorded bci, which is not a property this pass establishes). A
   write-through consumer may lift both pins first:
   `LiveModel::release_deopt_pins` / `release_phi_pins`, which `ir_lower` calls
   (the phi release behind `CRATONVM_JIT_IR_PHI_RESIDENCY`);
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
  allocation. What coalescing would take is recorded in
  `docs/internal/fixed-bugs/linear-scan-no-phi-coalescing-costs-a-move-per-carried-value-FIXED-20260918.md`
  (r9 wave 2): the phi-edge interval extension that fixed the 2026-09-05 alias
  bug makes every phi overlap its back-edge source by construction, so a hint
  alone can never fire.

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

> **Only where the consumer reads splits.** That last sentence is a property of
> the *allocation*, and until R4 it was not a property of the emitted code: the
> only production consumer, `ir_lower::plan_register_residency`, promoted a
> value only if it held one register over its **entire** live range and declined
> every split outright. A value given a register everywhere except across one
> call therefore got no register at all and paid a home store plus a reload at
> *every* use. The consumer now exists, behind `CRATONVM_JIT_IR_LS_SPLITS`
> (**default off**); see *Who consumes this* below. With the flag off the
> sentence is still false of `ir_lower`'s output, by design — the flag is how
> both arms are timed from one binary.

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

**Register choice (r9).** Among the registers nothing blocks over the whole
interval, a **caller-saved** one is taken before a callee-saved one listed
ahead of it: an interval with no blocking position in a volatile register
crosses no call and does not need a register that survives one, and taking the
callee-saved one anyway leaves the next call-crossing interval to split. This
is a no-op for `ir_lower`'s files (its GP file is all callee-saved and its XMM
file lists the volatile registers first), and it is what `RegFile::x86_64` /
`RegFile::aarch64`, which list callee-saved registers first, need. When a value
is *required* in a register that another value holds, only that holder is a
candidate victim: evicting anyone else frees a register the retry will not take.

**A last use at a call (r9 wave 2, opt-in).** A clobber at `p` blocks a register
over the whole of `p`, which splits a value whose range *ends* at `p` because
`p` is its last use — typically a `double` passed to a call or a helper-backed
op. `MachineModel::reads_precede_clobber` lists the positions at which the
emitter reads its operands before anything is destroyed; at a listed position a
value whose live-range end and last use are both `p` keeps its register through
`p` (`ls_select_register`), and `verify_allocation`'s proof 4 exempts exactly
that segment (it must end at `p`). The list is **empty by default** —
`for_graph` never fills it, because it is a property of the emitter (`cqo`
destroys RDX before `idiv` reads its divisor; a back edge's poll runs before
the edge's phi copies read their sources). `mark_call_operand_reads_first`
fills it with the clobbered call positions for a consumer that has audited its
call lowering. `mark_operand_reads_first_where` (r9 wave 4) is the filtered
form for a consumer that audited only some ops: `ir_lower` uses it behind
`CRATONVM_JIT_IR_LS_CALL_LAST_USE`, and mirrors the exemption in its second
opinion through the now-`pub(crate)` `MachineModel::read_before_clobber`.

**Loop depth (r9 wave 2).** `ls_loop_depths` now matches
`ir_schedule::loop_depths` exactly: one natural loop per **header** (the union
of all its latches), and no back edge out of an unreachable block. It used to
count one level per back edge (a two-latch `while` read two deep) and to treat
an edge out of dead code as a loop.

**Termination.** Every re-queue strictly advances the interval's start, so the
scan terminates regardless. `split_budget(intervals) = 4 × intervals + 16`
bounds *compile time* on top of that: a graph that needs more splits than this
is one where every promotion immediately costs a reload, i.e. one where linear
scan buys nothing, so it bails with `BailoutReason::RegisterPressure` rather
than spending the compile budget arriving at the frame layout `ir_lower` already
had.

## Who consumes this

`ir_lower::plan_register_residency` is the only production caller of
`allocate_linear_scan`, and what it does with the answer is what the machine
code actually is.

**Whole-range values** are the path that has always existed: one segment, one
register, published at the definition and read through `resident_gpr` /
`resident_xmm` for the value's whole life.

**Split values** are R4, behind `CRATONVM_JIT_IR_LS_SPLITS` (**default off**).
With it on, `reg_of` / `gp_reg_of` stop being a property of a live range and
become a property of a *position*: `ls_apply_transitions` re-points them
immediately before the node at each transition's position, so every existing
reader answers for the position the emission has reached rather than for the
value's whole life. `Allocation::events` is the input; `Allocation::segments` is
consulted alongside it, because there is no event for *"the register stops being
good"* — a `SpillKind::Store`'s `pos` is the last position at which it still is.

What that consumer emits, per kind:

| Kind | Emitted |
|---|---|
| `Store` | **nothing.** The backend writes every home word at the definition, and a managed value never has its home dropped, so the word already holds the value |
| `Load` | `mov to, [home(node)]` |
| `Remat` | `mov to, imm` for `Remat::Const`; `Remat::ConstF` and `Remat::Param` lower as a home load, because a true remat of either needs a GP scratch this backend does not have between two nodes |
| `Move` | `mov to, from` — or a home load when the source register is not currently readable, which can only *remove* a register read and so leaves the sequenced order valid |
| *(segment boundary into memory)* | nothing; the cached copy is simply dropped, so the next read takes the home word |

Several transitions at one position are a **parallel copy** and go through
`resolve_parallel_copy`, the same sequencer the phi edges use. A cycle would
need a scratch, and between two nodes this backend has none — RAX may be
carrying a value to the next node and RCX a deferred one — so a cycle refuses
split promotion for the whole method.

The refusals, each of which sends the value back to today's behaviour rather
than to a guess:

* a **phi** (its publish site is the edge copy, outside the per-node audit);
* a transition at a **terminator or edge position** (same reason: outside the
  audit `lower_block` runs after every node that is not `op_cannot_deopt`, which
  is the whole of the argument that a `Ref` in a register cannot outlive a point
  where a collector ran);
* a transition **at or before the definition** (liveness is widened to whole
  block spans, so a range can begin before the node that defines it, and a load
  from the home word there reads bytes nothing has written);
* a segment register outside the value's bank or the backend's file;
* the two liveness models disagreeing anywhere in the method — the whole-range
  second opinion cannot be taken for a split value, so it is replaced by the
  stronger fact that `plan_slots`' ranges and `build_live_model`'s coincide.

A managed value also **keeps its home word** and is **never named in a deopt
frame**: a frame state says "this value is in RBX" with no position attached,
and a split value is in RBX only sometimes.

### A split timeline is linear; the CFG is not (r9)

An `Allocation` is a timeline over **layout-order** positions, and the scan does
no edge resolution. When it splits a value, the location it records at the top
of a block is whatever the previous position in layout order had — the location
on the **fall-through** edge. On any other incoming edge (a loop back edge, a
join whose other predecessor is laid out elsewhere) the value can be somewhere
else:

```text
  loop top (pos 3..)   v in r0           <- carried in from the preheader
  body     (pos 6)     v evicted; r0 handed to w
  latch    (edge 9)    v in memory       -> back edge to pos 3
```

From the second iteration on, the loop's first positions read `v` from `r0`,
which holds `w`. A whole-range (single-segment) value cannot hit this. The
split-residency consumer above can, because it carries its residency state
across block boundaries in layout order too, and none of its refusals looks at
edges.

`split_edge_conflicts(schedule, live, alloc)` names the values affected: for
every non-fall-through edge `p → s` and every multi-segment value, a register
claim at `span[s].0 - 1` must be matched by the same register at `p`'s edge
position (a memory location at `s` is always consistent for a write-through
consumer). The consumer should refuse a flagged value exactly as it refuses a
transition at an edge position; that one-line gate is `ir_lower`'s (wave-1
cross-lane request A in `docs/internal/jit-review-r9/NOTES-regalloc.md`).

Two refinements landed in wave 2, both read through the emitter's eyes
(`ls_linear_state`): a position laid out **before the value's definition** is
"memory" whatever the timeline's register says (the emitter has published
nothing there — the wave-1 rule missed a predecessor laid out before the
definition), and a value the model knows is **not live-in** at `s`
(`LiveModel::live_in_at`) is not held against the edge.

**The resolution phase (r9 wave 2).** `split_edge_resolution(schedule, live,
alloc)` computes, from the SAME enumeration (so the two cannot drift), the
parallel copy each such edge needs: `Reg(want) ← Reg(r)` when the edge delivers
the value in another register, `Reg(want) ← Slot(home)` when it delivers it in
memory (correct for a write-through consumer, whose home word is written at the
definition, which dominates every block the value is live into). Copies target
the state at `span[s].0 - 1`, because a layout-order emitter then applies the
timeline's own events at `span[s].0`. Each edge carries a placement —
`PredEnd` (one successor), `SuccStart` (one predecessor) or `Critical` — and
its copies are in `phi_edge_copies`' `(destination, source)` convention so a
consumer sequences them together with the edge's phi copies through one
`resolve_parallel_copy`. A value is `unresolvable` (and every copy of it is
withdrawn) when the edge delivers it nowhere nameable, when a reload has no
home word, when two values would land in one register, or when the destination
register belongs to another value at the top of `s`.

What the consumer still owes before the gate can be relaxed: emit the copies at
its edge site (after the back-edge poll, sequenced with the phi copies), split
or refuse `Critical` edges, and drop copies for values it refuses for its own
reasons. That is `ir_lower`'s half; see the known-issues page.

## Bailing, never guessing

Every refusal is a structured `Bailout`; nothing in this module panics.

| Situation | Reason |
|---|---|
| Two values require the same register at the same position | `RegisterPressure` |
| One value requires two registers at one position | `RegisterPressure` |
| The split budget is exhausted | `RegisterPressure` |
| `verify_allocation` rejects the result | `Internal(...)` |

`Internal` is the right variant for a verifier failure: it is a compiler bug,
not a property of the input program. The method loses its optimized body; the VM
does not die.

> **Corrected 2026-09-18 (r9).** This table used to list *"a pinned register is
> clobbered inside the pinned value's range → `RegisterPressure`"*. Since R6 that
> is not a bailout: a `FixedConstraint` the allocator cannot honour is
> **demoted** — the value sits in its home word at the constrained position and
> the emitter loads the fixed register from there (see the comment at the `None`
> arm of `allocate_linear_scan`'s scan). Only two values requiring one register
> at one position, or one value requiring two, is still `RegisterPressure`.

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

**Two callers now, not one.** `ir_lower::emit_phi_copies` routes each edge's
gathered copies through `resolve_parallel_copy` and emits `Move` / `Save` /
`Restore` (the once-live bug where it emitted them in gather order, and a
swap loop's back edge left both locals holding `b`, is fixed). R4's
`ls_sequence_split_plan` is the second: the transitions at one linear position
are the same kind of simultaneous copy, and a load into a register another value
is still being moved out of has the same hazard as a move. The difference is
what each does with a cycle — an edge has RAX free and can take the `Save` /
`Restore` pair, while a point between two nodes has no free register at all and
refuses split promotion for the whole method instead.

## Metrics

> **Corrected 2026-09-18 (r9).** `record_allocation_metrics` no longer exists.
> It was deleted in R5 (it had no caller, and it would have written the PLANNED
> numbers into the same `Measured<u32>` the emitted ones feed); the reasoning is
> kept in the *Metrics* comment block of `regalloc.rs`. `CompilationReport` has
> since grown `planned_spills` / `planned_reloads`, so a producer of the planned
> pair would now be correct — it still has no caller, because the only holder of
> an `Allocation` (`ir_lower::plan_register_residency`) holds no recorder.

`ir_lower::lower_inner`'s signature is pinned and cannot take a recorder, so it
uses the *ambient* form — `metrics::note_current_spills` /
`note_current_reloads`, beside the older `note_current_peak_live_values`. Note
what it publishes: the transitions the backend **emitted**, not the ones the
allocation contains. Those are different numbers whenever a value the scan
promoted is one this file refused, and the emitted count is the one a
performance question is actually about.

R4 adds to `reloads` from `ls_apply_transitions`. A `Store` event adds to
neither, because the backend emits none — see *Who consumes this*.

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
* r9: an integer `Op::Rem` destroys no caller-saved register
  (`an_integer_remainder_is_not_a_call`); a required register is made room for
  by its holder only; a re-queued suffix is not stretched to an own constraint
  beyond it; a helper-produced `Ref` never receives a recycled home word; an
  interval that crosses no call prefers a volatile register; and
  `split_edge_conflicts` flags a value evicted inside a loop and not reloaded by
  the back edge, and never flags a single-segment value.
* r9 wave 2: `a_loop_with_two_latches_is_one_level_deep` and
  `an_edge_from_dead_code_does_not_make_a_loop` (ports of the `ir_schedule`
  twins); `a_last_use_at_a_declared_call_keeps_its_register` and
  `marking_call_operand_reads_lists_only_clobbered_call_positions`;
  `a_predecessor_laid_out_before_the_definition_delivers_memory`,
  `edge_resolution_repairs_exactly_the_flagged_values` (a reload, a register
  exchange sequenced as a real parallel copy, and an unresolvable squatter),
  `a_value_not_live_into_the_target_is_not_an_edge_conflict`,
  `a_converged_model_keeps_per_block_live_in_sets`; and
  `scalar_replacement_field_values_are_pinned_from_the_map`.

The **consumer** is tested in `jit/src/ir_lower.rs`, `mod tests`, under the
heading *R4: split residency*. Those fixtures hand-build the `Allocation` for
the same reason the ones here hand-build the `LiveModel`: what is under test is
the consumption of a split, and a test that had to coax the scan's heuristics
into splitting one particular value would stop testing the consumer the moment
those heuristics moved.

* a value live across a call keeps its register on **both** sides of the call
  and reads its home word only at the call itself;
* a reference the plan reloads is dropped again by `invalidate_ref_residency`,
  and its home word — the one the oop map names — is still written;
* a managed value keeps that home word and is never named in a deopt frame, with
  every switch that would otherwise drop the home turned on;
* two moves at one position are ordered so no source is read after it is
  written, a cycle refuses the whole plan, and two publishes into one register
  refuse it;
* a transition at an edge position, and one at or before the definition, are
  both refused;
* the prologue's save set is a snapshot and does not move when the value does;
* a move whose source is not readable falls back to the home word.
