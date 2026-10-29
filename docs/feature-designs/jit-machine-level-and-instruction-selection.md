# A machine level for the JIT, and the instruction selector that needs one

**Status:** Partial — the encoder level is built and anchored; the machine
level (level 2) is not, and every mode that would affect production output is
default-off.

## What is built

There are **four** compiler levels here, not three, and naming the missing one
is most of the value of this document.

- **Level 3, the encoder, is solid.** `jit/src/x64/isel.rs` carries a
  `PATTERNS` table with `Pattern`/`Op`/`Ty`/`OpKind`/`ImmForm`/`DispPolicy`/
  `Constraint`/`Cost`/`Enc`, a `select()` entry point, and byte-for-byte
  anchoring tests against the hand-written emitters in `jit/src/x64.rs`.
- **Tiler rules exist**, including `Rule::Lea` and `Rule::AluImm`, with a cost
  function.
- **The XMM half of the register-class work landed** (scalar file XMM2–XMM7,
  vector pool XMM8–XMM15).

Three modes exist to exercise it, all default-off, declared in
`jit/src/ir_lower.rs`:

| Flag | What it does |
|---|---|
| `CRATONVM_JIT_IR_ISEL_SHADOW` | tile and count, discard — emits nothing |
| `CRATONVM_JIT_IR_ISEL_VERIFY` | build the level-2 list and byte-compare it against the per-opcode arms — emits nothing |
| `CRATONVM_JIT_IR_ISEL_EMIT` | the only mode that emits, and it is fail-closed: an uncovered block, a tile-slot disagreement or an encoder refusal discards the artifact and drops the method to a lower tier |

`CRATONVM_DBG_IR_ISEL` reports.

## What is not built yet

- **Level 2 — the machine list** (`Vec<MInst>` plus a frame plan and safepoint
  records) does not exist. That is the level the selector needs, and its
  absence is why the selector cannot be the production path.
- **The GP register class is deliberately unbuilt.** It carries a safepoint
  obligation the XMM side does not.

## What the measurement said, and why the hold is the result

Shadow selection measured the tiler's real coverage on production compiles at
**15.7–19.0%**, with the two rules the migration was *for* firing **zero**
times. Closing the immediate half moved it to **19.3%**. Adding one anchored
`lea_r32_m` row took `Rule::Lea` from zero to **26 tiles** on a
framework-shaped corpus.

`AluImm`'s near-zero turned out not to be about pattern rows at all: **a
safepoint snapshot pins almost every live constant**, so a tile cannot absorb
it, and the cost model then compares a two-byte `ADD EAX, ECX` against a
three-byte `ADD EAX, 7` *without pricing the frame load the register form also
pays*. The next step is therefore more 32-bit pattern rows and a cost model
that prices the frame load — not a machine level.

## Goal

The C2 report asked for three IR levels. The real question underneath it: the
JIT has three finished, tested components that no production path can use, and
they all fail for the same reason. Decide whether closing that gap is worth its
cost — with a measurement, not an argument.

---

## Current state

### 1. There are four levels, and three of them exist

| # | Level | Artifact | Owns | May assume | Verified by |
|---|---|---|---|---|---|
| 0 | Bytecode | `&[u8]` + `JitScanResult` | Java semantics, exception ranges | the class-file verifier ran | the class-file verifier |
| 1 | IR | `ir::Graph` + `ir_schedule::Schedule` | value semantics, memory order, alias classes, deopt snapshots | every value is typed; every memory-effecting node carries a token | `ir_verify`, `ir_schedule::check_memory_order` |
| 2 | **Machine list — MISSING** | *(would be)* per-block `Vec<MInst>` + frame plan + safepoint records | instruction selection, register assignment, frame layout, safepoint publication | the schedule is final | `BlockSelection::covers`, `regalloc::verify_allocation`, `ir_lower::verify_data_locations` |
| 3 | Encoding | `isel::PATTERNS` + `x64::disp` | REX / ModRM / SIB / displacement / immediate width | it is handed physical registers and checked widths | 68 byte-for-byte tests against the hand-written emitters |

**The report's "HIR" already exists and it is the bytecode.** It has a
specification, a verifier, and three independent consumers in this tree:
`ir::IrBuilder::build` (`ir.rs:4122`), the `x64.rs` single-pass backend, and
`regalloc::allocate_registers_with_handlers` (`regalloc.rs:1645`), which
allocates registers directly over `&[u8]`. Nothing wants to see `invokevirtual`
in a form more abstract than `ir::Op::Call`.

**Level 1 should not be split either.** Every optimizing pass — `ir_optimize`,
`escape_analysis`, `range_analysis`, `null_check_elim`, `loop_analysis`, `scev`,
`ir_schedule` — reads and writes it, and none asks for a lower form. Splitting
it creates a boundary with no customer.

**Level 3 is the only level with a mechanical correctness oracle**: every
`PATTERNS` row names the `x64.rs` emitter it reproduces and the tests assert
byte-for-byte equality across a register matrix including R8–R15 and the
RSP/RBP/R12/R13 addressing special cases. 68 pass.

### 2. Level 2 is missing as an *artifact*, not as a design

Three finished components dead-end on one absence — **no value in this compiler
means "an instruction whose operands are values, not addresses"**:

* **`jit/src/x64/isel.rs`** (6 848 lines, 67 pattern rows, 68 passing tests) has
  a block tiler producing exactly that value: `Vec<MInst>` with `NodeId`
  operands (`isel.rs:3368`). **No production caller.**
* **`regalloc::allocate_linear_scan`** (`regalloc.rs:4749`) produces an
  assignment over IR values; `verify_allocation` (`regalloc.rs:5454`) proves it.
  Its one consumer (`ir_lower.rs:7051`) uses it as a **write-through read
  cache**, because the encoder cannot accept a register location:
  `ir_lower::frame_word_off` returns `Err` for `ValueLoc::Reg`
  (`ir_lower.rs:5088`) — "this backend keeps every value in a frame word".
* **`x64/vec_emit.rs::emit_vector_loop`** (`vec_emit.rs:1095`) has **no caller
  either**, and carries a private XMM0–XMM5 pool (`vec_emit.rs:514`) precisely
  because there is no shared place to put a register assignment.

Three private answers to "where does this value live", nothing joining them.

### 3. What `ir_lower` does, and the part of level 2 that already exists

`lower_inner_with_scopes` (`ir_lower.rs:6821`), in order: admission (five graph-
shape refusals), resource bounds, **frame layout** (`plan_slots` +
`verify_slot_colouring`), **location verification** (`verify_data_locations`),
buffer sizing, **register residency** (`plan_register_residency`), emission, and
post-emission soundness nets.

The bolded steps are *already* level-2 work, run as separate passes over level-1
data. They hand their results to the emitter as side tables (`slot_plan`,
`RegResidency`) that only the emitter reads. **A large part of level 2 exists;
what does not exist is a value that carries its result forward.**

---

## Design

### Where the safepoint / oop-map obligation lives

**With whoever assigns registers.** Not a preference — the tree settled it
twice, in two backends, and the answers agree.

There are **two** consumers of "where does this value live at this program
point", and they do not have the same power:

| Consumer | Can read a register? | Where |
|---|---|---|
| GC root walk / relocation | **No** | `OopMapEntry { frame_slot_offsets: Vec<i16>, … }` (`lib.rs:900`), consumed by `remap_one_jit_frame` (`conservative_roots.rs:2919`) |
| Deopt frame reconstruction | **Yes** | `FrameValue::Register` / `RegisterLong` / `RegisterRef` / `XmmFloat` / `XmmDouble` (`deopt.rs:123`), resolved against `SavedRegisters { gpr: [u64;16], xmm: [u64;16] }` (`deopt.rs:1417`) |

**The asymmetry is load-bearing and must not be "fixed" by symmetry.** Deopt can
read a register because the deopt stub *voluntarily spilled the whole register
file* before calling the runtime. A GC safepoint has no such spill: the
collector walks a frame it did not stop, through RBP, from outside. `x64.rs`
says so — the `reg_oops` register bitmap is "a TODO with no field, no producer
and no consumer anywhere in the workspace" (`x64.rs:3458`).

> **The rule: no reference may be register-resident at a GC safepoint.** The
> level that hands out registers owns proving it and discharges it by spilling
> to the value's frame home before the safepoint.

Both backends satisfy it, differently:

* **`x64.rs` single-pass — conditional, with a named predicate.** Java locals
  *are* register-homed (`LOCAL_REGS` = RBX/R12–R15 everywhere, plus RSI/RDI on
  Windows where those are callee-saved — `x64.rs:256`, `:273`). The
  pre-safepoint spill is elided only when
  `SafepointPublishPlan::no_reference_in_registers()` holds
  (`regalloc.rs:1541`). The five-point argument at `x64.rs:3457–3483` is the
  best statement of this obligation in the tree; read it before touching any of
  this.
* **`ir_lower` — structural**, by having no GP register to give: the linear-scan
  file is XMM2–XMM5 and `RegClass::of(IrType::Ref)` is `Some(Gp)`
  (`regalloc.rs:3526`), so a `Ref` cannot be promoted at all.

**Consequences for a level-2 artifact:**

1. **Do not change the map format before registers are real.** Keeping
   references memory-pinned costs one store per reference definition and is what
   both production backends already do. Changing `OopMapEntry` means changing
   `conservative_roots.rs` (in `vm/`, a different crate) and the moving-young
   relocation contract at the same time.
2. **The artifact must *carry* the safepoint records**, not recompute them from
   emitted bytes. Today `emit_safepoint_map` builds the slot list by scanning
   `defined_nodes` for `IrType::Ref` at emission time (`ir_lower.rs:1121`) —
   correct only because emission and slot assignment are the same pass. Split
   them and that scan becomes a third liveness model.
3. **Copy the `coverable` / `published` shape.** `emit_safepoint_map` publishes
   an empty slot list for two different reasons and refuses to conflate them: an
   oop-free safepoint sets `moving_young_coverage_complete: true`; a slot it
   could not describe sets it `false` and diverts that cycle to the non-moving
   sweep (`ir_lower.rs:1104`, `:1175`). A level-2 form needs the same
   three-valued shape — *covered*, *nothing to cover*, *cannot describe* — not
   an `Option`.

### What a level-2 form would have to carry

Not a type proposal; a checklist. Anything omitted comes back as a second copy
of the emitter.

| Emitter needs | Today comes from | Level-2 owner |
|---|---|---|
| the instruction and its operands | `lower_data_node`'s match arm | `MInst` — exists, `isel.rs:3368` |
| which nodes a tile absorbed | nothing (no tiling) | `Tile::covered` — exists, `isel.rs:3636` |
| each value's frame word | `slot_plan` side table | frame plan |
| each value's register, if any | `RegResidency` side table | allocation |
| live reference slots per safepoint | scan at emission time | safepoint record |
| deopt frame values per bci | `resolve_frame_values`, at emission time | deopt record |
| phi edge copies | `emit_phi_copies`, at emission time | edge copy list |
| call marshalling / IC cascade | `emit_inline_cache_call` etc. | **stays `Generic`** |

That last row is what makes this a migration rather than a rewrite.
`MInst::Generic { node }` (`isel.rs:3433`) means "no rule matched; `ir_lower`
lowers this node the way it always did", and is documented as **not** a no-op:
"A selector that answered an unmatched node with silence would drop the node's
semantics entirely; this arm names the node the caller must still lower."

---

## Why the migration is not a correctness argument

The brief asked whether a level split would make three real defects
**unrepresentable** rather than merely less likely, and said a design achieving
only the latter is not worth the cost. Scored honestly: **one of three.**

**1. The monitor op that lowered to nothing — unrepresentable. ✓**
`lower_data_node`'s catch-all was reachable for 5 of 53 `ir::Op` variants.
`verify_data_locations` looks like the general guard but only fires when some
node *reads* the op's value (`ir_lower.rs:5263`) — and a monitor produces no
value, which is exactly why that op and not another could compile to silence.
At level 2 the shape cannot be written: `select_block` returns a tile for every
node (`Tile::generic` when nothing matches) and `covers()` asserts the block's
node set equals the union of `covered`. Witness:
`an_unmatched_monitor_op_is_never_silently_dropped` (`isel.rs:6625`).

**2. Escape analysis forwarding a load to a later store — not prevented. ✗**
A level-1 analysis defect: a per-field answer to a per-load question. A machine
level below it would have faithfully emitted the wrong value. What fixed it was
the *same principle at a different level* — `LoadResolution` is three-valued
(`Value` / `ZeroDefault` / `Unknown`), because collapsing "zero" and "unknown"
into one `Option` is precisely how a load-before-store gets a value from its
future. The applier consumes it and refuses on `Unknown` (`lib.rs:10353`).

**3. The dead-store location key — not prevented. ✗**
Two `int` fields of one object sharing `(base, NO_NODE, kind)`. Also level 1,
also fixed by making the unnameable case refusable:
`store_matchable_location` (`ir_optimize.rs:1695`).

**What the score means.** All three were a *silent default where a refusal
belonged* — a catch-all arm, an `Option` conflating zero with unknown, a key
conflating "no offset" with "offset zero". None was a missing level. So the
migration is justified by the coverage measurement or not at all.

### The cheap alternative, landed instead · **DONE**

One set of operations was enumerated in three places, in two vocabularies:
`lower_data_node`'s arms (48 of 53 variants), `op_defines_result_slot` (37), and
`ir::ir_compatible` — the last in the *bytecode's* vocabulary
(`scan.anewarray_ops`, `scan.typecheck_ops`, `scan.has_athrow`), which is why
the catch-all's claim "bail in `ir_compatible` prevents reaching here" was
unverifiable where it was written.

* **`lower_data_node`'s catch-all now refuses.** `_ => {}` became an `other` arm
  latching `UnsupportedShape("ir_lower: op has no lowering arm")`, naming the
  node and its op. Latched rather than returned because the function is
  infallible by signature; `lower_inner_with_scopes` takes the latch and
  discards the artifact.
* **Four tests make lists (1) and (2) agree.** The forcing function is
  `declared_lowering`, an **exhaustive match with no wildcard arm** — a new
  `ir::Op` variant stops the crate compiling until somebody classifies it, at
  *build* time. The rest are `include_str!` source scans comparing
  `lower_data_node`'s arms, `op_defines_result_slot`'s body, and `ir.rs`'s own
  `pub enum Op` block.

Refusing changed nothing, verified rather than assumed: the five variants with
no arm are unreachable from a real compile (`I2B`/`I2C`/`I2S` are constructed
nowhere in the crate — `IrBuilder` decomposes 0x91–0x93 into `Shl`/`Shr`/`And`;
`ArrayLength`/`NewArray` only in `#[cfg(test)]` code). Before/after on two
Spring classes: identical refused-method sets, 2 399 IR admissions, **zero**
firings of the new arm.

---

## Risks

1. **Trusting a synthetic corpus.** Already materialised once: ten hand-written
   shapes said 38.2% and named `Lea` as a firing rule; real code says 15.7–19.0%
   and never fires it. A fixture's *node mix* is not a real method's. Re-measure
   with the flag, on real compiles, before every go/no-go.
2. **A measurement pass that changes what compiles.** Guarded by
   `shadow_selection_changes_no_emitted_byte` plus an "it actually ran" test —
   both are needed, because a pass that never executes satisfies the first
   trivially.
3. **Register-resident references at a GC safepoint** (increment 3 onward). See
   the rule above; the map format and its `vm/`-side consumer must change
   together or not at all.
4. ~~**The vector pool** (increment 4). A scalar FP value in XMM0–5 is silently
   destroyed across a vector region.~~ **Retired** — the pool is
   XMM8–XMM15 and disjoint from both scalar ranges. The risk that replaced it is
   narrower and mechanical: a caller that hands out a pool register its own
   prologue does not save, which `vector_pool_is_encodable` refuses.
5. **Unanchored pattern rows.** Every row naming the emitter it reproduces
   byte-for-byte is the table's one trustworthy property and the only reason a
   byte-equality oracle exists. A row invented rather than anchored destroys it,
   and no test would catch that.

## What to refuse

* A `mir/` directory, crate skeleton or trait hierarchy. Level 2's first
  artifact is a `Vec<MInst>` and two side tables that already exist.
* A level between bytecode and `ir::Graph`. No customer.
* Splitting `ir::Graph`. Every optimizing pass reads it; none asks for less.
* Changing `OopMapEntry` before increment 3.
* Any pattern row not anchored to a byte literal an existing emitter produces.
* A big-bang conversion of `ir_lower`'s 90 GP memory accesses. That is the "stop
  treating the frame as the value's identity" change — the whole item, not an
  increment (`docs/jit/linear-scan-wiring.md`).

## See also

* `docs/jit/instruction-selection.md` — the selector and the pattern table.
* `docs/jit/instruction-patterns.md` — retiring `x64.rs`'s emitters *onto* the
  table. A different migration from this one; they compose.
* `docs/jit/linear-scan-wiring.md` — the write-through consumer, and the lane
  shape increment 2 generalises from.
* `docs/jit/vectorization-emitter.md` — the private XMM pool, increment 4.
