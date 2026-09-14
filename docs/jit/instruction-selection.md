# Instruction selection

**Status:** component landed and **compiled**, not wired. Nothing in the
production pipeline calls it. See [Wiring](#wiring) for what the call site has
to do, and
`docs/feature-designs/jit-machine-level-and-instruction-selection.md` for the
increment order that wiring should follow. **There is a production caller**, behind `CRATONVM_JIT=ir-isel-emit` (default off): §6
items 2, 5 and 9 are closed, and `ir_lower` emits `Rule::AluReg` tiles through
this table with a byte-equality oracle over the whole compile.

**Where:** `jit/src/x64/isel.rs`, below the `// IR-level instruction selection`
banner. The memory-ordering rule it depends on is in `jit/src/ir_schedule.rs`.

---

## 0. Read this first: compiled, still unwired

`pub mod isel;` is at `jit/src/x64.rs:136`. It was once absent,
so this file had **never been compiled** and none of its tests had ever run;
the first compile surfaced a real cost-model bug — `Rule::Lea` beating the ALU
form on a one-register address, caught by
`a_constant_that_fits_imm8_becomes_an_immediate` and fixed by the candidate
ordering in `select_block` (`isel.rs:4245`). That is history now, and the state
to reason from is:

* The declarative pattern table compiles.
* Its tests — including the byte-for-byte equivalence sweep against `x64.rs`'s
  hand-written emitters, which is the entire basis for trusting the table —
  run. Verified on the Azure host:
  `cargo test -p cratonvm-jit --lib isel` → **68 passed, 0 failed**.
* What remains unverified is *execution of the selector's output*, not the
  table: nothing emits a tile, so no claim below about what selection would
  produce end-to-end has been observed running. §6 item 9 is the live gap.

Two related components are in the same state and for the same reason — no
consumer exists for "an instruction whose operands are values, not addresses":
`jit/src/regalloc.rs::allocate_linear_scan` (used only as a write-through read
cache) and `jit/src/x64/vec_emit.rs::emit_vector_loop` (no caller at all).
`docs/feature-designs/jit-machine-level-and-instruction-selection.md` is the
joint statement of that gap.

**Measured coverage, on real compiles.** Shadow selection
(`CRATONVM_JIT=ir-isel-shadow`, default off, emits nothing) ran over 850 Spring
Boot methods: `select_block` covers **15.7–19.0%** of scheduled
data nodes; the rest fall to `Rule::Generic`. **`Rule::AluImm` and `Rule::Lea`
fired ZERO times** — the table was 64-bit and Java arithmetic is 32-bit. Of the
four rules that did fire, `TestBranch` (60 tiles) is byte-for-byte what
`ir_lower` already emits.

An earlier ten-shape synthetic corpus said 38.2% and fired `Lea` once. It was
double the truth and named the wrong rules. That ranked §6's gap list
unambiguously: **the 32-bit rows first, before any production wiring.**

Both row gaps are now closed (§6 items 2 and 5) and re-measured on
CratonBenchC2, whose node mix is framework-shaped rather than kernel-shaped:
**23.6% / 24.0% / 32.3%** across its three phases, with `Rule::Lea` firing
**26 tiles** where it previously fired none. Full figures:
`docs/feature-designs/jit-machine-level-and-instruction-selection.md`.

---

## 1. What problem this solves

`ir_lower::lower_data_node` selects and lowers in one step, one IR node at a
time, through the frame. Every binary integer node emits:

```
MOV RAX, [RBP - a]        ; load_to_rax
MOV RCX, [RBP - b]        ; load_to_rcx
<op> RAX, RCX
MOV [RBP - d], RAX        ; store_rax
```

That is correct and it is what the cost model calls `GENERIC_COST` — 15 bytes,
4 micro-ops, ~9 cycles. It leaves five families of x86-64 instruction unused:

| Pattern | What the generic lowering does | What selection does |
|---|---|---|
| Address-mode folding | `a + (i<<2) + 16` = three ALU nodes, three frame round-trips | one `LEA r, [a + i*4 + 16]` |
| Compare-and-branch fusion | `CMP; SETcc AL; MOVZX; MOV [slot]; MOV RAX,[slot]; TEST; Jcc` | `CMP; Jcc` |
| `TEST` vs `CMP`-against-zero | `CMP r, 0` (3–4 bytes) | `TEST r, r` (2–3 bytes) |
| `LEA` for three-operand add | `MOV dst, lhs; ADD dst, rhs` when `lhs` is still live | `LEA dst, [lhs + rhs]` |
| Immediate / memory folding | materialise the constant or the load into a register first | fold it into the operand field |

---

## 2. The pattern set

Every rule is a `Rule` variant, and every rule produces a `Tile`: a set of
covered IR nodes plus the `MInst`s selected for them.

### `Rule::Lea` — address-mode folding

`match_address(ctx, root)` reads a `base + index*scale + disp` expression out
of the pure integer arithmetic rooted at `root`:

```
term := Add(term, term)             -- split
      | Shl(x, Const k), k in 0..=3 -- index x, scale 1<<k
      | Mul(x, Const c), c in 1/2/4/8
      | Const(c)                    -- disp += c   (never at the root)
      | anything else               -- base, or a scale-1 index
```

At the root only, the *small multiply* forms are also admitted: `x*2`, `x*3`,
`x*5`, `x*9` become `[x+x]`, `[x+x*2]`, `[x+x*4]`, `[x+x*8]`.

Deliberately **not** admitted:

* `x*4`, `x*8`, `x<<2`, `x<<3` at the root. They need `[x*4]` — a base-less
  operand. That form exists in x86 (`mod=00`, SIB `base=101`, disp32) but
  `isel::Mem` has no way to say "no base": its `base` is a `u8`. Admitting it
  would produce an `IrAddr` nothing can lower. `IrAddr::check` returns
  `SelError::NoBaseRegister` and the caller falls back to a shift.
* Anything at `Ty::I32`. See §6.

An interior node is absorbed only when `SelCtx::absorbable` holds: it is **in
this block** and has **exactly one value use**. Out-of-block matters as much as
single-use — absorbing a loop-invariant expression the scheduler hoisted would
recompute it every iteration *and* leave the original standing.

### `Rule::AluImm` — constant into the immediate field

The right operand (either operand, for the commutative ops) becomes an
immediate when `imm_form_for` accepts it. `Sub` folds only its right operand.

### `Rule::AluReg` — the two-address register form

`dst <- lhs op rhs` compiles to `OP dst, rhs` with `dst` and `lhs` coalesced.
That is only legal when `lhs` dies here, so when it does not, the tile carries
an explicit `MInst::Move` first — and that copy is what makes `LEA` win the
cost comparison. Neither instruction is unconditionally better; the cost model
decides.

### `Rule::AluFoldedLoad` — memory-operand folding

`MOV tmp, [addr]; ADD dst, tmp` becomes `ADD dst, [addr]`, gated by
`may_fold_load` (§3). Off by default (`SelectOptions::fold_loads`) because the
address half is still `AddrSource::Opaque` (§6).

### `Rule::CmpBranch` / `Rule::TestZeroBranch` — compare-and-branch fusion

An `Op::Cmp` that is in this block and has exactly one value use (the `Op::If`)
never materialises its 0/1 value. In-block matters: a compare the scheduler
placed in a dominator computes flags that every instruction in between would
have destroyed.

`CMP r, 0` becomes `TEST r, r`. The substitution is exact for **every**
condition code, signed or unsigned — not just the equality pair. `CMP r, 0` is
`SUB r, 0` and `TEST r, r` is `AND r, r`; both clear CF and OF and set SF and ZF
from the value itself, so all four flags agree.

A zero on the *left* (`0 < x`) swaps the operands, so the condition is
**mirrored**, not negated: `Lt` becomes `Gt`. `mirror` and `CmpOp::negate` are
different functions and confusing them inverts the program;
`a_zero_on_the_left_mirrors_the_condition` pins both.

### `Rule::TestBranch` — the unfused branch

`TEST cond, cond; JNE` — byte-for-byte what `ir_lower::lower_terminator`
already emits. It is the terminator's fall-back, not a rival: see §5.

### `Rule::CmpSetCc` — a compare whose value is read

`CMP; SETcc; MOVZX`. Currently always discarded by the encodability gate (§6),
so a compare with a second consumer falls to `Rule::Generic`, which is exactly
`ir_lower`'s existing `Op::Cmp` arm.

### `Rule::Generic` — the fall-back

A node no rule matched. **Not a no-op**: `MInst::Generic { node }` names the
node the caller must still lower. `BlockSelection::covers` asserts that every
block node is covered exactly once, and the tests check it — including
`an_unmatched_monitor_op_is_never_silently_dropped`, which is the shape of the
`lower_data_node` catch-all bug that dropped a lock.

---

## 3. The one-use / memory-effect gate

Folding a load into a later arithmetic user **moves the memory access
forward**, past everything between them. `may_fold_load` refuses unless all
five hold.

1. **Both in this block, load first.** The fold is a forward motion or it is
   nothing.
2. **Exactly one value consumer.** Two consumers and the fold either performs
   the access twice — a second cache miss, and *two* reads of a location
   another thread may be writing — or leaves the second consumer without a
   register.
3. **No safepoint snapshot names it.** A deopt frame has to name every live
   value; a value that only ever exists inside another instruction's operand
   cannot be named.
4. **A plain read.** Not a write, not an allocation, not a safepoint, and
   `MemOrder::Plain`. A volatile read is an acquire, and moving anything across
   an acquire — including itself — is precisely what an acquire forbids.
5. **Every node it crosses answers `Reorder::Allowed`.** `Graph::may_reorder`
   is documented as *pairwise*: it says nothing about a third node. So the
   check runs against each intervening node individually. Asking only about the
   user would say nothing about the store in between, and folding across a
   store to the same cell makes the load read the value that store *just
   wrote* — the same defect class as the escape-analysis bug found on this
   branch, where a load was folded to a later store's value.

### Value uses are not edge uses

`Graph::use_counts` counts every input slot. `IrBuilder` threads the memory
token *through loads* (`self.mem = load` after every `getfield`, `ir.rs:5011`),
so the next memory operation holds an edge to the load. Counting that edge
would make every load look multiply-used and refuse every fold.

`ValueUses::of` therefore skips slots `ir::is_memory_token_slot` identifies,
and counts safepoint snapshot references separately as `pinned`. The token edge
is an *ordering* edge, and ordering is what condition 5 checks.

The corollary matters: acting on condition 5 means the emission order no longer
agrees with the token chain. That is licensed — `Graph::may_reorder`'s own
documentation says a pass acting on an `Allowed` answer must repair the chain —
and for a selector it is free, because the chain is never emitted as anything.
Ordering in the emitted code is positional.

---

## 4. The cost model

Three numbers per instruction, ranked **micro-ops, then bytes, then latency**
(`SeqCost::key`). Front-end throughput is what a tiling decision buys; bytes
break the tie (instruction cache); latency last.

Costs come from the `PATTERNS` table wherever a row exists, so the cost model
and the encoder cannot drift. `MInst::cost` adds the operand-dependent bytes
the table's `cost.bytes` floor omits (a SIB byte, the displacement width from
`Disp::encode`), or the selector would fold a `disp32` in as though it were
free. The handful of instructions with no row carry a literal that states its
own arithmetic in the source.

### Net cost is what ranks tiles

Raw cost alone cannot rank a tiling. Folding a load makes the *consumer* more
expensive — `ADD r, [m]` is longer and slower than `ADD r, r` — and is still
right, because the load's own four instructions disappear.

`Tile::net_key` is `cost − baseline`, where `baseline` is one `GENERIC_COST`
per covered node: the alternative to covering a node is that the node gets its
own generic lowering. Signed, not saturating — two tiles covering the same
nodes have the same baseline, so raw cost still separates them, whereas a
saturating subtraction would clamp both to zero and make the choice arbitrary.

`GENERIC_COST` is read off `lower_data_node`'s actual shape (two frame loads,
the operation, one frame store) and is deliberately the most expensive thing
the model can name: a rule that covers more nodes must win, and this is what
makes it win.

### Worked example: `LEA` versus `ADD`

| | covers | raw | baseline | net (uops, bytes, lat) |
|---|---|---|---|---|
| `ADD dst, rhs`, `lhs` dies | 1 | (1, 3, 1) | (4, 15, 9) | (−3, −12, −8) ✅ |
| `LEA dst, [lhs+rhs]` | 1 | (1, 4, 1) | (4, 15, 9) | (−3, −11, −8) |
| `MOV dst,lhs; ADD dst,rhs`, `lhs` live | 1 | (2, 6, 2) | (4, 15, 9) | (−2, −9, −7) |
| `LEA dst, [lhs+rhs]`, `lhs` live | 1 | (1, 4, 1) | (4, 15, 9) | (−3, −11, −8) ✅ |

---

## 5. Fail-closed

There is no silent no-op in this module. Every path that declines produces
something a caller can act on.

* **An unmatched node** becomes `MInst::Generic { node }`, which names it.
* **An unmatched terminator** still gets a tile. `tile_terminator` returns
  candidates *most preferred first* — the fused form, then the unfused
  `TEST; Jcc` — and the driver takes the first the gate admits, falling back
  to `Tile::generic(term)`. A branch selection declined to cover is a branch
  nobody emits, which is the one failure this design must not have.
* **A tile the table cannot encode** is discarded under the default
  `SelectOptions::require_encodable`, and the reason is recorded as
  `Note::Unencodable`. The node falls back to the generic lowering rather than
  being selected into an instruction nobody can emit.
* **Every refusal names a node.** `AddrRefusal`, `FoldRefusal` and
  `OrderRefusal` all carry the node or value responsible.
* **`BlockSelection::covers`** is the whole-block invariant: every node
  covered exactly once. A node covered twice is computed twice; a node covered
  zero times is a dropped instruction.

### Immediate widths

`imm_form_for` is `i8::try_from` / `i32::try_from`. Never `as i8` / `as i32`.
`128 as i8` is `-128`, so `AND r, 128` written that way clears every bit but the
sign bit instead of setting one. Displacements go to `Disp::encode32` — the
checked helper in `x64/disp.rs` — and a folded displacement accumulates with
`checked_add`, so it cannot wrap into a small-looking number. This module adds
no narrowing of its own.

---

## 6. What is still unvalidated, and what each needs

Ordered by value.

1. ~~**The module is not compiled.**~~ Done (§0). Its tests run and
   pass; everything below is now a claim about the table's *contents*, not
   about whether anything checked them.
2. ~~**No 32-bit immediate or `LEA` rows.**~~ **Both closed** — the immediates
   (eight rows, anchored to `x64.rs`'s constant-folding fast path),
   the `LEA` (`lea_r32_m`, anchored to `x64/arith.rs`'s
   `emit_imul_const`: `8D 04 40` / `8D 04 80` / `8D 04 C0`). Both halves each
   time — `MInst::pattern_name` maps the new shape too, without which
   `require_encodable` discards the tile whatever the table holds.

   `Rule::Lea` now fires on real code: **26 tiles** across CratonBenchC2's
   three phases, against zero before. `Rule::AluImm` needed a third thing that
   was not a row — see item 10.
3. **`AluRM` has no `PATTERNS` row.** The load-fold *gate* is implemented and
   tested; the *encoding* is not. Needs `add/sub/and/or/xor/cmp_{r64,r32}_m`
   rows (`03/2B/23/0B/33/3B /r`, `reg: Dst`, `rm: Mem`,
   `DispPolicy::Smallest`). They were not added because no `x64.rs` emitter
   produces those bytes, and the table's discipline is that every row names the
   hand-written code it reproduces byte-for-byte. Adding six unanchored rows
   would weaken the one property that makes the table trustworthy.
   `SelectOptions::fold_loads` is off by default for the same reason.
4. **`AddrSource::Opaque`.** `Op::Load`'s edge layout is
   `[ctrl, mem, base, field_index]` — a *field index*, not a byte offset — so
   turning one into `[base + disp]` needs the object layout, which is
   `ir_lower`'s knowledge. The selector proves the fold legal (the part that
   fails silently) and leaves the address shape to the lowering. Closing this
   means teaching `AddrSource::Expr` how `ir_lower` computes a field address.
5. ~~**`Rule::Lea` refuses `Ty::I32`.**~~ **Closed.** The proof the
   item asked for turned out to be simpler than it expected and did not need to
   start at `Op::Return`: a 32-bit `LEA` zero-extends into the destination,
   which is the *same* high half `ADD EAX, ECX` leaves — and that is what
   `ir_lower`'s `Op::Add`/`Op::Mul` `Int` arms already emit. The row therefore
   raises no observability question the tree does not already answer.
6. **`SetCc` has no row.** SETcc's destination is an 8-bit register, and
   `SPL`/`BPL`/`SIL`/`DIL` require a REX prefix that `AH`/`CH`/`DH`/`BH` must
   not have. The table has no `Constraint` for that, and a row that emitted
   `RexMode::Always` would no longer reproduce `ir_lower`'s three-byte
   `0F 9x C0`. Needs a `ByteRegNeedsRex` constraint.
7. **`CmpRI` has no row** (`83 /7 ib`, `81 /7 id`). Mechanical, but again
   unanchored: no `x64.rs` emitter produces `CMP r64, imm`.
8. **`MInst::probe` uses placeholder registers.** Register numbers change an
   encoding's *length* (REX, the RSP/RBP addressing quirks) but not whether one
   exists for these rows, which is the only question answerable before
   allocation. The displacement, immediate and scale are real, so the probe
   catches the mistakes that produce wrong code. It does not catch
   allocation-time constraints — an index allocated to RSP, a base allocated to
   R12 needing a SIB byte. Those are `Mem`'s job and the table already states
   them (`Constraint::IndexNotRsp`, `base_requires_sib`); the register
   allocator has to honour them when it lowers an `IrAddr`.
9. ~~**No end-to-end byte comparison.**~~ **Closed**, and it is
   stronger than "same observable behaviour": `CRATONVM_JIT=ir-isel-verify`
   compiles through both paths and compares the **bytes**, per node, over a
   whole workload, refusing the compile on any disagreement. Scope is
   `Rule::AluReg`, the only rule whose bytes are provably identical to the
   per-opcode arm's. `ir-isel-emit` then makes those tiles the emitted bytes.
   See `docs/feature-designs/jit-machine-level-and-instruction-selection.md`
   increment 2.
10. **The cost model prices instructions, not operands** — and that, not the
   missing rows, is what kept `Rule::AluImm` at zero. `ADD EAX, ECX` is two
   bytes and `ADD EAX, 7` is three, so the register form wins; under a
   frame-homed allocation the register operand also costs a
   `MOV r64, [RBP-disp]` that the immediate form does not. `SelectOptions::
   frame_homed` states the allocation and `Tile::frame_homed` re-prices every
   candidate from its own operand set. The remaining half is that a consumer
   which is not frame-homed needs a different answer again, and there is no
   such consumer yet.

   A second cause, worth knowing before blaming a row: `ValueUses::single_use`
   is `count == 1` **and not pinned**, and a safepoint snapshot names almost
   every live constant. So a tile usually cannot *absorb* the constant even
   when it can use it as an immediate, and the two forms end up competing over
   one node rather than two.

### The seven new `PATTERNS` rows

`add_r64_r64`, `add_r32_r32`, `sub_r32_r32`, `and_r64_r64`, `or_r64_r64`,
`xor_r64_r64`, `imul_r64_r64`. Each is anchored to a byte literal
`ir_lower::lower_data_node` actually emits, and
`the_new_alu_rows_reproduce_the_ir_lower_byte_literals` checks each against
that literal directly. `Op::Xor` is a new `isel::Op` variant, kept separate
from the `XOR r,r` zeroing idiom (which is a `Op::Mov` row) so a request for
`xor a, b` can never be answered with `a ^ a`.

---

## 7. Wiring

The entry point:

```rust
pub fn select_block(
    graph: &crate::ir::Graph,
    block: &[crate::ir::NodeId],       // ir_schedule::Block::nodes, in order
    terminator: Option<crate::ir::NodeId>,  // ir_schedule::Block::terminator
    opts: &SelectOptions,
) -> BlockSelection
```

Total: never panics, never fails, and every node in `block` comes back covered
exactly once.

A production call site has to:

1. Declare the module (§0).
2. Call `select_block` per block with `SelectOptions::default()`.
3. Emit each `Tile` in order. For `MInst::Generic { node }`, call the existing
   `lower_data_node` / `lower_terminator` path for that node. For everything
   else, allocate registers for the `NodeId` operands and encode through
   `PATTERNS`.
4. **Not** lower any node that appears in a tile's `covered` list without being
   its `root` — those are absorbed.
5. Keep `Rule::TestBranch`'s two-successor phi-copy structure from
   `lower_terminator`: the conditional branch splits the critical edges, and
   the phi copies stay attached to their own edge. Selection does not model
   that and must not be allowed to flatten it.

Until step 1 happens, none of this runs.

---

## 8. The scheduler's ordering rule

In `jit/src/ir_schedule.rs`, under `// ── Memory-effect ordering ──`.

The scheduler decides what order instructions are emitted in, so it is the pass
that can silently break the memory model. The rule it obeys, stated once:

> Let *M* be the sub-sequence of a block's nodes that **order memory** — every
> node whose `MemEffect` is non-inert: it reads, writes, allocates, safepoints,
> or carries a JMM fence half. Then for two orderings of the same node set:
>
> 1. The candidate must be a **permutation** of the baseline.
> 2. Two members of *M* may swap **only** if `Graph::may_reorder` answers
>    `Allowed` for the pair, in baseline order.
> 3. A **barrier** — any member of *M* that is a safepoint or whose `MemOrder`
>    is not `Plain` — may not swap with another member of *M* at all, even when
>    rule 2 would allow it.
> 4. Nodes outside *M* are unconstrained. Dependence order pins them, and that
>    is `topo_sort_block`'s job.

### Why rule 3 is stronger than the model

`Graph::may_reorder` deliberately answers a *memory-side* question only. Its
own documentation lists what it does not cover, and one item is exactly what a
scheduler needs: **implicit exception order**. `Op::Load` faults on a null base
and deopts; `Op::Guard` transfers control and carries no memory edge at all.

Moving a load below an allocation is memory-neutral — the model answers
`Allowed(ReadOnly)`, and `the_barrier_rule_is_stronger_than_the_pairwise_model`
asserts that it does — and is still wrong, because a fault in the moved load
would rebuild an interpreter frame at a bci whose safepoint has already run.

Refusing costs an optimisation the scheduler does not currently attempt (the
priority pass already chains every impure node into a total order). A wrong
answer costs a wrong reconstruction, which is the failure mode that does not
announce itself.

### Where it is enforced

`check_memory_order(graph, baseline, candidate)` is called at the end of
`priority_sort_block`: if the reordering it just computed cannot be justified,
the plain topological order is kept. The side-effect chain above it is
*supposed* to make that vacuous — and "supposed to" is what a validator is for.

It costs one linear scan in the common case: when the memory-ordering nodes
appear in the same relative order in both sequences, which is what every
scheduler in this file currently produces, no pairwise query runs at all.
Over `MEMORY_ORDER_PAIR_BUDGET` inverted pairs the answer is
`OrderRefusal::Budget` — "not verified, therefore not allowed".

**This cannot change the default schedule.** `priority_within_blocks` is
off by default, and even when on the validator can only ever *reject* a
reordering, never cause one.
