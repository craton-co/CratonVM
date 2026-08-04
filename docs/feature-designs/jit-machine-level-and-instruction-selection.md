# A machine level for the JIT, and the instruction selector that needs one

**Status: increments 0, 1, 1b, 2 and 4 landed and measured; 3 deliberately not
built.** Shadow instruction selection (`CRATONVM_JIT=ir-isel-shadow`, default
off, emits nothing) measured the tiler's real coverage at **15.7–19.0%**, with
the two rules the migration was *for* firing **zero** times. Closing the
immediate half of that gap moved it to **19.3%** and `AluImm` fired **once**
across 719 methods — which read as "the rules do not match the shape of the IR
the optimizer produces".

**That reading was half right, and the half that was wrong mattered.**
2026-08-04 closed the other named gap (`lea_r32_m`, one anchored row) and
`Rule::Lea` went from zero to **26 tiles** on a framework-shaped corpus. And
`AluImm`'s near-zero turned out not to be about rows at all: a safepoint
snapshot **pins** almost every live constant, so a tile cannot absorb it, and
the cost model then compares a two-byte `ADD EAX, ECX` against a three-byte
`ADD EAX, 7` **without pricing the frame load the register form also pays**.

Increment 2 is therefore built and measured rather than argued about: the
selector has a production caller (`CRATONVM_JIT=ir-isel-emit`, default off,
fail-closed) and a byte-equality oracle that ran over a real workload and found
**zero** disagreements. See
`docs/internal/hir-02-mir-regalloc-handoff-RETIRED-20260804.md`.

Consolidates the `hir-01` and `hir-02` lanes of
`docs/known-issues/c2/deep-research-vm-c2.md`, which asked for an HIR/LIR/MIR
split and for somewhere to send `isel`'s output. Both are answered here.

Every claim is checked against the tree and cited `file:line`.

---

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

## Implementation steps

### Increment 0 — shadow selection · **LANDED 2026-08-03**

Run `select_block` per block inside `lower_inner_with_scopes`, assert
`BlockSelection::covers`, count what fired, discard, and emit through the
unchanged path.

| Piece | Where |
|---|---|
| Flag (declared, default off) | `CRATONVM_JIT=ir-isel-shadow` → `ir_lower::isel_shadow_enabled` |
| Report | `CRATONVM_DBG=ir-isel` → `ir_lower::isel_shadow_reporting` |
| The pass | `isel::shadow_select_method`, with `ShadowStats` + process totals |
| Call site | nine lines in `lower_inner_with_scopes`, after every refusal |

Placed after every refusal on purpose, so the population it measures is exactly
the population that gets a compiled body.

**Deliberately NOT fail-closed** — the one such place in `ir_lower`. A flag whose
documented effect is a count must not decide what compiles, or the count
describes a different program. A `covers()` violation is counted and printed,
not raised. `shadow_selection_changes_no_emitted_byte` holds that up; fail-closed
returns at increment 1.

**Its answer** — Azure Linux, release binary, two Spring Boot test classes,
every method the optimizing tier actually compiled:

| | `ConfigurationPropertiesTests` | `BinderTests` |
|---|---|---|
| methods shadowed | 718 | 132 |
| blocks | 1 653 | 258 |
| scheduled data nodes | 4 136 | 696 |
| tiles | 4 917 | 828 |
| tiles firing a real rule | 443 | 64 |
| **nodes covered by a real rule** | **785 (19.0%)** | **109 (15.7%)** |
| `covers()` violations | **0** | **0** |

Rules that fired, both classes summed:

| Rule | Tiles | Worth |
|---|---|---|
| `TestZeroBranch` | 284 | `CMP r, 0` → `TEST r, r`. Real, and small. |
| `AluReg` | 123 | The two-address form. Real — drops a frame round trip. |
| `TestBranch` | 60 | **Nothing.** Byte-for-byte what `lower_terminator` already emits. |
| `CmpBranch` | 40 | Compare-and-branch fusion. Best per-site win here. |
| `Lea` | **0** | — |
| `AluImm` | **0** | 119 `Unencodable` refusals |
| `AluFoldedLoad` | **0** | `fold_loads` off; `AddrSource::Opaque` |
| `CmpSetCc` | **0** | no `SETcc` row |

**The finding: the two rules the migration was *for* fire zero times on real
code.** `Rule::Lea` — address-mode folding, the headline item in
`instruction-selection.md` §1 — never matched once across 850 methods.
`Rule::AluImm` never matched either. Same cause: **the pattern table is 64-bit
and Java arithmetic is 32-bit.** `Rule::Lea` refuses `Ty::I32` outright; there
are no `*_r32_imm*` rows.

A synthetic ten-shape corpus had predicted 38.2% — double the truth, *and*
pointing at the wrong rules (it fired `Lea` once; real code never does).
Increment 1 would have been justified on that number. It is not justified on
this one. This is what increment 0 was for.

Also proved: `covers()` held on all **1 911** blocks. The invariant the
three-defect argument below rests on is real on real code, not just on
hand-built graphs.

### Increment 1 — the 32-bit immediate rows · **DONE 2026-08-03, and the answer is +0.3 points**

Eight rows added — `add/sub_r32_imm8`, `add/sub_r32_imm32`,
`and/or/xor_r32_imm8`, `cmp_r32_imm8` — each anchored to a byte literal
`x64.rs`'s constant-folding fast path already emits (the `iadd`/`isub`/`iand`/
`ior`/`ixor`/`if_icmp` const arms, EAX destination, no REX). Plus the half that
makes them load-bearing: `MInst::pattern_name` now maps `AluRI`/`CmpRI` at
`Ty::I32`, without which `require_encodable` discards the tile whatever the
table holds.

Same workload, same flag, before and after:

| | before | after |
|---|---|---|
| methods / nodes | 718 / 4 136 | 719 / 4 138 |
| **nodes covered** | **785 (19.0%)** | **797 (19.3%)** |
| `AluImm` tiles | 0 | **1** |
| `Unencodable` refusals | 104 | **57** |
| `CmpBranch` / `TestBranch` | 33 / 53 | 44 / 42 |
| `covers()` violations | 0 | 0 |

**The gap was real and closing it bought almost nothing.** `Unencodable`
halved, so the rows are being reached; `AluImm` then fired **once** across 719
methods. The genuine gain is elsewhere and smaller than it looks: `cmp_r32_imm8`
let eleven branches move from `TestBranch` (which buys nothing) to `CmpBranch`
(which fuses), and that is most of the +12 nodes.

The reason is `ir_optimize`. It runs *before* selection and folds constants, so
by the time the tiler sees the graph an `Add(x, Const)` is mostly already gone.
The rows were missing, but the population that wanted them is nearly empty on
optimized IR — a fact no amount of reading the table would have produced.

### Increment 1b — `Rule::Lea` at `Ty::I32` · **DONE 2026-08-04, and it fires**

The other half of `instruction-selection.md` §6 item 2. **The proof it was
blocked on is now available**, and it is simpler than the doc expected:

> `ir_lower`'s `Int` arms already leave the high half of the destination slot
> unspecified, and inconsistently so. `Op::Add`/`Mul` at `IrType::Int` emit
> 32-bit ops (`ADD EAX, ECX`), which x86-64 **zero**-extends into RAX;
> `Op::Const` at `IrType::Int` emits `emit_mov_rax_imm64`, which for a negative
> `int` **sign**-extends. `Op::Return` then copies the whole 64-bit slot. So the
> high half of an `int` slot is already two different things depending on which
> arm produced it, and the caller already truncates by descriptor — which is the
> only reason the tree is green.

A 32-bit `LEA` zero-extends, exactly like `ADD EAX, ECX`. It therefore leaves
the same high half the majority of existing `Int` arms leave and introduces no
new observability question. The `lea_r32_m` row is anchored too — `x64.rs`
already emits `8D 04 40` / `8D 04 80` / `8D 04 C0` (`LEA EAX, [RAX+RAX*n]`) in
its small-multiply fast path.

It was left unbuilt on the strength of increment 1's **+0.3 points**, on the
argument that the other half of the same gap would be worth about the same.
**That argument was wrong, and the reason is worth keeping.** `Rule::AluImm`'s
near-zero was never mostly about the rows (see the status banner); `Rule::Lea`'s
zero was *entirely* about the row, and the row was one line of table.

Built 2026-08-04. `Rule::Lea` fires **26 tiles** across CratonBenchC2's three
phases (12 / 4 / 10) against zero before, and is the most frequent non-generic
rule on that corpus.

**It also re-ranked the tiling, immediately.** `a + b` with `a` still live
started selecting an `LEA` — because `needs_copy` priced a two-address `MOV`
that does not exist under a frame-homed allocation, where the destination
register never held the left operand. `SelectOptions::frame_homed` now states
the allocation and `Tile::frame_homed` re-prices every candidate from its own
operand set. **Closing one gap re-ranks every other rule**, in both directions.

### Increment 2 — emit one rule, byte-identical · **DONE 2026-08-04**

`Rule::AluReg`: its rows are anchored to the exact byte literals
`lower_data_node` emits, and the frame-homed allocation the encoder assumes IS
this backend's allocation, so byte equality is reachable rather than
approximate.

| Piece | Where |
|---|---|
| The artifact | `ir_lower::MirPlan` — a `Vec<MInst>` per block plus a `tile_of` index. `SlotPlan` and `RegResidency` stay where they were. |
| The encoder | `Lowerer::encode_tile_frame_homed`, through `isel::select` — level 3 unchanged. |
| The oracle | `CRATONVM_JIT=ir-isel-verify`: the per-opcode arms still emit, and the encoder's answer is compared against what they wrote, per node. |
| The wiring | `CRATONVM_JIT=ir-isel-emit`: the encoder emits; the arm is not run for a node a tile covers. |

Both modes are **fail-closed** — an uncovered block, a destination slot the
encoder and `alloc_slot` disagree on, or any byte mismatch discards the
artifact. Scope is tiles covering exactly their own root: the caller skips every
node a tile covers, so an absorbed node the encoder does not fold would be
computed nowhere.

**The result**, CratonBenchC2, three phases, one process each: 19 methods, 39
tiles, **0 byte mismatches**, and a bit-identical checksum in all three modes.
Coverage on that corpus is 23.6% / 24.0% / 32.3%.

Two supporting refactors, both removing a copy rather than adding one:
`load_to_rax`/`load_to_rcx`/`store_rax` now delegate to `enc_frame_load` /
`enc_frame_store` (a byte-equality oracle is only as strong as the number of
places the bytes come from), and `alloc_slot_checked` splits into a pure
`planned_slot_off` plus its three mutations.

### Increment 2b — emitting the rules byte equality cannot cover · open

`Rule::AluImm` and `Rule::Lea` **cannot** ride increment 2's oracle by
construction: dropping a frame load is the point, so the bytes differ. They need
a differential-execution oracle — `verify-01`'s harness — and a decision about
whether the saving is worth it. Verify mode already reports the size of the
prize, on the `[ir-isel] MIR TOTALS` line: `shadow_tiles` is how many such tiles
there were, `arm_bytes` and `enc_bytes` what the two paths would have written
for exactly those nodes.

### Increment 3 — registers · **not built, deliberately**

Only here does `Allocation` stop being a read cache. **The prerequisite is the
prologue, not the allocator**: `ir_lower::emit_prologue` saves no callee-saved
register, so RBX/R12–R15 cannot be handed out until there is a save area
restored on all three exits (`emit_epilogue`, the inlined epilogue in
`emit_deopt_stub`, `emit_call_exc_stub`), placed at or above `callee_saved_lo`
so the conservative band scan does not read a caller's register as a root. The
safepoint rule above binds from here on.

**That prerequisite is not why it is unbuilt.** Building the save area is
tractable; a save area with no consumer would be a *fourth* finished component
with no caller, in a lane whose entire finding is that this compiler already has
three. The change that would give it a consumer is on this document's own "What
to refuse" list — the big-bang conversion of `ir_lower`'s 90 GP memory accesses
— and it takes the safepoint rule with it: today `ir_lower` satisfies "no
reference register-resident at a GC safepoint" *structurally*, by having no GP
register to give, and a GP class turns that into something proved per site.

What would change this: a measurement showing the frame round trip is a material
cost on real code. Increment 2 did not produce one, and its oracle cannot —
byte equality answers a correctness question by construction.

### Increment 4 — unify the vector pool · **DONE 2026-08-04, as far as it can go here**

`vec_emit`'s private XMM0–5 pool overlaps `ir_lower`'s FP value tier
(XMM0/XMM1) *and* the linear-scan file (XMM2–XMM5), and the failure is a scalar
FP value silently destroyed across a vector region.

The overlap itself **cannot be removed here**: the only registers that would
separate the ranges are XMM6–XMM15, callee-saved on Windows, and
`emit_prologue` saves nothing — increment 3's prerequisite. What landed removes
the *privacy* and the *assumption*:

* `regalloc::xmm_roles` declares all three ranges together, and `ir_lower`'s
  `XMM0`/`XMM1`/`IR_LOWER_LS_XMMS` are defined from it — one declaration, not
  three that agree today.
* `VecEmitRequest::vector_pool` replaces the private constant: which XMM
  registers are dead across a region is a fact about the surrounding method, so
  the surrounding method states it. An **empty pool is legal** and refuses at
  the first allocation — the right answer for a caller that has proved nothing.
  A pool naming a register the emitter cannot encode refuses the whole region
  rather than being quietly narrowed.
* `the_three_xmm_authorities_are_stated_in_one_place` **asserts** the overlap
  rather than wishing it away, and fails the day a prologue save area removes
  it.

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

### The cheap alternative, landed instead · **DONE 2026-08-03**

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
4. **The vector pool** (increment 4). A scalar FP value in XMM0–5 is silently
   destroyed across a vector region.
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

## Effort

Increments 0, 1, 1b, 2 and 4: **done**. Increment 2b (emitting `AluImm`/`Lea`
behind a differential oracle): **M**, and verify mode already reports what it
would be worth. Increment 3: **L**, and gated on a performance measurement
nobody has taken — not on the prologue.

---

## Premise deltas found while answering this

Reported per `docs/known-issues/c2/README.md` rule 1. All corrected in place.

| Claim | Source | Actual |
|---|---|---|
| "`isel.rs` … has never been compiled … its tests have never run" | `instruction-selection.md` §0 | **Stale.** `pub mod isel;` at `x64.rs:136`; 68 tests pass. |
| "add `pub mod isel;` before wave 1" | `instruction-patterns.md` | **Stale**, same reason. |
| "the applier still reads `field_values` … correct but pessimistic" | `escape-analysis.md` §3, §6.1 | **Stale.** `plan_scalar_replacement` consumes `load_value` and refuses on `Unknown`; witness `ea_load_before_a_later_store_forwards_the_pre_store_value` (`lib.rs:17228`). |
| the monitor ops have "no lowering arm here"; the catch-all "is `_ => {}`" | the guard atop `lower_inner_with_scopes` | **Stale.** A real `Op::MonitorEnter \| MonitorExit` arm exists. The guard's actual scope is narrower: monitor ops plus a helper table with no monitor entry. |
| `CRATONVM_JIT_NO_PRECISE_FIELD_OPS` declared | — | It was read by code and declared nowhere; `types`' `flag_declaration_guard` was **already red on `dev`**. Rule 4's recurrence. Declared. |
| "`ir_lower.rs` (~10.6k lines)" | the `hir-01` brief | 11 138 lines. |
| "no register bank in the oop map" | the `hir-01` brief | True for GC. **Not true for deopt.** The asymmetry is deliberate. |
| implied: `isel` is the only unwired component | the `hir-02` brief | `vec_emit::emit_vector_loop` has no caller either, for the same reason. |

## See also

* `docs/jit/instruction-selection.md` — the selector and the pattern table.
* `docs/jit/instruction-patterns.md` — retiring `x64.rs`'s emitters *onto* the
  table. A different migration from this one; they compose.
* `docs/jit/linear-scan-wiring.md` — the write-through consumer, and the lane
  shape increment 2 generalises from.
* `docs/jit/vectorization-emitter.md` — the private XMM pool, increment 4.
