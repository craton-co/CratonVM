# The lowering contract

**What this is.** The settled answer to `docs/known-issues/c2/hir-01`, which
asked how many IR levels this compiler should have, who owns the oop-map
obligation, what migration path keeps the tree green, and what the first
increment buys. It is prose and a migration plan. It deliberately contains no
crate skeleton, no trait hierarchy and no `mir/` directory.

**Every claim below was checked against the tree at `a9241eed` and the file and
line are given.** Where the tree contradicted the brief, the delta is recorded
in §8 rather than quietly worked around.

---

## 0. The answer in one page

**Four levels, not three, and three of them already exist.**

| # | Level | Artifact | Owns | May assume | Verified by |
|---|---|---|---|---|---|
| 0 | Bytecode | `&[u8]` + `JitScanResult` | Java semantics, exception ranges, the class file's own guarantees | the class-file verifier ran | the class-file verifier |
| 1 | IR | `ir::Graph` + `ir_schedule::Schedule` | value semantics, memory order, alias classes, deopt snapshots | every value is typed; every memory-effecting node carries a token | `ir_verify`, `ir_schedule::check_memory_order` |
| 2 | **Machine list — MISSING** | *(would be)* per-block `Vec<MInst>` + frame plan + safepoint records | instruction selection, register assignment, frame layout, safepoint publication | the schedule is final and the memory order will not change | `BlockSelection::covers`, `regalloc::verify_allocation`, `ir_lower::verify_data_locations` |
| 3 | Encoding | `isel::PATTERNS` + `x64::disp` | REX / ModRM / SIB / displacement / immediate width | it is handed physical registers and checked widths | 68 byte-for-byte tests against the hand-written emitters |

The report asked for HIR / LIR / MIR. **The "H" already exists and it is the
bytecode** — it has a specification, a verifier, and three independent consumers
in this tree (`ir::IrBuilder::build`, the `x64.rs` single-pass backend, and
`regalloc::allocate_registers_with_handlers`, which allocates registers directly
over `&[u8]`). Nothing in the tree wants to see `invokevirtual` in a form more
abstract than `ir::Op::Call`, and no pass asks for one.

So the real finding is not "one level too few". It is:

> **Level 2 is not missing as a design; it is missing as an artifact.** Its
> three jobs — select, allocate, encode — each have a finished, tested component
> in the tree, and all three are unwired for the same single reason: there is no
> value in this compiler that means *"an instruction whose operands are values,
> not addresses"*. Every one of them dead-ends on that.

Evidence, three components, one cause:

* `jit/src/x64/isel.rs` (6 848 lines, 67 pattern rows, 68 passing tests) has a
  block tiler that produces exactly that value — `Vec<MInst>` with `NodeId`
  operands (`isel.rs:3368`). **No caller.** The only mention of `isel::` outside
  the file is a comment (`x64.rs:133`).
* `jit/src/regalloc.rs::allocate_linear_scan` (`regalloc.rs:4749`) produces an
  assignment over IR values and `verify_allocation` (`regalloc.rs:5454`) proves
  it. Its one production consumer (`ir_lower.rs:7051`) uses it as a
  **write-through read cache**, because the encoder cannot accept a register
  location: `ir_lower::frame_word_off` returns `Err` for `ValueLoc::Reg`
  (`ir_lower.rs:5088`), with the comment "this backend keeps every value in a
  frame word".
* `jit/src/x64/vec_emit.rs::emit_vector_loop` (`vec_emit.rs:1095`) has **no
  caller either**, and carries its own private XMM0–XMM5 pool
  (`vec_emit.rs:514`) precisely because there is no shared place to put a
  register assignment.

Three finished components, three private answers to "where does this value
live", nothing joining them. That is the gap, stated concretely.

**And the migration is not justified by defect prevention.** §6 runs the
three-defect test hir-01 specifies and the honest score is **one of three**.
The recommendation in §5 is therefore conditional on a measurement, not on
safety.

---

## 1. What `ir_lower` actually does

The brief says `ir_lower` is "simultaneously doing instruction selection,
register assignment, frame layout, safepoint publication and encoding". That is
correct, and understated. Re-derived from `lower_inner_with_scopes`
(`ir_lower.rs:6821`), in the order it runs:

1. **Admission** — five separate refusals on graph shape (monitor helper absent,
   `new_object` helper absent, compact-layout field helpers absent, `Load(Ref)`
   helper absent, ABI incoming-slot capacity). `ir_lower.rs:6845–7026`.
2. **Resource bounds** — node count, then frame bytes. `ir_lower.rs:6926`.
3. **Frame layout** — `plan_slots` colours live ranges onto 8-byte frame words,
   `verify_slot_colouring` checks the colouring. `ir_lower.rs:5551`, `:5944`.
4. **Location verification** — `verify_data_locations` refuses the compile if
   any emitted read names a value whose lowering assigns no slot.
   `ir_lower.rs:5216`.
5. **Buffer sizing** — `ir_code_buffer_estimate`. `ir_lower.rs:6555`.
6. **Register residency (optional)** — `plan_register_residency`.
   `ir_lower.rs:6255`.
7. **Emission** — prologue, then per block, per node: select, assign scratch
   registers, encode bytes, publish safepoints, record deopt points, emit phi
   copies. `ir_lower.rs:2298`, `:2933`, `:3932`.
8. **Post-emission soundness nets** — latched bailout, shadow push/reload
   balance, branch patching, deopt stub, install verification.

Steps 3, 4 and 6 are *already* level-2 work, performed as separate passes over
level-1 data. **A large part of level 2 exists; what does not exist is a value
that carries its result forward.** They hand their results to the emitter as
side tables (`slot_plan`, `RegResidency`) that only the emitter reads.

### The vocabulary problem, measured

`ir::Op` has **53 variants**. `lower_data_node` names **48** of them.
The five with no arm — `ArrayLength`, `I2B`, `I2C`, `I2S`, `NewArray` — fall
into the catch-all at `ir_lower.rs:3928`:

```rust
// Unhandled — skip (bail in ir_compatible prevents reaching here)
_ => {}
```

`op_defines_result_slot` (`ir_lower.rs:5133`) is a **hand-maintained second copy**
of that match's arm list — its doc says "Must stay in step with
`lower_data_node`'s match arms". It names 37 ops. A third enumeration,
`ir::ir_compatible` (`ir.rs:6066`), decides admission — and it is written in a
*different vocabulary*: it inspects bytecode opcodes (`scan.anewarray_ops`,
`scan.typecheck_ops`, `scan.has_athrow`), not `Op` variants.

Cross-checking the three lists by hand today: they agree. No op claims a result
slot without a lowering arm, and every op with an arm but no slot is a control
or void node (`Store`, `MonitorEnter`, `MonitorExit`, `Guard`, `Start`,
`Return`, `If`, `Merge`, `Region`, `Proj`, `Dead`). **Nothing in the tree checks
this.** It was checked by running a script for this document.

That is the whole structural argument in one paragraph: three enumerations of
the same set, two of them in the emitter's vocabulary and one in the bytecode's,
kept in agreement by a comment.

---

## 2. Question 1 — how many levels, and what may each assume?

### Level 0 — bytecode

Owns Java semantics. May assume the class-file verifier's guarantees. Its
consumers are `ir::IrBuilder::build` (`ir.rs:4122`), the `x64.rs` single-pass
backend, and `regalloc::allocate_registers_with_handlers` (`regalloc.rs:1645`).

**Nothing should be built above this.** A "HIR" between bytecode and `ir::Graph`
would have exactly one producer and one consumer and would carry no invariant
the verifier does not already provide.

### Level 1 — `ir::Graph` + `Schedule`

Owns value semantics, memory ordering, alias classes and deopt snapshots. May
assume: every value node has an `IrType`; every memory-effecting node carries a
memory token at the slot `ir::Op::memory_shape` declares.

That single-table registration is why the monitor ops could be added without a
fourth copy of the token convention: `memory_token_slot`,
`is_memory_token_slot` and `ir_verify::is_memory_token_input` all read it
(`ir_optimize.rs:1754`). It is the model for what a level's invariant looks like
when it is stated once.

**This level should not be split.** Every optimizing pass in the tree —
`ir_optimize`, `escape_analysis`, `range_analysis`, `null_check_elim`,
`loop_analysis`, `scev`, `ir_schedule` — reads and writes it, and not one of
them asks for a lower form. Splitting it would create a boundary with no
customer.

### Level 2 — the machine list (the one that is missing)

Owns instruction selection, register assignment, frame layout and safepoint
publication. **May assume the schedule is final** — that is the load-bearing
assumption, and it is the reason this level can exist at all:
`ir_schedule::check_memory_order` (`docs/jit/instruction-selection.md` §8)
already establishes that the emitted order is a justified permutation of the
baseline, so level 2 never has to re-ask a memory-model question.

Its entry invariant is already written and already tested:

> `BlockSelection::covers(block)` — every scheduled node covered **exactly
> once**. A node covered twice is computed twice; a node covered zero times is a
> dropped instruction. `isel.rs:3784`.

Its exit invariant is `regalloc::verify_allocation` plus, for anything that
reaches a safepoint, §3's rule.

### Level 3 — encoding

Owns nothing but the encoding arithmetic. Already exists as `isel::PATTERNS` +
`x64::disp`, and it is the only level in this compiler with a *mechanical*
correctness oracle: every row names the `x64.rs` emitter it reproduces and the
tests assert byte-for-byte equality across a register matrix including R8–R15
and the RSP/RBP/R12/R13 addressing special cases. Re-run for this document on
Azure: **68 passed, 0 failed** (`cargo test -p cratonvm-jit --lib isel`).

### Why four and not three

Because the fourth boundary — level 2 / level 3 — is the only one in this
compiler that can be checked by comparing bytes, and that is worth more than the
tidiness of collapsing it. §5's whole migration plan rests on it.

---

## 3. Question 2 — where the safepoint / oop-map obligation lives

**It lives at level 2, and it is discharged by the component that assigns
registers.** This is not a design preference; the tree has already settled it
twice, in two different backends, and the two answers agree.

### The consumer's shape, and the asymmetry nobody states

There are **two** consumers of "where does this value live at this program
point", and they do not have the same power:

| Consumer | Can read a register? | Where |
|---|---|---|
| GC root walk / relocation | **No** | `OopMapEntry { frame_slot_offsets: Vec<i16>, … }` — `lib.rs:900`; consumed by `remap_one_jit_frame`, `conservative_roots.rs:2919` |
| Deopt frame reconstruction | **Yes** | `FrameValue::Register` / `RegisterLong` / `RegisterRef` / `XmmFloat` / `XmmDouble` — `deopt.rs:123`, resolved against `SavedRegisters { gpr: [u64;16], xmm: [u64;16] }`, `deopt.rs:1417` |

The asymmetry is not an oversight and must not be "fixed" by symmetry. Deopt
can read a register because the deopt stub *voluntarily spilled the whole
register file* before calling the runtime. A GC safepoint has no such spill: the
collector walks a frame it did not stop, through RBP, from the outside. `x64.rs`
says it in as many words — the `reg_oops` register bitmap is "a TODO with no
field, no producer and no consumer anywhere in the workspace" (`x64.rs:3458`).

So a register-resident reference at a *deopt point* is representable and safe. A
register-resident reference at a *GC safepoint* is a root the collector cannot
see, and — under moving young — a pointer it cannot rewrite.

### The rule

> **No reference may be register-resident at a GC safepoint.** The level that
> hands out registers owns proving it, and discharges it by spilling to the
> value's frame home before the safepoint.

Both existing backends satisfy this, by different means, and the difference is
instructive:

* **`x64.rs` single-pass — conditional, with a named predicate.** Java locals
  *are* register-homed here: `LOCAL_REGS` is RBX/R12–R15 everywhere, plus
  RSI/RDI on Windows where those two are callee-saved (`x64.rs:256`, `:273`).
  The pre-safepoint spill is elided only when
  `SafepointPublishPlan::no_reference_in_registers()` holds (`regalloc.rs:1541`,
  argued at `x64.rs:3453`), which is computed from
  `regalloc::find_reference_locals` unioned with the parameter oop mask — a
  method-wide, deliberately over-approximating scan. The five-point argument at
  `x64.rs:3457–3483` is the best statement of this obligation in the tree and
  should be read before anyone touches it.
* **`ir_lower` — structural, by having no GP registers to give.** The linear-scan
  file is XMM2–XMM5 and `RegClass::of(IrType::Ref)` is `Some(Gp)`
  (`regalloc.rs:3526`), so a `Ref` cannot be promoted at all. Write-through
  then keeps the frame image complete at every
  instruction boundary, so `emit_safepoint_map` is byte-identical to the
  colourer-only path (`docs/jit/linear-scan-wiring.md` §Safepoints).

### What this means for a level-2 artifact

1. **Do not change the map format for the first increment.** Keeping references
   memory-pinned costs one store per reference definition and is what both
   production backends already do. Changing `OopMapEntry` means changing
   `conservative_roots.rs` (`vm/`, a different crate, outside any JIT lane's
   file ownership) and the moving-young relocation contract at the same time.
2. **The level-2 artifact must carry the safepoint records**, not compute them
   afterwards from emitted bytes. Today `emit_safepoint_map` builds the slot
   list by scanning `defined_nodes` for `IrType::Ref` at the moment of emission
   (`ir_lower.rs:1121`) — which is only correct because emission and slot
   assignment are the same pass. Split them and that scan becomes a third
   liveness model. It must instead be a field of the level-2 form, produced by
   whoever assigned the locations.
3. **The `coverable` / `published` distinction is the model to copy.**
   `emit_safepoint_map` publishes an *empty* slot list for two different reasons
   and refuses to conflate them: a genuinely oop-free safepoint sets
   `moving_young_coverage_complete: true`, a slot it could not describe sets it
   `false` and diverts that cycle to the non-moving sweep (`ir_lower.rs:1104`,
   `:1175`). A level-2 form must have the same three-valued shape — *covered*,
   *nothing to cover*, *cannot describe* — and not an `Option`.

---

## 4. What a first-class level-2 form would have to carry

Not a proposal for a type; a checklist of what the emitter reads today that a
side table does not carry. Anything on this list that a level-2 form omits comes
straight back as a second copy of the emitter.

| Emitter needs | Today comes from | Level-2 owner |
|---|---|---|
| the instruction and its operands | `lower_data_node`'s match arm | `MInst` — exists, `isel.rs:3368` |
| which nodes a tile absorbed | nothing (no tiling) | `Tile::covered` — exists, `isel.rs:3636` |
| each value's frame word | `slot_plan` side table | frame plan |
| each value's register, if any | `RegResidency` side table | allocation |
| live reference slots per safepoint | scan at emission time | safepoint record |
| deopt frame values per bci | `resolve_frame_values`, at emission time | deopt record |
| phi edge copies | `emit_phi_copies`, at emission time | edge copy list |
| call marshalling / IC cascade shape | `emit_inline_cache_call` etc. | **stays at level 2 as `Generic`** |

The last row is the one that makes the increment tractable. `MInst::Generic
{ node }` (`isel.rs:3433`) means "no rule matched; `ir_lower` lowers this node
the way it always did" — and it is documented as **not** a no-op: "A selector
that answered an unmatched node with silence would drop the node's semantics
entirely; this arm names the node the caller must still lower."

That single variant is what makes this a migration rather than a rewrite.

---

## 5. Question 3 — the migration path, and Question 4 — the first increment

### Does the linear-scan lane's shape generalise?

**Yes, and the cross-check gets strictly stronger.** The linear-scan lane's
shape was: new path behind a declared flag, cross-checked against the existing
path, verifying its own output, bailing on mismatch. Two differences matter:

* Linear scan was **additive**. Write-through meant the new path's frame image
  was identical to the old one, so "cross-check" meant checking a *plan*
  (`verify_allocation`, plus re-running the aliasing and clobber questions
  against `plan_slots`' independent ranges). It could never compare output,
  because there was no second output.
* Selection is **substitutive**. The new path emits different bytes. So the
  cross-check cannot be "the plan verifies" — it must be "the bytes agree", and
  for the first increment **they can be made to agree exactly**, because the
  pattern table's discipline is that every row reproduces a specific
  hand-written emitter byte-for-byte. `the_new_alu_rows_reproduce_the_ir_lower_byte_literals`
  already asserts this per row for the seven ALU rows.

So the generalisation is: same shape, but the oracle is byte equality instead of
plan verification, and the granularity is **per node, not per method**.

### The increments

**Increment 0 — shadow selection. Emits nothing. Off by default.**

Run `select_block` per block inside `lower_inner_with_scopes`, assert
`BlockSelection::covers`, count what fired, discard the result, and emit through
the existing path unchanged. Zero emitted-byte difference by construction.

What it produces that is independently useful, *even if levels never split*:

1. **The coverage number nobody has.** Measured for this document over a
   ten-shape corpus of integer/branch/loop methods (probe in the appendix, run
   on Azure at `a9241eed`):

   ```
   scheduled data nodes           : 55
   tiles                          : 67
   tiles firing a real rule       : 18
   nodes covered by a real rule   : 21 (38.2%)
   rule histogram : {AluReg: 14, CmpBranch: 2, Generic: 49, Lea: 1, TestZeroBranch: 1}
   refusal notes  : {Unencodable/AluImm/UnknownPattern: 4}
   ```

   Read that last line. **`Rule::AluImm` fires zero times** on this corpus —
   every attempt is refused because the table has no matching row. The immediate
   rule, on Java `int` code, is dead. `Rule::AluFoldedLoad` also never fires
   (`SelectOptions::fold_loads` is off), nor does `CmpSetCc`, nor `TestBranch`
   (every branch fused). Two rules and one option carry the whole result.

   That converts `docs/jit/instruction-selection.md` §6's hand-written
   "what is still unvalidated" list from a reading into a measurement, and it
   ranks it: 32-bit rows first.

2. **`covers()` running on production graphs.** That is the monitor defect's
   detector (§6.1), running on every compile, at the cost of a tiling nobody
   emits.

3. **A refusal channel.** `Note::Unencodable { root, rule, why }` names, per
   method, exactly which table row is missing. Today that information exists
   only in a doc section maintained by hand.

**Increment 1 — emit one rule, byte-identical.** Pick `Rule::AluReg` (14 of the
18 firing tiles above). Its seven rows are already anchored to the exact byte
literals `lower_data_node` emits. The gate is byte equality on a corpus: compile
each method both ways, compare `ExecutableBuffer` contents, bail on any
difference. Off by default until the corpus is clean.

This is where the increment stops being cosmetic: it is the first node in this
compiler emitted from a *declarative* description, and the check is exact rather
than behavioural.

**Increment 2 — the 32-bit rows.** Ranked first by increment 0's measurement,
not by intuition. `Rule::Lea` refusing `Ty::I32` and `AluImm` having no `r32`
rows are the same gap and most Java arithmetic is `int`.

**Increment 3 — registers.** Only here does `Allocation` stop being a read
cache. The prerequisite is the prologue, not the allocator:
`ir_lower::emit_prologue` saves no callee-saved register, so RBX/R12–R15 cannot
be handed out until there is a save area restored on all three exits
(`emit_epilogue`, the inlined epilogue in `emit_deopt_stub`, `emit_call_exc_stub`)
placed at or above `callee_saved_lo` so the conservative band scan does not read
a caller's register as a root. §3's rule binds from this increment onward.

**Increment 4 — unify the vector pool.** `vec_emit`'s private XMM0–5 pool
(`vec_emit.rs:514`) overlaps `ir_lower`'s FP value tier (XMM0/XMM1) *and* the
linear-scan file (XMM2–XMM5). It is currently harmless only because
`emit_vector_loop` has no caller. Whoever wires either one owns the
unification — `docs/jit/vectorization-emitter.md` names it as the single
highest-risk prerequisite, and the failure is a scalar FP value silently
destroyed across a vector region.

### The ordering claim

Increments 0 and 1 are worth doing on their own merits and do not commit anyone
to 2–4. **Increment 0 is the decision point**: if the coverage measured on real
suite methods is small, or the rules that fire are ones `ir_lower` already emits
optimally, the honest answer is to stop, keep the number in the doc, and spend
the effort on `pgo` or `loop` instead.

---

## 6. The three-defect test

hir-01 asks whether the contract makes three real defects **unrepresentable**
rather than merely less likely, and says a design that only achieves the latter
is not worth the migration cost. Scored honestly: **one of three.**

### 6.1 The monitor op that lowered to nothing — **unrepresentable. ✓**

Today `lower_data_node` ends in `_ => {}` (`ir_lower.rs:3928`), reachable for 5
of 53 `Op` variants. `verify_data_locations` catches most of the damage, but
only for ops whose result is *read*: it walks each node's inputs and refuses
when a value-typed input's op is absent from `op_defines_result_slot`
(`ir_lower.rs:5263`). **A monitor produces no value.** Nothing reads it, so
nothing checks it — which is exactly why this op and not another was the one
that could compile to silence. The tree's answer was a hand-written guard at the
top of `lower_inner_with_scopes` (`ir_lower.rs:6859`), one op-specific check
added after the fact.

At level 2 this shape cannot be written. `select_block` returns a tile for every
node — `Tile::generic(root)` when nothing matches — and `BlockSelection::covers`
asserts the block's node set equals the union of `covered`. Emitting nothing for
a node requires *removing* it from the block, which `covers()` then rejects.
"No rule matched" is a value (`MInst::Generic`), not a fall-through. The witness
is already written and already passing: `an_unmatched_monitor_op_is_never_silently_dropped`
(`isel.rs:6625`) drives this exact op through the tiler.

This is the strongest form of the argument the brief asked for: not "we would
have noticed", but "the state has no representation".

### 6.2 Escape analysis forwarding a load to a later store — **not prevented. ✗**

`ScalarReplacementInfo::field_values[f]` held the *last* store to field `f`, and
the applier forwarded every replaced load to it, so
`Foo o = new Foo(); int a = o.x; o.x = 42;` folded `a` to `42` — a value from
the load's own future. The branchy variant was worse: with
`if (c) o.x = 1; else o.x = 2; int a = o.x;` no store has a higher node id than
the load, so the id-comparison guard did not fire either.

**No lowering contract touches this.** It is a level-1 analysis defect: the
analysis produced a per-field answer to a per-load question. A machine level
below it would have faithfully emitted the wrong value.

What did fix it is worth naming, because it is the *same principle* at a
different level: `LoadResolution` is three-valued — `Value(n)`, `ZeroDefault`,
`Unknown` — and the doc states why, exactly: "Collapsing `ZeroDefault` and
`Unknown` into one `Option<NodeId>` is exactly how a load-before-store gets a
value from its future." Making "I cannot answer" a representable state is what
made the miscompile unrepresentable. The applier now consumes it
(`lib.rs:10353`) and refuses on `Unknown`.

Level splitting is one way to buy that property. It is not the cheap way.

### 6.3 The dead-store location key — **not prevented. ✗**

Two stores to different `int` fields of the same object shared the location key
`(base, NO_NODE, kind)` whenever their `MemKind` widths agreed, so
`o.x = 1; o.y = 2;` could delete the live `o.x = 1`. Also level 1. Also fixed by
making the unnameable case refusable rather than guessable:
`store_matchable_location` (`ir_optimize.rs:1695`) admits a store only when the
offset is a real node or the allocation provably has a single field, and its doc
records that the production builder always emits the distinct-offset form — "so
this refusal costs nothing today; it removes the premise, which is the part that
was not established."

### What the score means

The migration is **not** justified as a correctness investment. One defect class
in three, and that class already has a cheaper local remedy (§7). The three
defects agree on a different lesson, and it is the one to carry forward:

> Every one of them was a **silent default** where a refusal belonged — a
> catch-all arm, an `Option` that conflated "zero" with "unknown", a key that
> conflated "no offset" with "offset zero". None of them was a missing level.

Justify the migration on §5's coverage measurement, or not at all.

---

## 7. What to do instead, if increment 0 says stop

Two cheap changes get most of §6.1's benefit without any of the migration, and
they belong to whoever owns `ir_lower.rs` regardless of what the HIR lane does:

1. **Make the three op enumerations agree mechanically.** A test that asserts
   every `ir::Op` variant is either named in `lower_data_node`'s arms or listed
   in an explicit `UNLOWERABLE` constant, and that `op_defines_result_slot`'s
   set equals the value-producing subset of the first. The lists agree today —
   verified by script for this document, not by any test. The exact edit that
   would trip it: add an `Op` variant and no arm.
2. **Replace `_ => {}` with a refusal.** The catch-all's own comment says "bail
   in `ir_compatible` prevents reaching here", and `ir_compatible` is written in
   the bytecode's vocabulary, not `Op`'s (`ir.rs:6066`). Turning the arm into a
   `refuse(Bailout::UnsupportedShape)` makes that claim checked instead of
   asserted, and costs nothing when it is true.

Neither is this lane's to write. Both are recorded here because the contract's
own analysis is what identified them, and because a reader who takes only §6
away should take these too.

---

## 8. Premise deltas — what the brief and the neighbouring docs got wrong

Per `docs/known-issues/c2/README.md` rule 1, reported before anything else was
decided.

| Claim | Source | Actual |
|---|---|---|
| "`isel.rs` … has never been compiled … its tests have never run" | `docs/jit/instruction-selection.md` §0 | **Stale.** `pub mod isel;` is at `x64.rs:136`; 68 tests pass. Corrected in that file as part of this lane. |
| "`ir_lower.rs` (~10.6k lines)" | hir-01 | 11 138 lines at `a9241eed`. |
| "the applier `apply_ea_to_ir` still reads `field_values`… correct but pessimistic" | `docs/jit/escape-analysis.md` §3, §6.1 | **Stale.** `plan_scalar_replacement` consumes `info.load_value` and refuses on `Unknown` (`lib.rs:10353`); the end-to-end witness is `ea_load_before_a_later_store_forwards_the_pre_store_value` (`lib.rs:17228`). Corrected in that file as part of this lane. |
| "the single level is why `ir_lower`'s catch-all was able to compile a monitor op to *nothing*" | hir-01 | True, but the mechanism is narrower and worth stating: `verify_data_locations` *does* catch an unlowered op — for ops whose result is read. The monitor slipped through because it produces no value (§6.1). |
| "no register bank in the oop map — which is why references are pinned to memory" | hir-01 | True for GC. **Not true for deopt**, which has a full register bank (`FrameValue::Register*`, `SavedRegisters`). The asymmetry is deliberate and load-bearing (§3). |
| implied: `isel` is the only unwired component | hir-02 | `vec_emit::emit_vector_loop` has no caller either, and for the same reason. |

---

## Appendix — the coverage probe

Not committed: hir-01's output is prose, and a measurement instrument is not
lane output. Reproduce by writing this to `jit/tests/hir01_probe.rs` and running
`cargo test -p cratonvm-jit --test hir01_probe -- --nocapture`.

It builds each shape's bytecode through `ir::IrBuilder`, schedules it, runs
`isel::select_block` per block with `SelectOptions::default()`, asserts
`covers()`, and histograms the rules that fired and the refusals. The corpus is
integer arithmetic, bitwise, shifts, compare-and-branch, a counted loop and an
address-shaped expression — i.e. the population `isel`'s rules target. It
deliberately contains no field access: `getfield` needs a constant pool the
builder is not given here, and `AddrSource::Opaque` means the selector would
decline the address half anyway.

```rust
use cratonvm_jit::ir::IrBuilder;
use cratonvm_jit::ir_schedule;
use cratonvm_jit::x64::isel::{self, Rule, SelectOptions};

fn shapes() -> Vec<(&'static str, usize, usize, Vec<u8>)> {
    vec![
        ("arith_chain",      3, 3, vec![0x1a, 0x1b, 0x60, 0x1c, 0x68, 0xac]),
        ("add_imm8",         1, 1, vec![0x1a, 0x10, 0x07, 0x60, 0xac]),
        ("add_imm16",        1, 2, vec![0x1a, 0x11, 0x30, 0x39, 0x60, 0xac]),
        ("bitwise",          2, 2, vec![0x1a, 0x1b, 0x7e, 0x1a, 0x1b, 0x82, 0x80, 0xac]),
        ("shift",            1, 2, vec![0x1a, 0x08, 0x78, 0xac]),
        ("cmp_branch",       2, 2, vec![0x1a, 0x1b, 0xa2, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac]),
        ("test_zero_branch", 1, 1, vec![0x1a, 0x9a, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac]),
        ("counted_loop",     1, 3, vec![
            0x03, 0x3c, 0x03, 0x3d,
            0x1c, 0x1a, 0xa2, 0x00, 0x0d,
            0x1b, 0x1c, 0x60, 0x3c,
            0x84, 0x02, 0x01,
            0xa7, 0xff, 0xf4,
            0x1b, 0xac,
        ]),
        ("long_arith",       4, 4, vec![0x1e, 0x20, 0x69, 0x1e, 0x61, 0xad]),
        ("lea_shape",        4, 4, vec![
            0x1a, 0x05, 0x68, 0x1b, 0x06, 0x78, 0x60, 0x1c, 0x60, 0x1d, 0x60, 0xac,
        ]),
    ]
}

#[test]
fn hir01_selection_coverage_over_a_corpus() {
    let (mut tot_nodes, mut tot_covered, mut tot_tiles, mut tot_matched) = (0, 0, 0, 0);
    let mut rule_hist: std::collections::BTreeMap<String, usize> = Default::default();
    let mut note_hist: std::collections::BTreeMap<String, usize> = Default::default();

    for (name, np, nl, code) in shapes() {
        let Some(graph) = IrBuilder::new(np, nl).build(&code, code.len()) else {
            println!("{name:>18}: BUILDER BAILED");
            continue;
        };
        let sched = ir_schedule::schedule(&graph);
        let (mut nodes, mut covered, mut tiles, mut matched) = (0, 0, 0, 0);
        for b in &sched.blocks {
            let sel = isel::select_block(&graph, &b.nodes, b.terminator, &SelectOptions::default());
            assert!(sel.covers(&b.nodes), "{name}: every scheduled node covered exactly once");
            nodes += b.nodes.len();
            tiles += sel.tiles.len();
            for t in &sel.tiles {
                *rule_hist.entry(format!("{:?}", t.rule)).or_default() += 1;
                if t.rule != Rule::Generic {
                    matched += 1;
                    covered += t.covered.len();
                }
            }
            for n in &sel.notes {
                let k = match n {
                    isel::Note::Address { why, .. } => format!("Address/{why:?}"),
                    isel::Note::Fold { why, .. } => format!("Fold/{why:?}"),
                    isel::Note::Unencodable { rule, why, .. } => format!("Unencodable/{rule:?}/{why:?}"),
                    isel::Note::WideImmediate { .. } => "WideImmediate".to_string(),
                };
                *note_hist.entry(k).or_default() += 1;
            }
        }
        println!(
            "{name:>18}: nodes={nodes:<3} tiles={tiles:<3} matched_tiles={matched:<3} \
             nodes_covered_by_a_rule={covered:<3} ({:.0}%)",
            if nodes == 0 { 0.0 } else { 100.0 * covered as f64 / nodes as f64 }
        );
        tot_nodes += nodes; tot_covered += covered; tot_tiles += tiles; tot_matched += matched;
    }
    println!("scheduled data nodes         : {tot_nodes}");
    println!("tiles                        : {tot_tiles}");
    println!("tiles firing a real rule     : {tot_matched}");
    println!("nodes covered by a real rule : {tot_covered} ({:.1}%)",
             100.0 * tot_covered as f64 / tot_nodes.max(1) as f64);
    println!("rule histogram               : {rule_hist:?}");
    println!("refusal notes                : {note_hist:?}");
}
```

**Its limits, stated so nobody over-reads it.** The corpus is ten synthetic
shapes, not suite methods; `select_block` is called on the *unoptimized* built
graph, so `ir_optimize` has not run; and `Rule::AluFoldedLoad` cannot fire
because `fold_loads` is off by default. Increment 0 exists precisely to replace
this with the same measurement taken over real compiles.

---

## What this contract refuses

* A `mir/` directory, a crate skeleton or a trait hierarchy. Level 2's first
  artifact is a `Vec<MInst>` and two side tables that already exist.
* A level between bytecode and `ir::Graph`. It would have no customer.
* Splitting `ir::Graph`. Every optimizing pass reads it and none asks for less.
* Changing `OopMapEntry` before increment 3. The consumer is in `vm/`, and the
  moving-young relocation contract depends on the current shape.
* Any pattern row not anchored to a byte literal an existing emitter produces.
  That anchoring is the table's one trustworthy property and the only reason
  §5's byte-equality oracle exists.
* A big-bang conversion of the 90 GP memory accesses in `ir_lower`. That is the
  "stop treating the frame as the value's identity" change and it is the whole
  item, not an increment (`docs/jit/linear-scan-wiring.md`).

---

## See also

* `docs/jit/instruction-selection.md` — level 2's selector and level 3's table.
* `docs/jit/instruction-patterns.md` — the pattern table's own migration order.
* `docs/jit/linear-scan-wiring.md` — the write-through consumer, and the shape
  §5 generalises from.
* `docs/jit/linear-scan-regalloc.md` — what `verify_allocation` proves.
* `docs/jit/vectorization-emitter.md` — the private XMM pool, increment 4.
* `docs/jit/escape-analysis.md` §3 — the three-valued-answer principle §6.2
  turns on.
* `docs/known-issues/c2/hir-02-mir-regalloc-handoff.md` — the lane this unblocks.
