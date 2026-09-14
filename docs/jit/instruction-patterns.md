# Declarative x86-64 instruction patterns

A single table that states what each machine instruction *is* — operands,
immediates, addressing, clobbers, flags, fixed registers, constraints, cost and
encoding — plus a matcher over it and tests generated from it.

**Why this exists.** The C2 review
("Implement instruction selection,
scheduling, and register allocation") has a P0 lane:

> Create declarative instruction patterns — describe operation, types,
> immediate forms, addressing forms, clobbers, flags, constraints, cost, and
> encoding for x86-64; generate matcher and encoder tests.
>
> Exit criterion: every selected instruction has verified operand constraints
> and a round-trip disassembly test.

Everything below lives in `jit/src/x64/isel.rs`.

---

## The problem the table solves

`x64.rs` selects and encodes in the same breath. Each emitter is a
hand-written function that decides the REX byte, the opcode, the ModRM/SIB
bytes and the immediate width inline, and the *selection* decisions are `if`
statements buried inside those functions:

| Decision | Where it lives today |
|---|---|
| `MOV r64, 0` → `XOR r32, r32` | `if imm == 0` inside `emit_mov_imm64` |
| `MOV r64, imm` → `imm32` when it fits | `if imm >= i32::MIN && imm <= i32::MAX` inside `emit_mov_imm64` |
| `SUB RSP, n` → `imm8` when it fits | `if (0..=127).contains(&imm)` inside `emit_sub_rsp_imm` |
| Self-move elision | `if dst == src` inside `emit_mov_reg_reg` **and** `emit_mov_r64_r64` |
| "this form emits no SIB, so RSP is illegal here" | a doc comment on `emit_mov_r64_mem_disp32`, unenforced |
| "`TEST BYTE [base]` must keep its `disp8` of zero" | a comment plus an `if` inside `emit_test_mem8_imm8` |

Nothing states, in one place, which bases an encoding can address, which
registers it clobbers, what it does to the flags, or what it costs. A
scheduler needs the flag and clobber facts; a register allocator needs the
fixed-register facts; neither can read them out of `x64.rs`.

## Schema

```rust
pub struct Pattern {
    pub name: &'static str,      // stable identifier, unique in the table
    pub emitter: &'static str,   // the x64.rs emitter this row reproduces
    pub op: Op,                  // Mov, Lea, Add, Cmp, Cmov, Idiv, Movq, …
    pub ty: Ty,                  // Void, I8, I16, I32, I64, F32, F64
    pub dst: OpKind,             // None | Gpr | Xmm | Mem | Imm | Rel32 | Cc | OpByte
    pub src: OpKind,
    pub extra: OpKind,           // third slot: condition code / parametric opcode
    pub enc: Enc,                // prefix, REX.W, REX policy, opcode, ModRM fields
    pub disp: DispPolicy,        // NotApplicable | Smallest | Force32 | AtLeast8
    pub imm: ImmForm,            // None | Imm8 | ImmU8 | Imm32 | Imm64
    pub peephole: Peephole,      // No | ElideWhenDstEqSrc
    pub constraints: &'static [Constraint],
    pub clobbers: &'static [u8], // GPRs written besides the destination
    pub fixed: &'static [FixedReg],  // ShiftCount→CL, Idiv's implicit RDX:RAX
    pub flags: FlagEffect,       // { writes, reads }
    pub mem: MemEffect,          // No | Read | Write | ReadWrite
    pub cost: Cost,              // { bytes (a floor), uops, latency }
    pub mode: Mode,              // Auto (matcher may pick) | Explicit (by name only)
}
```

### Addressing goes through `disp.rs`, not around it

`DispPolicy` has exactly three usable values and they are the three entry
points of `jit/src/x64/disp.rs`:

| `DispPolicy` | Resolves through | Used by |
|---|---|---|
| `Smallest` | `Disp::encode_for_base` | frame slots, `[RSP + n]` stack args, XMM spills |
| `Force32` | `Disp::encode32` | the `*_disp32` family — TLAB accessors, field accessors, the stack-bang probe |
| `AtLeast8` | `Disp::encode_for_base`, then `None` → `Disp8(0)` | `emit_test_mem8_imm8` only |

There is deliberately no fourth value. A row that needed some other narrowing
rule would be a row that re-derives displacement encoding, which is the exact
hazard `disp.rs` exists to remove. The RBP/R13 (no `mod=00` form) and RSP/R12
(SIB required) rules are *asked* of `base_requires_displacement` and
`base_requires_sib`; they are never restated.

`AtLeast8` deserves its own note. Shrinking that row to `mod=00` would be a
legal, one-byte-shorter encoding — and it would move every instruction after it
by one byte, under a branch patcher that has already recorded offsets. The
policy exists so the preservation is a stated property rather than an accident.

### Selection is table lookup, not control flow

`select(&Req)` filters the `Mode::Auto` rows by operation, type and operand
kinds, drops the rows whose constraints the operands violate, encodes the
survivors, and returns the shortest. The shrink chains fall out:

| Request | Candidate rows | Winner |
|---|---|---|
| `MOV r64, 0` | `mov_r64_imm0_xor` (2 B), `mov_r64_imm32` (7 B) | the `XOR` row |
| `MOV r64, 5` | `mov_r64_imm32` (7 B), `mov_r64_imm64` rejected by `ImmNeedsI64` | the `C7` row |
| `MOV r64, 2^40` | `mov_r64_imm64` (10 B) only | the `B8+rd` row |
| `ADD r64, 127` | `add_r64_imm8` (4 B), `add_r64_imm32` (7 B) | the byte form |
| `ADD r64, 128` | `add_r64_imm8` rejected by `ImmFitsI8` | the dword form |

The `emit_mov_imm64` shrink chain is therefore *reproduced by construction*,
not imitated. `Mode::Explicit` exists for rows a matcher must never choose on
its own: `mov_r64_imm64_full` is the non-shrinking ten-byte move an inline-cache
site rewrites in place, and `mov_r64_r64_store_form` is the `0x89` spelling of a
move that already has a `0x8B` spelling of the same length.

A memory request carries `Mem::force_disp32`. That is not a hint: a
fixed-width displacement and a smallest-width displacement are different
requests, and the matcher will not substitute one for the other.

## What is covered

**59 rows**, each naming the `x64.rs` emitter (or the inline byte literal) it
reproduces:

| Family | Rows | Reference |
|---|---:|---|
| GPR moves and widening | 3 | `emit_mov_reg_reg`, `emit_mov_r64_r64`, the `rex_w(); 63 C0` widen |
| GPR immediates | 4 | `emit_xor_reg_self`, `emit_mov_imm32_sx`, `emit_mov_imm64`, `emit_mov_imm64_full` |
| Loads / stores / `LEA` | 14 | `emit_load_local`, `emit_load_caller_arg`, `emit_store_local`, `emit_lea_frame_slot`, `emit_mov_rsp_disp_from_reg`, the whole `*_mem_disp32` family, `emit_mov_dword_mem_disp32_imm32`, `emit_mov_mem8_indexed_imm8`, `emit_stack_bang_load` |
| Integer ALU / compare / test | 15 | `emit_add/and/or_r64_imm8`, `emit_sub_rsp_imm`, `emit_add_rsp_imm`, `emit_sub_r64_r64`, `emit_cmp_r64_r64`, `emit_cmp_r32_r32`, `emit_cmp_r64_mem_disp32`, `emit_test_r64_r64`, `emit_test_r32_r32`, `emit_test_r64_imm32`, `emit_test_mem8_imm8`, the inline `IMUL` |
| Shifts and division | 8 | `emit_shr_r64_imm8`, the inline `D3 /4 /5 /7` shift-by-CL forms, `CQO`/`CDQ`, `IDIV` |
| Conditional move, parametric ALU | 2 | `emit_cmov_cc_reg_reg`, `emit_alu_r32_r32` |
| Stack and control flow | 5 | `emit_push_rbp`, `emit_pop_rbp`, `emit_ret`, `emit_jmp_rel32_patch`, `emit_jcc_rel32_patch` |
| XMM | 8 | `emit_movq_*`, `emit_movsd/movss_xmm_xmm`, `emit_pxor_xmm_self`, `emit_sqrtsd_xmm0` |

`fixed` and `clobbers` are load-bearing, not decorative: the shift rows pin the
count to CL, the `IDIV` rows declare `RDX:RAX` as an implicit source and both as
clobbers, `CQO`/`CDQ` declare `RAX` in and `RDX` out, and `PUSH`/`POP` declare
`RSP`. Those are exactly the facts a scheduler and an allocator have to
consult and cannot get from `x64.rs`.

## What the tests prove

Everything is in `#[cfg(test)] mod tests` in `isel.rs` and driven from the
table, so a new row is automatically swept.

1. **Byte-for-byte equivalence with `x64.rs`.** The reference side is a live
   `Compiler`; the tests call the real private emitters and compare the bytes
   the table produces for the same operands. Nothing reimplements an encoding.
   The sweep runs over
   `[RAX RCX RDX RBX RSP RBP RSI RDI R8 R11 R12 R13 R15]` in both operand
   positions — R8–R15 for the REX bits, RSP/R12 for the SIB rule, RBP/R13 for
   the no-`mod=00` rule — and over displacements that straddle the disp8
   boundary from both sides (`0, 8, 120, 127, 128, 129, 1024, −8, −128`).
2. **Round-trip disassembly.** A structural decoder takes the emitted bytes
   apart again — prefixes, REX, one- versus two-byte opcode, ModRM, SIB,
   displacement width, immediate — using x86's own rules, taking exactly one
   bit of table input (does this opcode carry a ModRM byte, which x86 does not
   encode structurally). Every row is then asserted to give back the register
   numbers, base, index, scale, displacement and immediate it was built from,
   and `render` prints the result as text so a failure names registers instead
   of dumping hex. The decoder itself is spot-checked against hand-verified
   encodings so a decoder bug cannot quietly validate an encoder bug.
3. **Constraint violations are rejected, not encoded.** An RSP base in a
   no-SIB form, RSP as a SIB index, a scale of 3, `TEST r,r` with two different
   registers, a `Jcc` byte handed to `CMOVcc`, register 16, an immediate that
   does not fit its field, a shift count of 64 — each comes back as a typed
   `SelError`.
4. **Displacements round-trip through `disp.rs`.** For every memory row, every
   base and thirteen displacement values: the decoded displacement equals the
   requested one, the `mod` field and the emitted width never disagree, the
   forced-width rows stay four bytes wide even at zero, RBP/R13 never take
   `mod=00`, and RSP/R12 always get a SIB byte. A displacement past disp32 is
   refused by every row rather than truncated.
5. **Immediate-size selection picks the smallest legal form**, with *signed*
   boundaries — 128 is a legal `u8` and not a legal `imm8`, which is the same
   arithmetic mistake `disp.rs` was created to prevent.
6. **Patch sites match.** The `Jcc`/`JMP` rows reproduce not only the bytes but
   the byte offset those emitters hand back to their callers for patching.
7. **Table well-formedness.** Unique names, every row anchored to a reference
   emitter, `REX.W` implies a mandatory REX byte, opcode extensions in `/0../7`,
   `PlusReg` opcodes with room for the register, a displacement policy exactly
   when there is a memory operand, and `cost.bytes` a genuine floor.

## Known divergences to reconcile

Two, both recorded as tests rather than prose.

**1. Negative `SUB`/`ADD RSP, imm`.** `emit_sub_rsp_imm` tests
`(0..=127).contains(&imm)`, so a *negative* immediate takes its `imm32` branch
even though `imm8` expresses it. The table picks the smallest legal form, so
for `-8` it emits four bytes where the emitter emits seven. Same instruction,
same operand value, different width. No call site reaches it — `emit_prologue`,
`emit_epilogue`, `emit_stack_arg_setup`/`_cleanup` and the inline
callee-deopt reserve all pass a non-negative byte count, and
`stack_arg_block_size` cannot return a negative one — so the equivalence test
covers the whole call domain and a separate test pins the out-of-domain
difference.

**2. The generic memory rows address more than the legacy forms.** Three
legacy emitters carry undocumented-by-the-type-system base restrictions that
the generic rows do not:

| Legacy emitter | Its restriction | Generic row |
|---|---|---|
| `emit_mov_r64_mem_disp32` and siblings | base must not be RSP/R12 (no SIB emitted) | emits the SIB byte automatically |
| `emit_mov_r32_mem_disp32` | same | `mov_r32_m_disp32` also covers `emit_stack_bang_load`'s RSP base |
| `emit_mov_mem8_indexed_imm8` | writes a literal `mod=00`, so an RBP/R13 base would encode as "no base" | resolves through `Disp::encode_for_base`, which promotes to `disp8(0)` |

The equivalence tests restrict themselves to the bases each legacy emitter is
actually correct for, and the generic row handles the rest. This is a
migration *benefit*, but it means "the row is equivalent" is a statement about
the legacy emitter's domain, not about all inputs.

Two smaller things worth knowing:

* **`AND`/`OR r64, imm` have no `imm32` row.** `x64.rs` never needed one, so
  the table does not invent one; an immediate past `imm8` is an error rather
  than an untested encoding.
* **`XOR` zeroing writes the flags.** `emit_mov_imm64` substitutes it
  unconditionally and so does `select`, which is faithful — but the row records
  `flags.writes`, so a future caller that must preserve flags across a move has
  something to consult. A flag-preserving `select` variant is the natural
  follow-up.

## Migration order

Nothing in `x64.rs` calls this module yet — deliberately. The order below goes
from rows with no addressing subtlety to rows whose length is load-bearing.

| Wave | Rows | Why here |
|---|---|---|
| 1 | `mov_r64_r64`, `movsxd_r64_r32`, `sub_r64_r64`, `cmp_r64_r64`, `cmp_r32_r32`, `test_r64_r64`, `test_r32_r32`, `imul_r32_r32`, `alu_r32_r32` | Register-direct, fixed length, no displacement, no immediate. A mistake cannot change any instruction's size. |
| 2 | `push_r64`, `pop_r64`, `ret`, `cqo`, `cdq`, `idiv_*`, the shift-by-CL rows | Same, plus they retire inline byte literals that no emitter currently names. |
| 3 | `mov_r64_imm0_xor`, `mov_r64_imm32`, `mov_r64_imm64`, `add/sub/and/or_r64_imm*`, `test_r64_imm32`, `shr_r64_imm8` | Length varies with the immediate. Migrate `emit_mov_imm64` and `emit_mov_imm32_sx` together, since one calls the other. |
| 4 | the `*_disp32` rows and `mov_m32_imm32` | Fixed-width displacement, so length is still constant — but they are on the TLAB/field fast paths, so land them with a benchmark. |
| 5 | `mov_r64_m`, `mov_m_r64`, `lea_r64_m`, `movq_m_xmm`, `movq_xmm_m` | Variable-width displacement. These feed `emit_load_local`'s slot-mirror elision, whose correctness depends on `buf.pos()` being exactly where the previous emission left it — check that first. |
| 6 | `test_m8_imm8`, `mov_m8_index_imm8` | Shape-preserving quirks (`AtLeast8`, the literal `mod=00`). Migrating these changes instruction *lengths* if done carelessly, under a branch patcher. |
| 7 | `jmp_rel32`, `jcc_rel32` | The call sites want the patch offset, not just the bytes; migrate them to `Encoded::imm_offset` in one change so no site computes the offset by hand. |
| — | `cmov_r64_r64`, `movq_xmm_r64`, `movq_r64_xmm`, `movsd/movss_xmm_xmm`, `pxor_xmm_self`, `sqrtsd_xmm_xmm`, `mov_r64_imm64_full`, `mov_r64_r64_store_form` | Migrate opportunistically with whatever wave touches their call sites. |

**Not the same migration as
`docs/feature-designs/jit-machine-level-and-instruction-selection.md`.** That one
builds a machine level *above* this table, so `ir_lower` stops selecting and
encoding in one breath. This one retires `x64.rs`'s hand-written emitters
*onto* the table without changing who calls them. They are independent and can
land in either order; each makes the other cheaper.

One thing to do *before* wave 1, and it is not a code change to `x64.rs`'s
emitters:

1. ~~Declare the module.~~ **Done** — `pub mod isel;` is at
   `x64.rs:136`, in the shape given under [Wiring](#wiring) below. Its 68 tests
   run and pass.
2. Decide whether `select` or `encode_named` is the call-site API. The
   emitters have void signatures and bail through
   `ExecutableBuffer::mark_overflowed`; `Pattern::encode` returns a `Result`.
   The adapter that converts one into the other should be written once, next to
   `mark_overflowed`, not at every call site.

## Wiring

**Landed.** `jit/src/x64.rs:136` carries the line below, immediately
after the `disp` re-export and before the `// SIMD loop analysis and
vectorization` banner. Kept here because the rationale under it is still the
reason the module is `pub mod` and not `mod` + glob:

```rust
pub use disp::{base_requires_displacement, base_requires_sib, disp8_const, Disp, DispOutOfRange};
// ---------------------------------------------------------------------------
// Declarative instruction patterns (instruction selection)
// ---------------------------------------------------------------------------
//
// The pattern table and its matcher. Declared `pub mod` like `disp` so the
// fully-qualified `isel::Pattern` path is available outside this file.
pub mod isel;
```

`isel` is a child of `x64`, so it reaches `Compiler` and the private emitters
for its equivalence tests, and it reaches the `pub(super)` register constants
through `x64`'s glob re-export. It inherits `x64.rs`'s
`deny(clippy::panic, …)` gate: every fallible path in the module returns
`SelError`.
